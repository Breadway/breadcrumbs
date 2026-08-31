//! All NetworkManager access goes over its D-Bus API
//! (`org.freedesktop.NetworkManager` on the system bus) — no `nmcli`
//! subprocess and no terse-output parsing. The D-Bus API returns structured
//! values (SSID as a byte array, signal strength as `u8`, device state as a
//! `u32` enum), so there is nothing like the old `-t` escaping to get wrong.
//!
//! Every function here is fail-silent in the same spirit as the old nmcli
//! calls: a missing bus / unreachable NetworkManager yields `None`/`false`/
//! an empty collection, never a panic. The one exception is
//! [`connect_verbose`], which returns the D-Bus error text so callers can
//! surface *why* a connect failed.
//!
//! The client connects with [`zbus::blocking::Connection::system`], which
//! honors the standard `DBUS_SYSTEM_BUS_ADDRESS` environment variable — the
//! test suite uses that to point at a fake NetworkManager served on a
//! private bus, with no test-only code paths in here.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use crate::config::NetworkDef;

const NM_DEST: &str = "org.freedesktop.NetworkManager";
const NM_PATH: &str = "/org/freedesktop/NetworkManager";
const NM_IFACE: &str = "org.freedesktop.NetworkManager";
const DEV_IFACE: &str = "org.freedesktop.NetworkManager.Device";
const WIFI_IFACE: &str = "org.freedesktop.NetworkManager.Device.Wireless";
const AP_IFACE: &str = "org.freedesktop.NetworkManager.AccessPoint";
const SETTINGS_PATH: &str = "/org/freedesktop/NetworkManager/Settings";
const SETTINGS_IFACE: &str = "org.freedesktop.NetworkManager.Settings";
const CONN_IFACE: &str = "org.freedesktop.NetworkManager.Settings.Connection";
const ACTIVE_IFACE: &str = "org.freedesktop.NetworkManager.Connection.Active";
const IP4_IFACE: &str = "org.freedesktop.NetworkManager.IP4Config";

// NM_DEVICE_TYPE_WIFI
const DEV_TYPE_WIFI: u32 = 2;
// NM_DEVICE_STATE_ACTIVATED
const DEV_STATE_ACTIVATED: u32 = 100;
// NM_802_11_AP_FLAGS_PRIVACY
const AP_FLAG_PRIVACY: u32 = 0x1;
// NM_802_11_AP_SEC_KEY_MGMT_PSK / SAE / 802_1X
const SEC_PSK: u32 = 0x100;
const SEC_802_1X: u32 = 0x200;
const SEC_SAE: u32 = 0x400;
// NM_SETTINGS_ADD_CONNECTION2_FLAG_TO_DISK / UPDATE2_FLAG_TO_DISK
const FLAG_TO_DISK: u32 = 0x1;

/// A fresh connection to the system bus (or wherever `DBUS_SYSTEM_BUS_ADDRESS`
/// points). Created per call — cheap relative to the subprocess the old code
/// spawned — and immune to environment changes between calls.
fn connection() -> Option<Connection> {
    Connection::system().ok()
}

/// zbus error text, for the one call that surfaces it.
fn err_text(e: zbus::Error) -> String {
    format!("D-Bus: {e}")
}

/// Extract a byte array (SSID / CA cert blob) from a settings dict value.
/// zvariant has no `TryFrom<&Value>` for `Vec<u8>` (only for owned `Value`),
/// so we peel the `Array` ourselves.
fn value_bytes(v: &Value) -> Option<Vec<u8>> {
    match v {
        Value::Array(a) => {
            let mut out = Vec::new();
            for item in a.inner() {
                if let Value::U8(b) = item {
                    out.push(*b);
                } else {
                    return None;
                }
            }
            Some(out)
        }
        _ => None,
    }
}

/// Wrap an owned [`Value`] as an [`OwnedValue`] for storage in a settings
/// dict. Only fails for exotic non-ownable values (FDs); our data never hits
/// that, so a panic here would be a genuine bug.
fn ov(v: Value<'_>) -> OwnedValue {
    OwnedValue::try_from(v).expect("settings value is ownable")
}

