//! All NetworkManager access goes over its D-Bus API
//! (`org.freedesktop.NetworkManager` on the system bus): no `nmcli`
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
//! honors the standard `DBUS_SYSTEM_BUS_ADDRESS` environment variable: the
//! test suite uses that to point at a fake NetworkManager served on a
//! private bus, with no test-only code paths in here.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use crate::config::NetworkDef;
use crate::notify::log;

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
const PROPS_IFACE: &str = "org.freedesktop.DBus.Properties";

// NM_DEVICE_TYPE_WIFI
const DEV_TYPE_WIFI: u32 = 2;
// NM_DEVICE_STATE_ACTIVATED
const DEV_STATE_ACTIVATED: u32 = 100;
// NM_ACTIVE_CONNECTION_STATE_ACTIVATED / DEACTIVATING / DEACTIVATED
const AC_STATE_ACTIVATED: u32 = 2;
const AC_STATE_DEACTIVATING: u32 = 3;
const AC_STATE_DEACTIVATED: u32 = 4;
// NM_802_11_AP_FLAGS_PRIVACY
const AP_FLAG_PRIVACY: u32 = 0x1;
// NM_802_11_AP_SEC_KEY_MGMT_PSK / SAE / 802_1X
const SEC_PSK: u32 = 0x100;
const SEC_802_1X: u32 = 0x200;
const SEC_SAE: u32 = 0x400;
// NM_SETTINGS_ADD_CONNECTION2_FLAG_TO_DISK / UPDATE2_FLAG_TO_DISK
const FLAG_TO_DISK: u32 = 0x1;

/// `user.data` key marking a profile whose DNS breadcrumbs pinned, so a
/// later `dns = ""` can undo exactly its own pin and nothing the user set.
const DNS_PIN_MARKER: &str = "dev.breadway.breadcrumbs.dns-pinned";

/// A fresh connection to the system bus (or wherever `DBUS_SYSTEM_BUS_ADDRESS`
/// points). Created per call, cheap relative to the subprocess the old code
/// spawned, and immune to environment changes between calls.
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