fn proxy<'a>(conn: &'a Connection, path: &'a str, iface: &'a str) -> Option<Proxy<'a>> {
    Proxy::new(conn, NM_DEST, path, iface).ok()
}

fn nm_proxy(conn: &Connection) -> Option<Proxy<'_>> {
    proxy(conn, NM_PATH, NM_IFACE)
}

/// All realized device object paths (GetDevices), empty on error.
fn devices(conn: &Connection) -> Vec<OwnedObjectPath> {
    nm_proxy(conn)
        .and_then(|p| p.call("GetDevices", &()).ok())
        .unwrap_or_default()
}

/// The object path of the device whose `Interface` property is `iface`.
fn device_path(conn: &Connection, iface: &str) -> Option<OwnedObjectPath> {
    for d in devices(conn) {
        // Scope the proxy so its borrow of `d` ends before we move `d` out.
        let name: String = {
            let dev = proxy(conn, d.as_str(), DEV_IFACE)?;
            dev.get_property("Interface").ok()?
        };
        if name == iface {
            return Some(d);
        }
    }
    None
}

/// All access points visible to a Wi-Fi device, as `(path, ssid, strength,
/// flags, wpa_flags, rsn_flags)` tuples (SSID decoded lossily from bytes).
fn access_points(conn: &Connection, iface: &str) -> Vec<(String, String, u8, u32, u32, u32)> {
    let dev = match device_path(conn, iface) {
        Some(d) => d,
        None => return Vec::new(),
    };
    let wifi = match proxy(conn, dev.as_str(), WIFI_IFACE) {
        Some(p) => p,
        None => return Vec::new(),
    };
    let aps: Vec<OwnedObjectPath> = match wifi.call("GetAllAccessPoints", &()) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    aps.into_iter()
        .filter_map(|ap| {
            let p = proxy(conn, ap.as_str(), AP_IFACE)?;
            let ssid: Vec<u8> = p.get_property("Ssid").ok()?;
            let strength: u8 = p.get_property("Strength").ok()?;
            let flags: u32 = p.get_property("Flags").ok()?;
            let wpa: u32 = p.get_property("WpaFlags").ok()?;
            let rsn: u32 = p.get_property("RsnFlags").ok()?;
            Some((
                ap.as_str().to_string(),
                String::from_utf8_lossy(&ssid).into_owned(),
                strength,
                flags,
                wpa,
                rsn,
            ))
        })
        .collect()
}

pub fn wifi_interface() -> Option<String> {
    wifi_interface_preferred(None)
}

/// Object paths of every Wi-Fi device. The watch loop's monitor subscribes
/// to `Device.StateChanged` on each of these so link churn wakes it early.
pub fn wifi_device_paths() -> Vec<String> {
    let Some(conn) = connection() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for d in devices(&conn) {
        let dev = match proxy(&conn, d.as_str(), DEV_IFACE) {
            Some(p) => p,
            None => continue,
        };
        if matches!(dev.get_property::<u32>("DeviceType"), Ok(t) if t == DEV_TYPE_WIFI) {
            out.push(d.as_str().to_string());
        }
    }
    out
}

/// Whether NetworkManager itself is present and answering on the system bus
/// (or wherever `DBUS_SYSTEM_BUS_ADDRESS` points). Used by `doctor` to
/// report NetworkManager presence without shelling out. A reachable bus with
/// no NM service (or a non-NM service under the name) reports false.
pub fn available() -> bool {
    let Some(conn) = connection() else {
        return false;
    };
    let Some(p) = nm_proxy(&conn) else {
        return false;
    };
    // A well-known-name lookup that actually gets a reply proves the real
    // NetworkManager holds the name.
    p.call::<_, _, Vec<OwnedObjectPath>>("GetDevices", &()).is_ok()
}

/// Find the Wi-Fi interface. When `pref` is `Some`, that exact device is
/// used if present; otherwise (or if the preferred device is missing — e.g.
/// an unplugged USB dongle) the first Wi-Fi device wins.
pub fn wifi_interface_preferred(pref: Option<&str>) -> Option<String> {
    let conn = connection()?;
    let mut wifi: Vec<String> = Vec::new();
    for d in devices(&conn) {
        let dev = proxy(&conn, d.as_str(), DEV_IFACE)?;
        let devtype: u32 = dev.get_property("DeviceType").ok()?;
        if devtype == DEV_TYPE_WIFI {
            let name: String = dev.get_property("Interface").ok()?;
            wifi.push(name);
        }
    }
    if let Some(p) = pref {
        if wifi.iter().any(|d| d == p) {
            return Some(p.to_string());
        }
    }
    wifi.into_iter().next()
}

pub fn radio_on() {
    let Some(conn) = connection() else { return };
    let Some(p) = nm_proxy(&conn) else { return };
    let _ = p.set_property("WirelessEnabled", true);
}

pub fn rescan(iface: &str, ssids: &[String]) {
    let Some(conn) = connection() else { return };
    let Some(dev) = device_path(&conn, iface) else { return };
    let Some(wifi) = proxy(&conn, dev.as_str(), WIFI_IFACE) else {
        return;
    };
    let mut options: HashMap<String, Value> = HashMap::new();
    if !ssids.is_empty() {
        let ssid_bytes: Vec<Vec<u8>> = ssids.iter().map(|s| s.as_bytes().to_vec()).collect();
        options.insert("ssids".into(), Value::from(ssid_bytes));
    }
    let _ = wifi.call_noreply("RequestScan", &(options,));
}

pub fn visible_ssids(iface: &str) -> HashSet<String> {
    let Some(conn) = connection() else {
        return HashSet::new();
    };
    access_points(&conn, iface)
        .into_iter()
        .map(|(_, ssid, _, _, _, _)| ssid)
        .filter(|s| !s.is_empty())
        .collect()
}

/// Visible SSIDs with their signal strength (0–100), one entry per SSID
/// (strongest BSSID wins). Used for signal-aware network selection and
/// scored detection.
pub fn visible_signals(iface: &str) -> HashMap<String, i32> {
    let Some(conn) = connection() else {
        return HashMap::new();
    };
    let mut m: HashMap<String, i32> = HashMap::new();
    for (_, ssid, strength, _, _, _) in access_points(&conn, iface) {
        if ssid.is_empty() {
            continue;
        }
        let sig = strength as i32;
        m.entry(ssid)
            .and_modify(|e| {
                if sig > *e {
                    *e = sig;
                }
            })
            .or_insert(sig);
    }
    m
}

#[derive(Debug, Clone)]
pub struct ScanEntry {
    pub ssid: String,
    pub signal: String,
    pub security: String,
}

/// Derive an nmcli-style SECURITY column value from the 802.11 flag sets.
/// Mirrors what nmcli shows for an AP: `--` for open, `WPA1 WPA2` / `WPA2` /
/// `WPA3` / `802.1X` / `WEP` for secured networks.
fn security_string(flags: u32, wpa: u32, rsn: u32) -> String {
    if flags & AP_FLAG_PRIVACY == 0 && wpa == 0 && rsn == 0 {
        return "--".into();
    }
    if rsn & SEC_SAE != 0 {
        return "WPA3".into();
    }
    let mut parts: Vec<&str> = Vec::new();
    if wpa & SEC_PSK != 0 {
        parts.push("WPA1");
    }
    if rsn & SEC_PSK != 0 {
        parts.push("WPA2");
    }
    if !parts.is_empty() {
        return parts.join(" ");
    }
    if wpa & SEC_802_1X != 0 || rsn & SEC_802_1X != 0 {
        return "802.1X".into();
    }
    "WEP".into()
}

pub fn scan_list(iface: &str) -> Vec<ScanEntry> {
    let Some(conn) = connection() else {
        return Vec::new();
    };
    let mut out: Vec<ScanEntry> = Vec::new();
    for (_, ssid, strength, flags, wpa, rsn) in access_points(&conn, iface) {
        if ssid.is_empty() {
            // Hidden networks have no SSID in the scan; they're not
            // selectable here anyway (see `cmd_scan`), so skip them.
            continue;
        }
        let signal = strength.to_string();
        let security = security_string(flags, wpa, rsn);
        // One entry per SSID: dedup keeping the *strongest* signal, so a
        // network broadcast by several APs shows once (at its best signal).
        match out.iter_mut().find(|e| e.ssid == ssid) {
            Some(existing) => {
                if signal_rank(&signal) > signal_rank(&existing.signal) {
                    existing.signal = signal;
                    existing.security = security;
                }
            }
            None => out.push(ScanEntry {
                ssid,
                signal,
                security,
            }),
        }
    }
    out
}