fn value_string(v: &Value) -> Option<String> {
    match v {
        Value::Str(s) => Some(s.as_str().to_string()),
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

/// Every property of `iface` on `path` in one round trip (`GetAll`), rather
/// than one `Get` per property.
fn get_all(conn: &Connection, path: &str, iface: &str) -> Option<HashMap<String, OwnedValue>> {
    let p = proxy(conn, path, PROPS_IFACE)?;
    p.call("GetAll", &(iface,)).ok()
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
        // A failure here is per-device (e.g. a mid-scan D-Bus hiccup on an
        // unrelated device), skip just this one and keep scanning, rather
        // than aborting the whole lookup via `?`.
        let name: Option<String> = {
            let Some(dev) = proxy(conn, d.as_str(), DEV_IFACE) else {
                continue;
            };
            dev.get_property("Interface").ok()
        };
        if name.as_deref() == Some(iface) {
            return Some(d);
        }
    }
    None
}

/// One visible access point.
#[derive(Debug, Clone)]
struct ApInfo {
    ssid: String,
    strength: u8,
    flags: u32,
    wpa: u32,
    rsn: u32,
}

/// All access points visible to a Wi-Fi device (SSID decoded lossily from
/// bytes). One `GetAll` per AP instead of five property reads.
fn access_points(conn: &Connection, iface: &str) -> Vec<ApInfo> {
    let Some(dev) = device_path(conn, iface) else {
        return Vec::new();
    };
    let Some(wifi) = proxy(conn, dev.as_str(), WIFI_IFACE) else {
        return Vec::new();
    };
    let aps: Vec<OwnedObjectPath> = match wifi.call("GetAllAccessPoints", &()) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    aps.into_iter()
        .filter_map(|ap| {
            let props = get_all(conn, ap.as_str(), AP_IFACE)?;
            let ssid = value_bytes(props.get("Ssid")?)?;
            let strength: u8 = props.get("Strength")?.downcast_ref().ok()?;
            let flags: u32 = props.get("Flags")?.downcast_ref().ok()?;
            let wpa: u32 = props.get("WpaFlags")?.downcast_ref().ok()?;
            let rsn: u32 = props.get("RsnFlags")?.downcast_ref().ok()?;
            Some(ApInfo {
                ssid: String::from_utf8_lossy(&ssid).into_owned(),
                strength,
                flags,
                wpa,
                rsn,
            })
        })
        .collect()
}

pub fn wifi_interface() -> Option<String> {
    wifi_interface_preferred(None)
}

/// Whether the device at `path` is a Wi-Fi device. Used by the watch loop
/// to subscribe to devices that appear after it started (USB hotplug).
pub fn is_wifi_device(path: &str) -> bool {
    let Some(conn) = connection() else {
        return false;
    };
    let Some(dev) = proxy(&conn, path, DEV_IFACE) else {
        return false;
    };
    matches!(dev.get_property::<u32>("DeviceType"), Ok(t) if t == DEV_TYPE_WIFI)
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
    p.call::<_, _, Vec<OwnedObjectPath>>("GetDevices", &())
        .is_ok()
}

/// Find the Wi-Fi interface. When `pref` is `Some`, that exact device is
/// used if present; otherwise (or if the preferred device is missing, e.g.
/// an unplugged USB dongle) the first Wi-Fi device wins.
pub fn wifi_interface_preferred(pref: Option<&str>) -> Option<String> {
    let conn = connection()?;
    let mut wifi: Vec<String> = Vec::new();
    for d in devices(&conn) {
        // As in `device_path`: skip just this device on a per-device D-Bus
        // failure instead of aborting the whole scan with `?`.
        let Some(dev) = proxy(&conn, d.as_str(), DEV_IFACE) else {
            continue;
        };
        let Ok(devtype) = dev.get_property::<u32>("DeviceType") else {
            continue;
        };
        if devtype == DEV_TYPE_WIFI {
            let Ok(name) = dev.get_property::<String>("Interface") else {
                continue;
            };
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

/// Ask NetworkManager for a fresh scan. `ssids` are sent as directed probe
/// requests, which broadcast those names over the air: callers should only
/// pass hidden networks (a visible network is found by a passive scan).
pub fn rescan(iface: &str, ssids: &[String]) {
    let Some(conn) = connection() else { return };
    let Some(dev) = device_path(&conn, iface) else {
        return;
    };
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
        .map(|ap| ap.ssid)
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
    for ap in access_points(&conn, iface) {
        if ap.ssid.is_empty() {
            continue;
        }
        let sig = i32::from(ap.strength);
        m.entry(ap.ssid)
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
    let mut out: Vec<(ScanEntry, u8)> = Vec::new();
    for ap in access_points(&conn, iface) {
        if ap.ssid.is_empty() {
            // Hidden networks have no SSID in the scan; they're not
            // selectable here anyway (see `cmd_scan`), so skip them.
            continue;
        }
        let security = security_string(ap.flags, ap.wpa, ap.rsn);
        // One entry per SSID: dedup keeping the *strongest* signal, so a
        // network broadcast by several APs shows once (at its best signal).
        match out.iter_mut().find(|(e, _)| e.ssid == ap.ssid) {
            Some((existing, strength)) => {
                if ap.strength > *strength {
                    *strength = ap.strength;
                    existing.signal = ap.strength.to_string();
                    existing.security = security;
                }
            }
            None => out.push((
                ScanEntry {
                    ssid: ap.ssid,
                    signal: ap.strength.to_string(),
                    security,
                },
                ap.strength,
            )),
        }
    }
    out.into_iter().map(|(e, _)| e).collect()
}

fn strongest_ap(conn: &Connection, iface: &str, ssid: &str) -> Option<ApInfo> {
    access_points(conn, iface)
        .into_iter()
        .filter(|ap| ap.ssid == ssid)
        .max_by_key(|ap| ap.strength)
}

fn active_ssid_on(conn: &Connection, dev: &OwnedObjectPath) -> Option<String> {
    let wifi = proxy(conn, dev.as_str(), WIFI_IFACE)?;
    let ap: OwnedObjectPath = wifi.get_property("ActiveAccessPoint").ok()?;
    if ap.as_str() == "/" {
        return None;
    }
    let p = proxy(conn, ap.as_str(), AP_IFACE)?;
    let ssid: Vec<u8> = p.get_property("Ssid").ok()?;
    let s = String::from_utf8_lossy(&ssid).into_owned();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

pub fn active_ssid(iface: &str) -> Option<String> {
    let conn = connection()?;
    let dev = device_path(&conn, iface)?;
    active_ssid_on(&conn, &dev)
}

/// The device's current IPv4 address (dotted quad), via `Device.Ip4Config`
/// → `IP4Config.AddressData` (first address). `AddressData` is
/// `aa{sv}` with the address as a string; the deprecated `Addresses`
/// property is `aau` in network byte order and is not used.
pub fn ipv4_address(iface: &str) -> Option<String> {
    let conn = connection()?;
    let dev = device_path(&conn, iface)?;
    let dev_proxy = proxy(&conn, dev.as_str(), DEV_IFACE)?;
    let cfg_path: OwnedObjectPath = dev_proxy.get_property("Ip4Config").ok()?;
    if cfg_path.as_str() == "/" {
        return None;
    }
    let ip4 = proxy(&conn, cfg_path.as_str(), IP4_IFACE)?;
    let data: Vec<HashMap<String, OwnedValue>> = ip4.get_property("AddressData").ok()?;
    data.iter()
        .find_map(|entry| entry.get("address").and_then(|v| value_string(v)))
}

fn device_activated(conn: &Connection, dev: &OwnedObjectPath) -> bool {
    proxy(conn, dev.as_str(), DEV_IFACE)
        .map(|p| matches!(p.get_property::<u32>("State"), Ok(s) if s == DEV_STATE_ACTIVATED))
        .unwrap_or(false)
}

pub fn device_connected(iface: &str) -> bool {
    let Some(conn) = connection() else {
        return false;
    };
    let Some(dev) = device_path(&conn, iface) else {
        return false;
    };
    device_activated(&conn, &dev)
}

/// The object path of the *active* connection's settings profile (via
/// Device.ActiveConnection → Connection.Active.Connection), if the device is
/// up. Used to locate the profile to pin DNS onto.
fn active_connection_path(conn: &Connection, dev: &OwnedObjectPath) -> Option<OwnedObjectPath> {
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

fn setting_str(s: &SettingsMap, section: &str, key: &str) -> Option<String> {
    s.get(section)
        .and_then(|m| m.get(key))
        .and_then(|v| value_string(v))
}

/// The saved profile's `802-11-wireless.hidden`. NetworkManager omits the key
/// when it is false, so a missing key reads as `false`.
fn saved_hidden(s: &SettingsMap) -> bool {
    s.get("802-11-wireless")
        .and_then(|m| m.get("hidden"))
        .and_then(|v| v.downcast_ref::<bool>().ok())
        .unwrap_or(false)
}

/// Whether connecting needs to rewrite the saved profile. Credentials always
/// do; the hidden flag does when it differs from what is saved, in either
/// direction, so un-hiding a network reaches the profile instead of leaving
/// `hidden=true` behind forever.
fn profile_needs_update(net: &NetworkDef, saved: &SettingsMap) -> bool {
    net.password.is_some() || net.eap.is_some() || net.hidden != saved_hidden(saved)
}

fn setting_ssid(s: &SettingsMap) -> Option<String> {
    s.get("802-11-wireless")
        .and_then(|m| m.get("ssid"))
        .and_then(|v| value_bytes(v))
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .filter(|s| !s.is_empty())
}

/// A client-mode Wi-Fi profile: type `802-11-wireless` and not an access
/// point / hotspot, ad-hoc or mesh profile. Everything breadcrumbs matches,
/// activates, deletes or prunes must pass this, so a wired, VPN or hotspot
/// profile that happens to share a name is never touched.
fn is_wifi_client_profile(s: &SettingsMap) -> bool {
    if setting_str(s, "connection", "type").as_deref() != Some("802-11-wireless") {
        return false;
    }
    !matches!(
        setting_str(s, "802-11-wireless", "mode").as_deref(),
        Some("ap") | Some("adhoc") | Some("mesh")
    )
}

/// Every saved client Wi-Fi profile as `(path, settings)`.
fn wifi_client_profiles(conn: &Connection) -> Vec<(OwnedObjectPath, SettingsMap)> {
    let Some(settings) = proxy(conn, SETTINGS_PATH, SETTINGS_IFACE) else {
        return Vec::new();
    };
    let conns: Vec<OwnedObjectPath> = match settings.call("ListConnections", &()) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    conns
        .into_iter()
        .filter_map(|c| {
            let s = get_settings(conn, c.as_str())?;
            is_wifi_client_profile(&s).then_some((c, s))
        })
        .collect()
}

/// The first saved client Wi-Fi profile whose `802-11-wireless.ssid` is
/// `ssid`. Matching is on the SSID itself, never on the profile name: a
/// renamed profile (or a wired/VPN profile sharing the name) must not be
/// activated in place of the requested network.
fn first_profile_for_ssid(conn: &Connection, ssid: &str) -> Option<OwnedObjectPath> {
    wifi_client_profiles(conn)
        .into_iter()
        .find(|(_, s)| setting_ssid(s).as_deref() == Some(ssid))
        .map(|(c, _)| c)
}

/// Disable autoconnect on every saved client profile for `ssid` except
/// `keep` (the one about to be activated). Desktop GUIs clone a numbered
/// duplicate ("NCC 1", "NCC 2", ...) on some connects; left with autoconnect
/// on, those stale dupes race the next activation and NM reports "base
/// network connection was interrupted" (CRUMB-23). Disabling (rather than
/// deleting) is reversible and preserves any distinct settings a dupe may
/// hold; the kept profile is never touched. Returns the neutered count.
fn neuter_duplicate_profiles(conn: &Connection, keep: Option<&str>, ssid: &str) -> usize {
    let matching: Vec<(OwnedObjectPath, SettingsMap)> = wifi_client_profiles(conn)
        .into_iter()
        .filter(|(_, s)| setting_ssid(s).as_deref() == Some(ssid))
        .collect();
    if matching.len() <= 1 {
        return 0;
    }
    let mut neutered = 0;
    for (path, mut s) in matching {
        if Some(path.as_str()) == keep {
            continue;
        }
        s.entry("connection".into())
            .or_default()
            .insert("autoconnect".into(), ov(Value::from(false)));
        let updated: Result<HashMap<String, OwnedValue>, zbus::Error> =
            match proxy(conn, path.as_str(), CONN_IFACE) {
                Some(p) => p.call(
                    "Update2",
                    &(&s, FLAG_TO_DISK, &HashMap::<String, Value>::new()),
                ),
                None => continue,
            };
        if updated.is_ok() {
            neutered += 1;
        }
    }
    if neutered > 0 {
        log(&format!(
            "neutered {neutered} duplicate profile(s) for '{ssid}' (autoconnect off)"
        ));
    }
    neutered
}

/// A settings file path as the NM blob form: `file://<absolute path>`
/// followed by a terminating NUL, which is how NetworkManager tells a path
/// apart from an inline DER/PEM blob.
fn cert_path_blob(path: &str) -> Vec<u8> {
    let abs = std::fs::canonicalize(path).unwrap_or_else(|_| {
        let p = Path::new(path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|d| d.join(p))
                .unwrap_or_else(|_| p.to_path_buf())
        }
    });
    let mut out = format!("file://{}", abs.display()).into_bytes();
    out.push(0);
    out
}

/// The `802-1x` settings section for an enterprise network.
fn eap_section(net: &NetworkDef) -> HashMap<String, OwnedValue> {
    let mut x1: HashMap<String, OwnedValue> = HashMap::new();
    let method = net.eap.clone().unwrap_or_default().to_lowercase();
    x1.insert("eap".into(), ov(Value::from(vec![method.clone()])));
    if let Some(id) = &net.identity {
        x1.insert("identity".into(), ov(Value::from(id.clone())));
    }
    if let Some(ca) = &net.ca_cert {
        x1.insert("ca-cert".into(), ov(Value::from(cert_path_blob(ca))));
    }
    if method == "tls" {
        if let Some(c) = &net.client_cert {
            x1.insert("client-cert".into(), ov(Value::from(cert_path_blob(c))));
        }
        if let Some(k) = &net.private_key {
            x1.insert("private-key".into(), ov(Value::from(cert_path_blob(k))));
        }
        if let Some(pw) = &net.password {
            x1.insert("private-key-password".into(), ov(Value::from(pw.clone())));
        }
    } else {
        // PEAP and TTLS tunnel an inner method; NetworkManager rejects the
        // profile without one.
        let phase2 = net.phase2.clone().unwrap_or_else(|| "mschapv2".into());
        x1.insert("phase2-auth".into(), ov(Value::from(phase2)));
        if let Some(pw) = &net.password {
            x1.insert("password".into(), ov(Value::from(pw.clone())));
        }
    }
    x1
}

/// Key management for a personal (PSK) network: `sae` for a WPA3-only AP
/// (no PSK AKM advertised), `wpa-psk` otherwise, including WPA2/WPA3
/// transition networks.
fn psk_key_mgmt(ap: Option<&ApInfo>) -> &'static str {
    match ap {
        Some(a) if a.rsn & SEC_SAE != 0 && (a.rsn | a.wpa) & SEC_PSK == 0 => "sae",
        _ => "wpa-psk",
    }
}

/// Build the `a{sa{sv}}` settings dict for a new Wi-Fi connection.
fn wifi_settings(net: &NetworkDef, uuid: &str, ap: Option<&ApInfo>) -> SettingsMap {
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
        settings.insert("802-1x".into(), eap_section(net));
    } else if let Some(pw) = &net.password {
        let mut sec: HashMap<String, OwnedValue> = HashMap::new();
        sec.insert("key-mgmt".into(), ov(Value::from(psk_key_mgmt(ap))));
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

/// Update a saved profile's settings in place for `net`: credentials (PSK,
/// or the 802.1x section for enterprise networks) and the hidden flag.
/// Sections are only created when there is something to put in them: an
/// open network's profile never gains an empty security section (which
/// NetworkManager rejects for lacking `key-mgmt`).
fn updated_settings_for(net: &NetworkDef, s: &mut SettingsMap, ap: Option<&ApInfo>) {
    if net.eap.is_some() {
        let sec = s.entry("802-11-wireless-security".to_string()).or_default();
        sec.insert("key-mgmt".into(), ov(Value::from("wpa-eap")));
        sec.remove("psk");
        s.insert("802-1x".into(), eap_section(net));
    } else if let Some(pw) = &net.password {
        let existing = setting_str(s, "802-11-wireless-security", "key-mgmt");
        let key_mgmt = match existing.as_deref() {
            Some("sae") | Some("wpa-psk") => existing.clone().unwrap_or_default(),
            _ => psk_key_mgmt(ap).to_string(),
        };
        s.remove("802-1x");
        let sec = s.entry("802-11-wireless-security".to_string()).or_default();
        sec.insert("key-mgmt".into(), ov(Value::from(key_mgmt)));
        sec.insert("psk".into(), ov(Value::from(pw.clone())));
    }
    let wifi = s.entry("802-11-wireless".to_string()).or_default();
    wifi.insert("hidden".into(), ov(Value::from(net.hidden)));
}

/// RFC 4122 v4 UUID from `/dev/urandom`: used for the `connection.uuid` of
/// newly created profiles. (NetworkManager would generate one itself if
/// omitted, but being explicit matches `nmcli` and keeps the fake NM simple.)
/// Falls back to clock + pid entropy if `/dev/urandom` is unreadable, so a
/// failure can never produce the same UUID twice.
fn new_uuid() -> String {
    let mut bytes = [0u8; 16];
    let read_ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| {
            use std::io::Read;
            f.read_exact(&mut bytes)
        })
        .is_ok();
    if !read_ok {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mix = nanos ^ (u128::from(std::process::id()) << 64);
        bytes = mix.to_le_bytes();
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40; // version 4
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // variant 10
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7], bytes[8],
        bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
    )
}

/// Wait up to `wait` seconds for the *new* active connection to reach
/// ACTIVATED. Watching the active connection, not the device, matters: right
/// after an activation request the device still reports ACTIVATED for the
/// connection it is about to leave, which used to make a switch look
/// finished before it started.
///
/// On timeout the pending activation is cancelled so it can't complete
/// later underneath whatever the caller tries next.
fn wait_activated(conn: &Connection, active: &OwnedObjectPath, wait: u32) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(u64::from(wait.max(1)));
    loop {
        match proxy(conn, active.as_str(), ACTIVE_IFACE).map(|p| p.get_property::<u32>("State")) {
            Some(Ok(AC_STATE_ACTIVATED)) => return Ok(()),
            Some(Ok(AC_STATE_DEACTIVATING | AC_STATE_DEACTIVATED)) => {
                return Err("activation failed (NetworkManager deactivated the connection)".into())
            }
            Some(Ok(_)) => {}
            // The active-connection object vanishes when NM abandons the
            // attempt (bad secrets, AP gone).
            Some(Err(_)) | None => return Err("activation failed".into()),
        }
        if Instant::now() >= deadline {
            deactivate(conn, active);
            return Err(format!("timed out waiting for activation after {wait}s"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn deactivate(conn: &Connection, active: &OwnedObjectPath) {
    if let Some(nm) = nm_proxy(conn) {
        let _: Result<(), _> = nm.call("DeactivateConnection", &(active,));
    }
}

fn user_data(s: &SettingsMap) -> HashMap<String, String> {
    s.get("user")
        .and_then(|m| m.get("data"))
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| HashMap::<String, String>::try_from(v).ok())
        .unwrap_or_default()
}

/// Rewrite the DNS keys of one IP section. `dns` is the typed `dns` value
/// (`au` for IPv4, `aay` for IPv6, which every NetworkManager version
/// accepts); `dns-data` (`as`, newer NM) is kept in sync when the profile
/// already carries it, since it takes precedence over `dns` when present.
fn set_ip_dns(
    section: &mut HashMap<String, OwnedValue>,
    ignore_auto: bool,
    dns: Value<'_>,
    data: Vec<String>,
) {
    section.insert("ignore-auto-dns".into(), ov(Value::from(ignore_auto)));
    section.insert("dns".into(), ov(dns));
    if section.contains_key("dns-data") {
        section.insert("dns-data".into(), ov(Value::from(data)));
    }
}

/// Apply the DNS pin (or unpin) for `dns` to a settings dict. Returns
/// whether anything changed, so an already-correct profile is not
/// rewritten to disk on every connect.
///
/// A non-empty `dns` pins those servers on both IPv4 and IPv6 (servers of
/// the other family are cleared, so no auto DNS leaks around the pin) and
/// marks the profile in `user.data`. An empty `dns` removes a pin only if
/// the marker shows breadcrumbs made it, restoring automatic DNS.
fn apply_dns(s: &mut SettingsMap, dns: &str) -> Result<bool, String> {
    let mut data = user_data(s);
    let pinned_by_us = data.contains_key(DNS_PIN_MARKER);
    let (v4, v6) = crate::dns::parse_servers(dns)?;

    if v4.is_empty() && v6.is_empty() {
        if !pinned_by_us {
            return Ok(false);
        }
        if let Some(ip4) = s.get_mut("ipv4") {
            set_ip_dns(ip4, false, Value::from(Vec::<u32>::new()), Vec::new());
        }
        if let Some(ip6) = s.get_mut("ipv6") {
            set_ip_dns(ip6, false, Value::from(Vec::<Vec<u8>>::new()), Vec::new());
        }
        data.remove(DNS_PIN_MARKER);
    } else {
        let want = format!("{v4:?}{v6:?}");
        if data.get(DNS_PIN_MARKER) == Some(&want) {
            return Ok(false);
        }
        let v4_wire: Vec<u32> = v4.iter().map(|a| u32::from_ne_bytes(a.octets())).collect();
        let v4_str: Vec<String> = v4.iter().map(ToString::to_string).collect();
        let v6_wire: Vec<Vec<u8>> = v6.iter().map(|a| a.octets().to_vec()).collect();
        let v6_str: Vec<String> = v6.iter().map(ToString::to_string).collect();
        let ip4 = s.entry("ipv4".to_string()).or_default();
        set_ip_dns(ip4, true, Value::from(v4_wire), v4_str);
        // Create the ipv6 section too, so auto DNS cannot leak around the pin.
        // Only DNS keys are written; no ipv6 `method`, so NM keeps its default.
        let ip6 = s.entry("ipv6".to_string()).or_default();
        set_ip_dns(ip6, true, Value::from(v6_wire), v6_str);
        data.insert(DNS_PIN_MARKER.to_string(), want);
    }
    let user = s.entry("user".to_string()).or_default();
    user.insert("data".into(), ov(Value::from(data)));
    Ok(true)
}

/// Pin (or unpin, see [`apply_dns`]) DNS on the active connection of `dev`:
/// persist via Update2, then Reapply on the device so it takes effect
/// without bouncing the link.
fn enforce_dns(conn: &Connection, dev: &OwnedObjectPath, dns: &str) -> Result<(), String> {
    let active = active_connection_path(conn, dev).ok_or("no active connection")?;
    let mut s = get_settings(conn, active.as_str()).ok_or("could not read profile settings")?;
    if !apply_dns(&mut s, dns)? {
        return Ok(());
    }

    // Persist the change on the *profile* object via `Update2`: calling
    // `Settings.AddConnection2` with the profile's own UUID would fail on
    // real NetworkManager (NM_SETTINGS_ERROR_UUID_EXISTS; duplicates are
    // rejected, not upserted).
    let conn_proxy = proxy(conn, active.as_str(), CONN_IFACE).ok_or("profile unavailable")?;
    let _: HashMap<String, OwnedValue> = conn_proxy
        .call(
            "Update2",
            &(&s, FLAG_TO_DISK, &HashMap::<String, Value>::new()),
        )
        .map_err(err_text)?;

    let dev_proxy = proxy(conn, dev.as_str(), DEV_IFACE).ok_or("device unavailable")?;
    dev_proxy
        .call::<_, _, ()>("Reapply", &(&s, 0u64, 0u32))
        .map_err(err_text)
}

fn enforce_dns_logged(conn: &Connection, dev: &OwnedObjectPath, ssid: &str, dns: &str) {
    if let Err(e) = enforce_dns(conn, dev, dns) {
        log(&format!("dns: could not apply DNS setting on {ssid}: {e}"));
    }
}

/// Connect to a network and pin DNS. Returns true only if associated.
pub fn connect(iface: &str, net: &NetworkDef, wait: u32, dns: &str) -> bool {
    connect_verbose(iface, net, wait, dns).is_ok()
}

/// Connect to a network and pin DNS. Returns the D-Bus error on failure.
///
/// Idempotent: if the device is already fully connected to `net.ssid`,
/// nothing is re-activated (re-activating drops a working link, which is
/// what made recovery runs cause outages on slow networks). A new password
/// is still saved to the profile and DNS is still applied, both without
/// bouncing the link.
///
/// Otherwise reuses an existing saved profile for the SSID when one exists
/// (updating its credentials) so that repeated connections do not
/// accumulate numbered duplicates in NetworkManager, falling back to
/// creating a new connection (`AddAndActivateConnection2`, which also
/// activates it) only when no saved profile is found.
///
/// `net.password` is only sent when `Some`: on the reuse path, `None` means
/// "leave the saved secret alone" (either NetworkManager already durably
/// owns it, or the network is open); on the create path it means "no secret
/// section", which is also how a genuinely open (no-security) SSID is
/// connected. See the field doc on [`NetworkDef::password`] for how a local
/// secret transitions to `None` after its first successful use.
///
/// The secret (when sent) travels inside the D-Bus `Update2`/`AddAndActivate
/// Connection2` settings payload, never on a process argv line.
pub fn connect_verbose(iface: &str, net: &NetworkDef, wait: u32, dns: &str) -> Result<(), String> {
    crate::dns::validate(dns)?;
    let conn = connection().ok_or_else(|| "cannot connect to the D-Bus system bus".to_string())?;
    let dev = device_path(&conn, iface)
        .ok_or_else(|| format!("no NetworkManager device named '{iface}'"))?;
    let ap = strongest_ap(&conn, iface, &net.ssid);
    let profile = first_profile_for_ssid(&conn, &net.ssid);
    // Pre-existing numbered dupes must not out-autoconnect the profile we
    // are about to use (CRUMB-23); a no-op when there is at most one.
    neuter_duplicate_profiles(&conn, profile.as_ref().map(|p| p.as_str()), &net.ssid);

    if let Some(profile) = &profile {
        let mut s = get_settings(&conn, profile.as_str())
            .ok_or_else(|| "failed to read saved profile settings".to_string())?;
        if profile_needs_update(net, &s) {
            updated_settings_for(net, &mut s, ap.as_ref());
            // Update the existing profile in place via `Settings.Connection
            // .Update2`. `Settings.AddConnection2` with the profile's own
            // UUID would be rejected by real NetworkManager
            // (NM_SETTINGS_ERROR_UUID_EXISTS: duplicates are not upserted).
            let conn_proxy = proxy(&conn, profile.as_str(), CONN_IFACE)
                .ok_or_else(|| "NetworkManager Settings.Connection unavailable".to_string())?;
            let _: HashMap<String, OwnedValue> = conn_proxy
                .call(
                    "Update2",
                    &(&s, FLAG_TO_DISK, &HashMap::<String, Value>::new()),
                )
                .map_err(err_text)?;
        }
    }

    if device_activated(&conn, &dev) && active_ssid_on(&conn, &dev).as_deref() == Some(&net.ssid) {
        enforce_dns_logged(&conn, &dev, &net.ssid, dns);
        return Ok(());
    }

    let nm = nm_proxy(&conn).ok_or_else(|| "NetworkManager unavailable".to_string())?;
    let specific = zbus::zvariant::ObjectPath::try_from("/").expect("root object path");
    let active: OwnedObjectPath = if let Some(profile) = &profile {
        nm.call("ActivateConnection", &(profile, &dev, &specific))
            .map_err(err_text)?
    } else {
        // No saved profile: create one (and activate it in one call).
        let settings = wifi_settings(net, &new_uuid(), ap.as_ref());
        let options: HashMap<String, Value> =
            HashMap::from([("persist".to_string(), Value::from("disk"))]);
        let (_path, active): (OwnedObjectPath, OwnedObjectPath) = nm
            .call(
                "AddAndActivateConnection2",
                &(&settings, &dev, &specific, &options),
            )
            .map_err(err_text)?;
        active
    };
    wait_activated(&conn, &active, wait)?;
    enforce_dns_logged(&conn, &dev, &net.ssid, dns);
    Ok(())
}

/// List all client Wi-Fi connection profiles as `(name, ssid)` pairs, using
/// the profile's `802-11-wireless.ssid` setting when present (falling back
/// to the profile name). Hotspot / ad-hoc profiles are never listed. Used
/// by `breadcrumbs prune`.
pub fn wireless_profiles() -> Vec<(String, String)> {
    let Some(conn) = connection() else {
        return Vec::new();
    };
    wifi_client_profiles(&conn)
        .into_iter()
        .map(|(_, s)| {
            let name = setting_str(&s, "connection", "id").unwrap_or_default();
            let ssid = setting_ssid(&s).unwrap_or_else(|| name.clone());
            (name, ssid)
        })
        .collect()
}

/// Delete every saved client Wi-Fi profile for `ssid` (used by
/// `breadcrumbs forget` and `prune`). A profile matches on its SSID, or on
/// its name when it has no SSID set; wired, VPN and hotspot profiles are
/// never deleted, whatever their name.
pub fn delete_connections_for_ssid(ssid: &str) -> bool {
    let Some(conn) = connection() else {
        return false;
    };
    let mut removed = false;
    for (c, s) in wifi_client_profiles(&conn) {
        let matches = match setting_ssid(&s) {
            Some(profile_ssid) => profile_ssid == ssid,
            None => setting_str(&s, "connection", "id").as_deref() == Some(ssid),
        };
        if !matches {
            continue;
        }
        if let Some(p) = proxy(&conn, c.as_str(), CONN_IFACE) {
            if p.call::<_, _, ()>("Delete", &()).is_ok() {
                removed = true;
            }
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn ap(wpa: u32, rsn: u32) -> ApInfo {
        ApInfo {
            ssid: "x".into(),
            strength: 50,
            flags: AP_FLAG_PRIVACY,
            wpa,
            rsn,
        }
    }

    #[test]
    fn security_string_derivation() {
        // Open AP: no privacy bit, no WPA/RSN.
        assert_eq!(security_string(0, 0, 0), "--");
        // WPA2-PSK (RSN PSK set).
        assert_eq!(security_string(AP_FLAG_PRIVACY, 0, SEC_PSK), "WPA2");
        // WPA1+WPA2 (both flag sets carry PSK).
        assert_eq!(
            security_string(AP_FLAG_PRIVACY, SEC_PSK, SEC_PSK),
            "WPA1 WPA2"
        );
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
    fn psk_key_mgmt_uses_sae_only_for_wpa3_only_aps() {
        assert_eq!(psk_key_mgmt(Some(&ap(0, SEC_SAE))), "sae");
        // Transition mode (PSK + SAE) keeps wpa-psk, which works either way.
        assert_eq!(psk_key_mgmt(Some(&ap(0, SEC_SAE | SEC_PSK))), "wpa-psk");
        assert_eq!(psk_key_mgmt(Some(&ap(0, SEC_PSK))), "wpa-psk");
        // Not visible (hidden): assume WPA2.
        assert_eq!(psk_key_mgmt(None), "wpa-psk");
    }

    #[test]
    fn cert_path_blob_is_file_uri_with_trailing_nul() {
        let b = cert_path_blob("/nonexistent/ca.pem");
        assert_eq!(b, b"file:///nonexistent/ca.pem\0".to_vec());
    }

    #[test]
    fn connect_rejects_invalid_dns_before_opening_dbus() {
        let error = connect_verbose("wlan0", &NetworkDef::default(), 0, "1.1.1.x").unwrap_err();
        assert!(error.contains("invalid DNS server '1.1.1.x'"));
    }

    #[test]
    fn apply_dns_rejects_invalid_nonempty_value_without_mutating_existing_pin() {
        let mut s = settings_with_ip();
        assert!(apply_dns(&mut s, "1.1.1.1").unwrap());
        let before_v4: Vec<u32> = s["ipv4"]["dns"].try_clone().unwrap().try_into().unwrap();
        let before_data = user_data(&s);
        assert!(apply_dns(&mut s, "1.1.1.x").is_err());
        let after_v4: Vec<u32> = s["ipv4"]["dns"].try_clone().unwrap().try_into().unwrap();
        assert_eq!(after_v4, before_v4);
        assert_eq!(user_data(&s), before_data);
        assert!(apply_dns(&mut s, "1.1.1.1, bogus").is_err());
        let after_v4: Vec<u32> = s["ipv4"]["dns"].try_clone().unwrap().try_into().unwrap();
        assert_eq!(after_v4, before_v4);
        assert_eq!(user_data(&s), before_data);
    }

    #[test]
    fn failed_parse_cannot_turn_an_invalid_dns_value_into_unpin() {
        let mut s = settings_with_ip();
        assert!(apply_dns(&mut s, "1.1.1.1").unwrap());
        let before_v4: Vec<u32> = s["ipv4"]["dns"].try_clone().unwrap().try_into().unwrap();
        let before_data = user_data(&s);
        let attempt = (|| {
            crate::dns::validate("1.1.1.x")?;
            apply_dns(&mut s, "1.1.1.x")
        })();
        assert!(attempt.is_err());
        let after_v4: Vec<u32> = s["ipv4"]["dns"].try_clone().unwrap().try_into().unwrap();
        assert_eq!(after_v4, before_v4);
        assert_eq!(
            user_data(&s),
            before_data,
            "invalid config preserves pin marker"
        );
        assert!(get_bool(&s, "ipv4", "ignore-auto-dns"));
        assert!(user_data(&s).contains_key(DNS_PIN_MARKER));
    }

    #[test]
    fn parse_dns_splits_families_and_rejects_garbage() {
        let (v4, v6) = crate::dns::parse_servers("1.1.1.1, 2606:4700::1111;9.9.9.9").unwrap();
        assert_eq!(
            v4,
            vec![Ipv4Addr::new(1, 1, 1, 1), Ipv4Addr::new(9, 9, 9, 9)]
        );
        assert_eq!(v6, vec!["2606:4700::1111".parse::<Ipv6Addr>().unwrap()]);
        assert_eq!(crate::dns::parse_servers("").unwrap(), (vec![], vec![]));
        assert!(crate::dns::parse_servers("bogus").is_err());
    }

    fn settings_with_ip() -> SettingsMap {
        let mut s: SettingsMap = HashMap::new();
        let mut ip4 = HashMap::new();
        ip4.insert("method".into(), ov(Value::from("auto")));
        ip4.insert("dns-data".into(), ov(Value::from(Vec::<String>::new())));
        s.insert("ipv4".into(), ip4);
        let mut ip6 = HashMap::new();
        ip6.insert("method".into(), ov(Value::from("auto")));
        s.insert("ipv6".into(), ip6);
        s
    }

    fn get_bool(s: &SettingsMap, section: &str, key: &str) -> bool {
        s[section][key].downcast_ref::<bool>().unwrap()
    }

    #[test]
    fn apply_dns_pins_both_families_with_wire_types_and_marks_profile() {
        let mut s = settings_with_ip();
        assert!(apply_dns(&mut s, "10.0.0.1").unwrap());
        let dns: Vec<u32> = s["ipv4"]["dns"].try_clone().unwrap().try_into().unwrap();
        // Network byte order: the in-memory bytes are the octets in order.
        assert_eq!(dns, vec![u32::from_ne_bytes([10, 0, 0, 1])]);
        let data: Vec<String> = s["ipv4"]["dns-data"]
            .try_clone()
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(data, vec!["10.0.0.1".to_string()]);
        assert!(get_bool(&s, "ipv4", "ignore-auto-dns"));
        // IPv6 auto DNS is ignored too, so nothing leaks around the pin.
        assert!(get_bool(&s, "ipv6", "ignore-auto-dns"));
        assert!(user_data(&s).contains_key(DNS_PIN_MARKER));
        // Re-applying the same pin is a no-op (no disk write).
        assert!(!apply_dns(&mut s, "10.0.0.1").unwrap());
    }

    #[test]
    fn apply_dns_empty_unpins_only_our_own_pin() {
        let mut untouched = settings_with_ip();
        assert!(
            !apply_dns(&mut untouched, "").unwrap(),
            "no marker: leave user DNS alone"
        );

        let mut s = settings_with_ip();
        apply_dns(&mut s, "1.1.1.1").unwrap();
        assert!(apply_dns(&mut s, "").unwrap());
        assert!(!get_bool(&s, "ipv4", "ignore-auto-dns"));
        let dns: Vec<u32> = s["ipv4"]["dns"].try_clone().unwrap().try_into().unwrap();
        assert!(dns.is_empty());
        assert!(!user_data(&s).contains_key(DNS_PIN_MARKER));
    }

    fn settings_without_ipv6() -> SettingsMap {
        let mut s: SettingsMap = HashMap::new();
        let mut ip4 = HashMap::new();
        ip4.insert("method".into(), ov(Value::from("auto")));
        s.insert("ipv4".into(), ip4);
        s
    }

    #[test]
    fn apply_dns_pin_creates_ipv6_section_when_profile_has_none() {
        // Without an ipv6 section, DHCP/RA DNS would leak around the pin.
        let mut s = settings_without_ipv6();
        assert!(apply_dns(&mut s, "10.0.0.1, 2606:4700:4700::1111").unwrap());
        assert!(get_bool(&s, "ipv4", "ignore-auto-dns"));
        assert!(
            s.contains_key("ipv6"),
            "pin must create the ipv6 section like ipv4"
        );
        assert!(get_bool(&s, "ipv6", "ignore-auto-dns"));
        let v4: Vec<u32> = s["ipv4"]["dns"].try_clone().unwrap().try_into().unwrap();
        assert_eq!(v4, vec![u32::from_ne_bytes([10, 0, 0, 1])]);
        // IPv6 DNS uses the byte-array wire type (aay).
        let v6: Vec<Vec<u8>> = s["ipv6"]["dns"].try_clone().unwrap().try_into().unwrap();
        assert_eq!(
            v6,
            vec!["2606:4700:4700::1111"
                .parse::<Ipv6Addr>()
                .unwrap()
                .octets()
                .to_vec()]
        );
        // Only DNS keys are added: no ipv6 method, so NM keeps its default.
        assert!(!s["ipv6"].contains_key("method"));
        assert!(user_data(&s).contains_key(DNS_PIN_MARKER));
    }

    #[test]
    fn apply_dns_pin_v4_only_still_clears_ipv6_auto_dns_on_created_section() {
        let mut s = settings_without_ipv6();
        assert!(apply_dns(&mut s, "9.9.9.9").unwrap());
        let v6: Vec<Vec<u8>> = s["ipv6"]["dns"].try_clone().unwrap().try_into().unwrap();
        assert!(v6.is_empty(), "no v6 servers: nothing to pin on ipv6");
        assert!(get_bool(&s, "ipv6", "ignore-auto-dns"));
        assert!(!s["ipv6"].contains_key("method"));
    }

    #[test]
    fn apply_dns_unpin_does_not_invent_ipv6_section() {
        let mut s = settings_without_ipv6();
        apply_dns(&mut s, "1.1.1.1").unwrap();
        // Remove the ipv6 section to simulate a profile that never had one.
        s.remove("ipv6");
        assert!(apply_dns(&mut s, "").unwrap());
        assert!(!s.contains_key("ipv6"), "unpin must not create ipv6");
        assert!(!get_bool(&s, "ipv4", "ignore-auto-dns"));
        assert!(!user_data(&s).contains_key(DNS_PIN_MARKER));
    }

    #[test]
    fn apply_dns_unpin_without_ipv6_section_leaves_no_ipv6_when_pinned_before() {
        // A profile pinned before this fix has no ipv6 section at all.
        let mut s = settings_without_ipv6();
        let mut data = HashMap::new();
        data.insert(DNS_PIN_MARKER.to_string(), "[1]".to_string());
        s.insert(
            "user".into(),
            HashMap::from([("data".into(), ov(Value::from(data)))]),
        );
        assert!(apply_dns(&mut s, "").unwrap());
        assert!(!s.contains_key("ipv6"));
    }

    #[test]
    fn updated_settings_for_open_hidden_network_adds_no_security_section() {
        let mut s: SettingsMap = HashMap::new();
        let net = NetworkDef {
            ssid: "Open".into(),
            hidden: true,
            ..Default::default()
        };
        updated_settings_for(&net, &mut s, None);
        assert!(!s.contains_key("802-11-wireless-security"));
        assert!(get_bool(&s, "802-11-wireless", "hidden"));
    }

    #[test]
    fn unhiding_a_network_triggers_a_profile_update_and_clears_the_flag() {
        let mut saved: SettingsMap = HashMap::new();
        let mut wifi = HashMap::new();
        wifi.insert("hidden".into(), ov(Value::from(true)));
        saved.insert("802-11-wireless".into(), wifi);
        let visible = NetworkDef {
            ssid: "Net".into(),
            hidden: false,
            ..Default::default()
        };
        // No password, no eap: only the hidden flag differs.
        assert!(profile_needs_update(&visible, &saved));
        updated_settings_for(&visible, &mut saved, None);
        assert!(!get_bool(&saved, "802-11-wireless", "hidden"));
        // Once saved matches the config there is nothing left to rewrite.
        assert!(!profile_needs_update(&visible, &saved));
    }

    #[test]
    fn hidden_flag_matching_the_saved_profile_does_not_force_a_rewrite() {
        // NetworkManager omits `hidden` when false.
        let saved: SettingsMap = HashMap::new();
        let visible = NetworkDef {
            ssid: "Net".into(),
            ..Default::default()
        };
        assert!(!profile_needs_update(&visible, &saved));
        let hidden = NetworkDef {
            hidden: true,
            ..visible.clone()
        };
        assert!(profile_needs_update(&hidden, &saved));
        let with_pw = NetworkDef {
            password: Some("x".into()),
            ..visible
        };
        assert!(profile_needs_update(&with_pw, &saved));
    }

    #[test]
    fn updated_settings_for_keeps_existing_sae_key_mgmt() {
        let mut s: SettingsMap = HashMap::new();
        let mut sec = HashMap::new();
        sec.insert("key-mgmt".into(), ov(Value::from("sae")));
        s.insert("802-11-wireless-security".into(), sec);
        let net = NetworkDef {
            ssid: "W3".into(),
            password: Some("pw".into()),
            ..Default::default()
        };
        updated_settings_for(&net, &mut s, None);
        assert_eq!(
            setting_str(&s, "802-11-wireless-security", "key-mgmt").as_deref(),
            Some("sae")
        );
    }

    #[test]
    fn eap_section_sets_phase2_for_peap_and_certs_for_tls() {
        let peap = NetworkDef {
            ssid: "Corp".into(),
            eap: Some("PEAP".into()),
            password: Some("pw".into()),
            ..Default::default()
        };
        let x1 = eap_section(&peap);
        assert_eq!(
            value_string(&x1["phase2-auth"]).as_deref(),
            Some("mschapv2")
        );
        assert!(x1.contains_key("password"));

        let tls = NetworkDef {
            ssid: "Corp".into(),
            eap: Some("tls".into()),
            client_cert: Some("/nonexistent/c.pem".into()),
            private_key: Some("/nonexistent/k.pem".into()),
            password: Some("keypw".into()),
            ..Default::default()
        };
        let x1 = eap_section(&tls);
        assert!(!x1.contains_key("phase2-auth"));
        assert!(!x1.contains_key("password"));
        assert_eq!(
            value_bytes(&x1["client-cert"]).unwrap(),
            b"file:///nonexistent/c.pem\0".to_vec()
        );
        assert_eq!(
            value_string(&x1["private-key-password"]).as_deref(),
            Some("keypw")
        );
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