/// Numeric rank of a signal string ("72" or "72 %" — the fake NM can emit
/// either), for the strongest-wins dedup.
fn signal_rank(s: &str) -> i32 {
    s.trim()
        .trim_end_matches('%')
        .trim()
        .parse::<i32>()
        .unwrap_or(-100)
}

pub fn active_ssid(iface: &str) -> Option<String> {
    let conn = connection()?;
    let dev = device_path(&conn, iface)?;
    let wifi = proxy(&conn, dev.as_str(), WIFI_IFACE)?;
    let ap: OwnedObjectPath = wifi.get_property("ActiveAccessPoint").ok()?;
    if ap.as_str() == "/" {
        return None;
    }
    let p = proxy(&conn, ap.as_str(), AP_IFACE)?;
    let ssid: Vec<u8> = p.get_property("Ssid").ok()?;
    let s = String::from_utf8_lossy(&ssid).into_owned();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// The device's current IPv4 address (dotted quad), via `Device.Ip4Config`
/// → `IP4Config.Addresses` (first address). Replaces the old
/// `nmcli -g IP4.ADDRESS device show` call.
pub fn ipv4_address(iface: &str) -> Option<String> {
    let conn = connection()?;
    let dev = device_path(&conn, iface)?;
    let dev_proxy = proxy(&conn, dev.as_str(), DEV_IFACE)?;
    let cfg_path: OwnedObjectPath = dev_proxy.get_property("Ip4Config").ok()?;
    if cfg_path.as_str() == "/" {
        return None;
    }
    let ip4 = proxy(&conn, cfg_path.as_str(), IP4_IFACE)?;
    // a(ayu): each entry is (address, prefix, gateway), address in network
    // byte order as a host-order u32.
    let addresses: Vec<(u32, u32, u32)> = ip4.get_property("Addresses").ok()?;
    let (ip, _, _) = addresses.into_iter().next()?;
    Some(format!(
        "{}.{}.{}.{}",
        (ip >> 24) & 0xff,
        (ip >> 16) & 0xff,
        (ip >> 8) & 0xff,
        ip & 0xff
    ))
}

pub fn device_connected(iface: &str) -> bool {
    let Some(conn) = connection() else {
        return false;
    };
    let Some(dev) = device_path(&conn, iface) else {
        return false;
    };
    let Some(p) = proxy(&conn, dev.as_str(), DEV_IFACE) else {
        return false;
    };
    matches!(p.get_property::<u32>("State"), Ok(s) if s == DEV_STATE_ACTIVATED)
}

/// The object path of the *active* connection's settings profile (via
/// Device.ActiveConnection → Connection.Active.Connection), if the device is
/// up. Used to locate the profile to pin DNS onto.
fn active_connection_path(conn: &Connection, iface: &str) -> Option<OwnedObjectPath> {
    let dev = device_path(conn, iface)?;
    let dev_proxy = proxy(conn, dev.as_str(), DEV_IFACE)?;
    let active: OwnedObjectPath = dev_proxy.get_property("ActiveConnection").ok()?;
    if active.as_str() == "/" {
        return None;
    }
    let ac = proxy(conn, active.as_str(), ACTIVE_IFACE)?;
    let conn_path: OwnedObjectPath = ac.get_property("Connection").ok()?;
    Some(conn_path)
}

/// Full settings dict of a saved connection profile.
type SettingsMap = HashMap<String, HashMap<String, OwnedValue>>;

fn get_settings(conn: &Connection, conn_path: &str) -> Option<SettingsMap> {
    let p = proxy(conn, conn_path, CONN_IFACE)?;
    p.call("GetSettings", &()).ok()
}

/// Return the path of the first saved NM connection profile whose name is
/// either exactly `ssid` or `ssid N` (NM's numbered-duplicate convention), or
/// whose 802-11-wireless SSID equals `ssid`. Returns `None` if no such
/// profile exists.
fn first_profile_for_ssid(conn: &Connection, ssid: &str) -> Option<OwnedObjectPath> {
    let settings = proxy(conn, SETTINGS_PATH, SETTINGS_IFACE)?;
    let conns: Vec<OwnedObjectPath> = settings.call("ListConnections", &()).ok()?;
    let mut fallback: Option<OwnedObjectPath> = None;
    for c in conns {
        let Some(s) = get_settings(conn, c.as_str()) else {
            continue;
        };
        let conn_id = s
            .get("connection")
            .and_then(|m| m.get("id"))
            .and_then(|v| v.downcast_ref::<String>().ok());
        let conn_ssid = s
            .get("802-11-wireless")
            .and_then(|m| m.get("ssid"))
            .and_then(|v| value_bytes(v))
            .map(|b| String::from_utf8_lossy(&b).into_owned());
        match &conn_id {
            Some(id) if id == ssid => return Some(c),
            _ => {}
        }
        if conn_ssid.as_deref() == Some(ssid) {
            return Some(c);
        }
        if fallback.is_none() {
            if let Some(id) = &conn_id {
                if let Some(suffix) = id.strip_prefix(ssid) {
                    let s = suffix.trim();
                    if !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()) {
                        fallback = Some(c);
                    }
                }
            }
        }
    }
    fallback
}

/// Build the `a{sa{sv}}` settings dict for a new Wi-Fi connection.
fn wifi_settings(net: &NetworkDef, uuid: &str) -> SettingsMap {
    let mut settings: SettingsMap = HashMap::new();

    let mut conn: HashMap<String, OwnedValue> = HashMap::new();
    conn.insert("id".into(), ov(Value::from(net.ssid.clone())));
    conn.insert("type".into(), ov(Value::from("802-11-wireless")));
    conn.insert("uuid".into(), ov(Value::from(uuid.to_string())));
    settings.insert("connection".into(), conn);

    let mut wifi: HashMap<String, OwnedValue> = HashMap::new();
    wifi.insert("ssid".into(), ov(Value::from(net.ssid.as_bytes().to_vec())));
    wifi.insert("mode".into(), ov(Value::from("infrastructure")));
    wifi.insert("hidden".into(), ov(Value::from(net.hidden)));
    settings.insert("802-11-wireless".into(), wifi);

    if net.eap.is_some() {
        let mut sec: HashMap<String, OwnedValue> = HashMap::new();
        sec.insert("key-mgmt".into(), ov(Value::from("wpa-eap")));
        settings.insert("802-11-wireless-security".into(), sec);

        let mut x1: HashMap<String, OwnedValue> = HashMap::new();
        if let Some(eap) = &net.eap {
            x1.insert("eap".into(), ov(Value::from(vec![eap.clone()])));
        }
        if let Some(id) = &net.identity {
            x1.insert("identity".into(), ov(Value::from(id.clone())));
        }
        if let Some(ca) = &net.ca_cert {
            // NM stores `802-1x.ca-cert` as a GBytes (`ay`). For a filesystem
            // path we send the conventional `file://` URI — the same form
            // `nmcli` persists when given a path — not a bare string.
            x1.insert(
                "ca-cert".into(),
                ov(Value::from(format!("file://{ca}").into_bytes())),
            );
        }
        if let Some(pw) = &net.password {
            x1.insert("password".into(), ov(Value::from(pw.clone())));
        }
        settings.insert("802-1x".into(), x1);
    } else if let Some(pw) = &net.password {
        let mut sec: HashMap<String, OwnedValue> = HashMap::new();
        sec.insert("key-mgmt".into(), ov(Value::from("wpa-psk")));
        sec.insert("psk".into(), ov(Value::from(pw.clone())));
        settings.insert("802-11-wireless-security".into(), sec);
    }

    let mut ipv4: HashMap<String, OwnedValue> = HashMap::new();
    ipv4.insert("method".into(), ov(Value::from("auto")));
    settings.insert("ipv4".into(), ipv4);
    let mut ipv6: HashMap<String, OwnedValue> = HashMap::new();
    ipv6.insert("method".into(), ov(Value::from("auto")));
    settings.insert("ipv6".into(), ipv6);

    settings
}

/// Update a saved profile's settings in place for `net`: PSK (or 802.1x
/// properties for enterprise networks) and the hidden flag. Returns the
/// updated dict.
fn updated_settings_for(net: &NetworkDef, s: &mut SettingsMap) {
    let sec = s.entry("802-11-wireless-security".to_string()).or_default();
    if net.eap.is_some() {
        sec.insert("key-mgmt".into(), ov(Value::from("wpa-eap")));
    } else if net.password.is_some() {
        sec.insert("key-mgmt".into(), ov(Value::from("wpa-psk")));
    }
    if let Some(pw) = &net.password {
        if net.eap.is_some() {
            let x1 = s.entry("802-1x".to_string()).or_default();
            x1.insert("password".into(), ov(Value::from(pw.clone())));
        } else {
            sec.insert("psk".into(), ov(Value::from(pw.clone())));
        }
    }
    let wifi = s.entry("802-11-wireless".to_string()).or_default();
    wifi.insert("hidden".into(), ov(Value::from(net.hidden)));
}

/// RFC 4122 v4 UUID from `/dev/urandom` — used for the `connection.uuid` of
/// newly created profiles. (NetworkManager would generate one itself if
/// omitted, but being explicit matches `nmcli` and keeps the fake NM simple.)
fn new_uuid() -> String {
    let mut bytes = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = f.read_exact(&mut bytes);
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40; // version 4
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // variant 10
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7], bytes[8],
        bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
    )
}

/// Wait up to `wait` seconds for the device to reach ACTIVATED. Polls the
/// device State property, like `nmcli --wait` blocks for activation.
fn wait_activated(conn: &Connection, dev: &OwnedObjectPath, wait: u32) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(wait.max(1) as u64);
    loop {
        match proxy(conn, dev.as_str(), DEV_IFACE) {
            Some(p) => match p.get_property::<u32>("State") {
                Ok(s) if s == DEV_STATE_ACTIVATED => return Ok(()),
                Ok(_) => {}
                Err(e) => return Err(err_text(e)),
            },
            None => return Err("device disappeared while waiting for activation".into()),
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for device {dev} to activate after {wait}s"
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Pin DNS onto the active connection of `iface`: update `ipv4.ignore-auto-dns`
/// and `ipv4.dns`, persist via Update2, then reapply on the device.
fn enforce_dns(conn: &Connection, iface: &str, dns: &str) -> bool {
    if dns.trim().is_empty() {
        return true;
    }
    let Some(active) = active_connection_path(conn, iface) else {
        return false;
    };
    let Some(mut s) = get_settings(conn, active.as_str()) else {
        return false;
    };
    let ipv4 = s.entry("ipv4".to_string()).or_default();
    ipv4.insert("ignore-auto-dns".into(), ov(Value::from(true)));
    ipv4.insert("dns".into(), ov(Value::from(vec![dns.to_string()])));

    // Persist the change on the *profile* object via `Update2` — calling
    // `Settings.AddConnection2` with the profile's own UUID would fail on
    // real NetworkManager (NM_SETTINGS_ERROR_UUID_EXISTS; duplicates are
    // rejected, not upserted).
    let conn_proxy = match proxy(conn, active.as_str(), CONN_IFACE) {
        Some(p) => p,
        None => return false,
    };
    let _: Result<HashMap<String, OwnedValue>, _> = conn_proxy.call(
        "Update2",
        &(&s, FLAG_TO_DISK, &HashMap::<String, Value>::new()),
    );

    // Reapply the updated settings on the device so DNS takes effect without
    // bouncing the link.
    let Some(dev) = device_path(conn, iface) else {
        return false;
    };
    let Some(dev_proxy) = proxy(conn, dev.as_str(), DEV_IFACE) else {
        return false;
    };
    let _: Result<(), _> = dev_proxy.call("Reapply", &(&s, 0u64, 0u32));
    true
}

/// Connect to a network and pin DNS. Returns true only if associated.
pub fn connect(iface: &str, net: &NetworkDef, wait: u32, dns: &str) -> bool {
    connect_verbose(iface, net, wait, dns).is_ok()
}

/// Connect to a network and pin DNS. Returns the D-Bus error on failure.
///
/// Reuses an existing saved profile for the SSID when one exists (updating its
/// PSK) so that repeated connections do not accumulate numbered duplicates in
/// NetworkManager. Falls back to creating a new connection
/// (`AddAndActivateConnection2`, which also activates it) only when no saved
/// profile is found.
///
/// `net.password` is only sent when `Some`: on the reuse path, `None` means
/// \"leave the saved secret alone\" (either NetworkManager already durably
/// owns it, or the network is open); on the create path it means \"no secret
/// section\", which is also how a genuinely open (no-security) SSID is
/// connected. See the field doc on [`NetworkDef::password`] for how a local
/// secret transitions to `None` after its first successful use.
///
/// The secret (when sent) travels inside the D-Bus `Update2`/`AddAndActivate
/// Connection2` settings payload — never on a process argv line — so it is
/// not readable by other local users via `/proc/<pid>/cmdline` the way the
/// old `nmcli ... psk <pw>` invocation was.
pub fn connect_verbose(iface: &str, net: &NetworkDef, wait: u32, dns: &str) -> Result<(), String> {
    let conn = connection().ok_or_else(|| "cannot connect to the D-Bus system bus".to_string())?;
    let dev = device_path(&conn, iface)
        .ok_or_else(|| format!("no NetworkManager device named '{iface}'"))?;

    if let Some(profile) = first_profile_for_ssid(&conn, &net.ssid) {
        // Update the saved credentials and, for hidden networks, ensure the
        // hidden flag is set. PSK vs 802.1x (enterprise) profiles get their
        // own property sets.
        if net.password.is_some() || net.hidden {
            let mut s = get_settings(&conn, profile.as_str())
                .ok_or_else(|| "failed to read saved profile settings".to_string())?;
            updated_settings_for(net, &mut s);
            // Update the existing profile in place via `Settings.Connection
            // .Update2`. `Settings.AddConnection2` with the profile's own
            // UUID would be rejected by real NetworkManager
            // (NM_SETTINGS_ERROR_UUID_EXISTS — duplicates are not upserted).
            let conn_proxy = proxy(&conn, profile.as_str(), CONN_IFACE)
                .ok_or_else(|| "NetworkManager Settings.Connection unavailable".to_string())?;
            let _: HashMap<String, OwnedValue> = conn_proxy
                .call("Update2", &(&s, FLAG_TO_DISK, &HashMap::<String, Value>::new()))
                .map_err(err_text)?;
        }
        let nm = nm_proxy(&conn).ok_or_else(|| "NetworkManager unavailable".to_string())?;
        let specific = zbus::zvariant::ObjectPath::try_from("/").expect("root object path");
        let _: OwnedObjectPath = nm
            .call("ActivateConnection", &(&profile, &dev, &specific))
            .map_err(err_text)?;
        wait_activated(&conn, &dev, wait)?;
        enforce_dns(&conn, iface, dns);
        return Ok(());
    }

    // No saved profile — create one (and activate it in one call).
    let settings = wifi_settings(net, &new_uuid());
    let nm = nm_proxy(&conn).ok_or_else(|| "NetworkManager unavailable".to_string())?;
    let options: HashMap<String, Value> =
        HashMap::from([("persist".to_string(), Value::from("disk"))]);
    let specific = zbus::zvariant::ObjectPath::try_from("/").expect("root object path");
    let (_path, _active): (OwnedObjectPath, OwnedObjectPath) = nm
        .call(
            "AddAndActivateConnection2",
            &(&settings, &dev, &specific, &options),
        )
        .map_err(err_text)?;
    wait_activated(&conn, &dev, wait)?;
    enforce_dns(&conn, iface, dns);
    Ok(())
}

/// List all wireless connection profiles as `(name, ssid)` pairs, using the
/// profile's `802-11-wireless.ssid` setting when present (falling back to
/// the profile name). Used by `breadcrumbs prune`.
pub fn wireless_profiles() -> Vec<(String, String)> {
    let Some(conn) = connection() else {
        return Vec::new();
    };
    let Some(settings) = proxy(&conn, SETTINGS_PATH, SETTINGS_IFACE) else {
        return Vec::new();
    };
    let conns: Vec<OwnedObjectPath> = match settings.call("ListConnections", &()) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for c in conns {
        let Some(s) = get_settings(&conn, c.as_str()) else {
            continue;
        };
        let typ = s
            .get("connection")
            .and_then(|m| m.get("type"))
            .and_then(|v| v.downcast_ref::<String>().ok());
        if typ.as_deref() != Some("802-11-wireless") {
            continue;
        }
        let name = s
            .get("connection")
            .and_then(|m| m.get("id"))
            .and_then(|v| v.downcast_ref::<String>().ok())
            .unwrap_or_default();
        let conn_ssid = s
            .get("802-11-wireless")
            .and_then(|m| m.get("ssid"))
            .and_then(|v| value_bytes(v))
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .filter(|s| !s.is_empty());
        out.push((name.clone(), conn_ssid.unwrap_or(name)));
    }
    out
}

/// Delete every saved connection profile whose name or 802-11-wireless SSID
/// matches `ssid` (used by `breadcrumbs forget` to purge stale entries).
pub fn delete_connections_for_ssid(ssid: &str) -> bool {
    let Some(conn) = connection() else {
        return false;
    };
    let Some(settings) = proxy(&conn, SETTINGS_PATH, SETTINGS_IFACE) else {
        return false;
    };
    let conns: Vec<OwnedObjectPath> = match settings.call("ListConnections", &()) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let mut removed = false;
    for c in conns {
        let Some(s) = get_settings(&conn, c.as_str()) else {
            continue;
        };
        let name = s
            .get("connection")
            .and_then(|m| m.get("id"))
            .and_then(|v| v.downcast_ref::<String>().ok())
            .unwrap_or_default();
        let conn_ssid = s
            .get("802-11-wireless")
            .and_then(|m| m.get("ssid"))
            .and_then(|v| value_bytes(v))
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        if name == ssid || conn_ssid == ssid {
            if let Some(p) = proxy(&conn, c.as_str(), CONN_IFACE) {
                if p.call::<_, _, ()>("Delete", &()).is_ok() {
                    removed = true;
                }
            }
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn security_string_derivation() {
        // Open AP: no privacy bit, no WPA/RSN.
        assert_eq!(security_string(0, 0, 0), "--");
        // WPA2-PSK (RSN PSK set).
        assert_eq!(security_string(AP_FLAG_PRIVACY, 0, SEC_PSK), "WPA2");
        // WPA1+WPA2 (both flag sets carry PSK).
        assert_eq!(security_string(AP_FLAG_PRIVACY, SEC_PSK, SEC_PSK), "WPA1 WPA2");
        // WPA3 (SAE).
        assert_eq!(security_string(AP_FLAG_PRIVACY, 0, SEC_SAE), "WPA3");
        // Enterprise (802.1x key management).
        assert_eq!(
            security_string(AP_FLAG_PRIVACY, SEC_802_1X, SEC_802_1X),
            "802.1X"
        );
        // WEP: privacy bit but no WPA/RSN.
        assert_eq!(security_string(AP_FLAG_PRIVACY, 0, 0), "WEP");
    }

    #[test]
    fn signal_rank_handles_percent_suffix() {
        assert_eq!(signal_rank("90 %"), 90);
        assert_eq!(signal_rank("80"), 80);
        assert_eq!(signal_rank("garbage"), -100);
    }

    #[test]
    fn uuid_v4_is_shape_valid() {
        let u = new_uuid();
        let bytes = u.as_bytes();
        assert_eq!(bytes.len(), 36);
        assert_eq!(bytes[8], b'-');
        assert_eq!(bytes[13], b'-');
        assert_eq!(bytes[18], b'-');
        assert_eq!(bytes[23], b'-');
        // Version nibble is 4.
        assert_eq!(bytes[14], b'4');
    }
}
