//! A faithful fake NetworkManager served as a real D-Bus service on a
//! private `dbus-daemon`, so the production `nm` module (a pure zbus client
//! against `org.freedesktop.NetworkManager` on the system bus) can be
//! exercised end-to-end — real D-Bus marshalling, real property reads, real
//! method calls — with zero test-only code paths in `src/`.
//!
//! Two ways to use it:
//!
//! - [`launch_private`] — starts a fresh private daemon + fake NM and
//!   returns a [`FakeNmBus`] handle. The caller passes
//!   `DBUS_SYSTEM_BUS_ADDRESS=<addr>` to any subprocess (the CLI sandbox) so
//!   the binary's `Connection::system()` lands on this bus. Independent per
//!   test → safe to run in parallel.
//! - [`shared`] — one process-wide fake NM bus pointed at by the process
//!   env var (in-process tests can't set per-test env safely). Tests using
//!   it must serialize against each other, which the returned guard does.
//!
//! The fake implements the subset of the NetworkManager D-Bus API the
//! client uses: devices, access points (SSID/strength/security flags),
//! connection profiles (list/add/get/delete), activation (which lands the
//! device on the matching AP and marks it ACTIVATED), per-device IP4Config,
//! and the `WirelessEnabled`/`Connectivity` root properties. It also
//! records every call for assertions.

use std::collections::{BTreeMap, HashMap};
use std::io::BufRead;
use std::ops::Deref;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use zbus::fdo;
use zbus::interface;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

pub const NM_DEST: &str = "org.freedesktop.NetworkManager";
pub const NM_PATH: &str = "/org/freedesktop/NetworkManager";
pub const SETTINGS_PATH: &str = "/org/freedesktop/NetworkManager/Settings";
const DEVICES_PREFIX: &str = "/org/freedesktop/NetworkManager/Devices";
const APS_PREFIX: &str = "/org/freedesktop/NetworkManager/AccessPoints";
const ACTIVE_PREFIX: &str = "/org/freedesktop/NetworkManager/ActiveConnection";
const IP4_PREFIX: &str = "/org/freedesktop/NetworkManager/IP4Config";
const CONNS_PREFIX: &str = "/org/freedesktop/NetworkManager/Settings";

// Security flag constants (mirrors src/nm.rs).
pub const DEV_TYPE_WIFI: u32 = 2;
pub const DEV_STATE_ACTIVATED: u32 = 100;
const AP_FLAG_PRIVACY: u32 = 0x1;
const SEC_PSK: u32 = 0x100;
const SEC_802_1X: u32 = 0x200;
const SEC_SAE: u32 = 0x400;

/// Wi-Fi security flavors the fake can advertise for an AP.
#[derive(Debug, Clone, Copy)]
pub enum Security {
    Open,
    Wpa2,
    Wpa3,
    Wpa1Wpa2,
    Enterprise,
    Wep,
}

impl Security {
    fn to_flags(self) -> (u32, u32, u32) {
        match self {
            Security::Open => (0, 0, 0),
            Security::Wpa2 => (AP_FLAG_PRIVACY, 0, SEC_PSK),
            Security::Wpa3 => (AP_FLAG_PRIVACY, 0, SEC_SAE),
            Security::Wpa1Wpa2 => (AP_FLAG_PRIVACY, SEC_PSK, SEC_PSK),
            Security::Enterprise => (AP_FLAG_PRIVACY, SEC_802_1X, SEC_802_1X),
            Security::Wep => (AP_FLAG_PRIVACY, 0, 0),
        }
    }
}

pub type SettingsMap = HashMap<String, HashMap<String, OwnedValue>>;

#[derive(Debug, Clone)]
pub struct FakeDeviceData {
    pub iface: String,
    pub dev_type: u32,
    pub state: u32,
    pub active_ap: Option<String>,
    pub ip4: String,
}

#[derive(Debug, Clone)]
pub struct FakeApData {
    pub ssid: Vec<u8>,
    pub strength: u8,
    pub flags: u32,
    pub wpa: u32,
    pub rsn: u32,
}

#[derive(Debug, Default)]
pub struct FakeState {
    pub devices: BTreeMap<String, FakeDeviceData>,
    pub aps: BTreeMap<String, FakeApData>,
    pub dev_aps: HashMap<String, Vec<String>>,
    pub connections: BTreeMap<String, SettingsMap>,
    /// active-conn path -> settings-conn path
    pub active_conns: BTreeMap<String, String>,
    /// device path -> active-conn path
    pub dev_active: HashMap<String, String>,
    pub connectivity: u32,
    pub wireless_enabled: bool,
    /// When set, activation lands the device on the AP with this SSID
    /// (simulates an NM autoconnect race landing elsewhere).
    pub land_on: Option<String>,
    /// When set, activation creates a matching AP on the fly if none exists
    /// (hidden networks appear in the scan only after connecting).
    pub connect_any: bool,
    /// When > 0, the next activations fail (simulating a transient NM
    /// failure); each attempted activation decrements the counter.
    pub fail_next_activations: u32,
    pub calls: Vec<String>,
    pub next_dev: u32,
    pub next_ap: u32,
    pub next_conn: u32,
    pub next_active: u32,
}

#[derive(Clone)]
struct Shared {
    state: Arc<Mutex<FakeState>>,
}

pub fn value_bytes(v: &OwnedValue) -> Option<Vec<u8>> {
    match v.deref() {
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

/// Extract an array-of-strings (e.g. `ipv4.dns`, `802-1x.eap`) from a
/// settings dict value. zvariant has no `TryFrom<&Value>` for `Vec<String>`,
/// so we peel the `Array` ourselves.
pub fn value_str_list(v: &OwnedValue) -> Option<Vec<String>> {
    match v.deref() {
        Value::Array(a) => {
            let mut out = Vec::new();
            for item in a.inner() {
                match item {
                    Value::Str(s) => out.push(s.as_str().to_string()),
                    _ => return None,
                }
            }
            Some(out)
        }
        _ => None,
    }
}

fn ov(v: Value<'_>) -> OwnedValue {
    OwnedValue::try_from(v).expect("settings value is ownable")
}

/// OwnedValue isn't `Clone` (only `try_clone`), so settings dicts must be
/// copied field-by-field when the fake hands one out.
fn clone_settings(s: &SettingsMap) -> SettingsMap {
    s.iter()
        .map(|(section, vals)| {
            let cloned = vals
                .iter()
                .map(|(k, v)| v.try_clone().map(|c| (k.clone(), c)))
                .collect::<Result<HashMap<String, OwnedValue>, _>>()
                .expect("settings values are ownable");
            (section.clone(), cloned)
        })
        .collect()
}

fn conn_ssid(s: &SettingsMap) -> Option<String> {
    s.get("802-11-wireless")
        .and_then(|m| m.get("ssid"))
        .and_then(value_bytes)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
}

fn conn_id(s: &SettingsMap) -> Option<String> {
    s.get("connection")
        .and_then(|m| m.get("id"))
        .and_then(|v| v.downcast_ref::<String>().ok())
}

fn obj(path: &str) -> OwnedObjectPath {
    OwnedObjectPath::try_from(path).expect("valid object path")
}

// ---------------------------------------------------------------------
// Root interface: org.freedesktop.NetworkManager
// ---------------------------------------------------------------------

struct FakeNm {
    shared: Shared,
}

#[interface(name = "org.freedesktop.NetworkManager")]
impl FakeNm {
    async fn get_devices(&self) -> fdo::Result<Vec<OwnedObjectPath>> {
        let st = self.shared.state.lock().unwrap();
        Ok(st.devices.keys().map(|p| obj(p)).collect())
    }

    #[zbus(property)]
    fn wireless_enabled(&self) -> fdo::Result<bool> {
        Ok(self.shared.state.lock().unwrap().wireless_enabled)
    }

    // Setters must return `zbus::Error` (not `fdo::Error`): the macro's
    // generated setter keeps the fallible arm's error type verbatim, and the
    // dispatch future is typed `Result<(), zbus::Error>`.
    #[zbus(property)]
    fn set_wireless_enabled(&self, v: bool) -> zbus::Result<()> {
        self.shared.state.lock().unwrap().wireless_enabled = v;
        Ok(())
    }

    #[zbus(property)]
    fn connectivity(&self) -> fdo::Result<u32> {
        Ok(self.shared.state.lock().unwrap().connectivity)
    }

    async fn activate_connection(
        &self,
        conn: OwnedObjectPath,
        dev: OwnedObjectPath,
        _specific: OwnedObjectPath,
        #[zbus(connection)] c: &zbus::Connection,
    ) -> fdo::Result<OwnedObjectPath> {
        let active = self.register_active(c, conn.as_str(), dev.as_str()).await?;
        Ok(active)
    }

    async fn add_and_activate_connection2(
        &self,
        settings: SettingsMap,
        dev: OwnedObjectPath,
        _specific: OwnedObjectPath,
        _options: HashMap<String, OwnedValue>,
        #[zbus(connection)] c: &zbus::Connection,
    ) -> fdo::Result<(OwnedObjectPath, OwnedObjectPath)> {
        let conn_path = self.save_connection(settings, c).await?;
        let active = self.register_active(c, conn_path.as_str(), dev.as_str()).await?;
        Ok((conn_path, active))
    }
}

impl FakeNm {
    async fn save_connection(&self, settings: SettingsMap, c: &zbus::Connection) -> fdo::Result<OwnedObjectPath> {
        let path = {
            let mut st = self.shared.state.lock().unwrap();
            st.calls.push("AddAndActivateConnection2".into());
            // Same UUID-exists semantics as real NetworkManager: this call
            // creates a *new* profile, so a duplicate UUID is an error, not
            // an upsert (updates go through Settings.Connection.Update2).
            if st.connections.values().any(|s| conn_id(s) == conn_id(&settings)) {
                return Err(fdo::Error::Failed(
                    "A connection with this UUID already exists.".into(),
                ));
            }
            let p = format!("{CONNS_PREFIX}/{}", st.next_conn);
            st.next_conn += 1;
            st.connections.insert(p.clone(), settings);
            p
        };
        c.object_server()
            .at(path.as_str(), FakeConn {
                shared: self.shared.clone(),
                path: path.clone(),
            })
            .await?;
        Ok(obj(&path))
    }

    async fn register_active(
        &self,
        c: &zbus::Connection,
        conn_path: &str,
        dev: &str,
    ) -> fdo::Result<OwnedObjectPath> {
        let (active_path, new_ap) = {
            let mut st = self.shared.state.lock().unwrap();
            st.calls.push(format!("activate {conn_path} -> {dev}"));
            if st.fail_next_activations > 0 {
                st.fail_next_activations -= 1;
                return Err(fdo::Error::Failed("transient activation failure".into()));
            }
            let ssid = st.connections.get(conn_path).and_then(conn_ssid);
            let target = st.land_on.clone().or(ssid);
            let mut dev_ap = target.as_ref().and_then(|t| {
                let aps = st.dev_aps.get(dev).cloned().unwrap_or_default();
                aps.into_iter().find(|ap| {
                    st.aps
                        .get(ap)
                        .map(|a| String::from_utf8_lossy(&a.ssid).into_owned() == *t)
                        .unwrap_or(false)
                })
            });
            // Hidden networks don't appear in a scan until after they're
            // associated; create the AP on the fly in that case.
            let mut new_ap = None;
            if dev_ap.is_none() && st.connect_any {
                if let Some(t) = &target {
                    let id = st.next_ap;
                    st.next_ap += 1;
                    let ap_path = format!("{APS_PREFIX}/{id}");
                    st.aps.insert(
                        ap_path.clone(),
                        FakeApData {
                            ssid: t.as_bytes().to_vec(),
                            strength: 80,
                            flags: AP_FLAG_PRIVACY,
                            wpa: 0,
                            rsn: SEC_PSK,
                        },
                    );
                    st.dev_aps.entry(dev.to_string()).or_default().push(ap_path.clone());
                    dev_ap = Some(ap_path.clone());
                    new_ap = Some(ap_path);
                }
            }
            if let Some(d) = st.devices.get_mut(dev) {
                if let Some(ap) = &dev_ap {
                    d.active_ap = Some(ap.clone());
                }
                d.state = DEV_STATE_ACTIVATED;
            }
            let p = format!("{ACTIVE_PREFIX}/{}", st.next_active);
            st.next_active += 1;
            st.active_conns.insert(p.clone(), conn_path.to_string());
            st.dev_active.insert(dev.to_string(), p.clone());
            (p, new_ap)
        };
        if let Some(ap) = &new_ap {
            c.object_server()
                .at(ap.as_str(), FakeAp {
                    shared: self.shared.clone(),
                    path: ap.clone(),
                })
                .await?;
        }
        c.object_server()
            .at(active_path.as_str(), FakeActive {
                shared: self.shared.clone(),
                path: active_path.clone(),
            })
            .await?;
        Ok(obj(&active_path))
    }
}

// ---------------------------------------------------------------------
// Settings: org.freedesktop.NetworkManager.Settings
// ---------------------------------------------------------------------

struct FakeSettings {
    shared: Shared,
}

#[interface(name = "org.freedesktop.NetworkManager.Settings")]
impl FakeSettings {
    async fn list_connections(&self) -> fdo::Result<Vec<OwnedObjectPath>> {
        let st = self.shared.state.lock().unwrap();
        Ok(st.connections.keys().map(|p| obj(p)).collect())
    }

    async fn add_connection2(
        &self,
        settings: SettingsMap,
        _flags: u32,
        _args: HashMap<String, OwnedValue>,
        #[zbus(connection)] c: &zbus::Connection,
    ) -> fdo::Result<(OwnedObjectPath, HashMap<String, OwnedValue>)> {
        let path = {
            let mut st = self.shared.state.lock().unwrap();
            st.calls.push("AddConnection2".into());
            // Real NetworkManager rejects a duplicate UUID with
            // NM_SETTINGS_ERROR_UUID_EXISTS — it does NOT upsert. Existing
            // profiles must be edited via Settings.Connection.Update2; model
            // that here so a client regression fails loudly.
            if st.connections.values().any(|s| conn_id(s) == conn_id(&settings)) {
                return Err(fdo::Error::Failed(
                    "A connection with this UUID already exists.".into(),
                ));
            }
            let p = format!("{CONNS_PREFIX}/{}", st.next_conn);
            st.next_conn += 1;
            st.connections.insert(p.clone(), settings);
            p
        };
        c.object_server()
            .at(path.as_str(), FakeConn {
                shared: self.shared.clone(),
                path: path.clone(),
            })
            .await?;
        Ok((obj(&path), HashMap::new()))
    }
}

// ---------------------------------------------------------------------
// Settings.Connection
// ---------------------------------------------------------------------

struct FakeConn {
    shared: Shared,
    path: String,
}

#[interface(name = "org.freedesktop.NetworkManager.Settings.Connection")]
impl FakeConn {
    async fn get_settings(&self) -> fdo::Result<SettingsMap> {
        let st = self.shared.state.lock().unwrap();
        let settings = st
            .connections
            .get(&self.path)
            .ok_or_else(|| fdo::Error::UnknownObject(self.path.clone()))?;
        Ok(clone_settings(settings))
    }

    async fn delete(&self) -> fdo::Result<()> {
        let mut st = self.shared.state.lock().unwrap();
        st.calls.push(format!("delete {}", self.path));
        st.connections.remove(&self.path);
        Ok(())
    }

    async fn update2(
        &self,
        settings: SettingsMap,
        _flags: u32,
        _args: HashMap<String, OwnedValue>,
    ) -> fdo::Result<HashMap<String, OwnedValue>> {
        let mut st = self.shared.state.lock().unwrap();
        st.calls.push("Update2".into());
        st.connections.insert(self.path.clone(), settings);
        Ok(HashMap::new())
    }
}

// ---------------------------------------------------------------------
// Access point
// ---------------------------------------------------------------------

struct FakeAp {
    shared: Shared,
    path: String,
}

#[interface(name = "org.freedesktop.NetworkManager.AccessPoint")]
impl FakeAp {
    #[zbus(property)]
    fn ssid(&self) -> fdo::Result<Vec<u8>> {
        Ok(self.shared.state.lock().unwrap().aps[&self.path].ssid.clone())
    }

    #[zbus(property)]
    fn strength(&self) -> fdo::Result<u8> {
        Ok(self.shared.state.lock().unwrap().aps[&self.path].strength)
    }

    #[zbus(property)]
    fn flags(&self) -> fdo::Result<u32> {
        Ok(self.shared.state.lock().unwrap().aps[&self.path].flags)
    }

    #[zbus(property)]
    fn wpa_flags(&self) -> fdo::Result<u32> {
        Ok(self.shared.state.lock().unwrap().aps[&self.path].wpa)
    }

    #[zbus(property)]
    fn rsn_flags(&self) -> fdo::Result<u32> {
        Ok(self.shared.state.lock().unwrap().aps[&self.path].rsn)
    }
}

// ---------------------------------------------------------------------
// Device + Wireless + IP4Config
// ---------------------------------------------------------------------

struct FakeDevice {
    shared: Shared,
    path: String,
}

#[interface(name = "org.freedesktop.NetworkManager.Device")]
impl FakeDevice {
    #[zbus(property)]
    fn interface(&self) -> fdo::Result<String> {
        Ok(self.shared.state.lock().unwrap().devices[&self.path].iface.clone())
    }

    #[zbus(property)]
    fn device_type(&self) -> fdo::Result<u32> {
        Ok(self.shared.state.lock().unwrap().devices[&self.path].dev_type)
    }

    #[zbus(property)]
    fn state(&self) -> fdo::Result<u32> {
        Ok(self.shared.state.lock().unwrap().devices[&self.path].state)
    }

    #[zbus(property)]
    fn active_connection(&self) -> fdo::Result<OwnedObjectPath> {
        let st = self.shared.state.lock().unwrap();
        Ok(st
            .dev_active
            .get(&self.path)
            .map(|p| obj(p))
            .unwrap_or_else(|| obj("/")))
    }

    #[zbus(property)]
    fn ip4_config(&self) -> fdo::Result<OwnedObjectPath> {
        let st = self.shared.state.lock().unwrap();
        Ok(obj(&st.devices[&self.path].ip4))
    }

    async fn reapply(
        &self,
        _settings: SettingsMap,
        _version: u64,
        _flags: u32,
    ) -> fdo::Result<()> {
        let mut st = self.shared.state.lock().unwrap();
        st.calls.push("Reapply".into());
        Ok(())
    }
}

struct FakeWireless {
    shared: Shared,
    path: String,
}

#[interface(name = "org.freedesktop.NetworkManager.Device.Wireless")]
impl FakeWireless {
    #[zbus(property)]
    fn active_access_point(&self) -> fdo::Result<OwnedObjectPath> {
        let st = self.shared.state.lock().unwrap();
        Ok(st.devices[&self.path]
            .active_ap
            .as_ref()
            .map(|p| obj(p))
            .unwrap_or_else(|| obj("/")))
    }

    async fn get_all_access_points(&self) -> fdo::Result<Vec<OwnedObjectPath>> {
        let st = self.shared.state.lock().unwrap();
        Ok(st
            .dev_aps
            .get(&self.path)
            .map(|aps| aps.iter().map(|p| obj(p)).collect())
            .unwrap_or_default())
    }

    async fn request_scan(&self, _options: HashMap<String, OwnedValue>) -> fdo::Result<()> {
        let mut st = self.shared.state.lock().unwrap();
        st.calls.push("RequestScan".into());
        Ok(())
    }
}

struct FakeIp4 {
    shared: Shared,
    /// Device path (so the fake can find the device's IP).
    dev: String,
}

#[interface(name = "org.freedesktop.NetworkManager.IP4Config")]
impl FakeIp4 {
    #[zbus(property)]
    fn addresses(&self) -> fdo::Result<Vec<(u32, u32, u32)>> {
        let st = self.shared.state.lock().unwrap();
        // A fixed, recognizable address: 192.168.1.42/24, gw .1.
        let _ = &st.devices[&self.dev];
        Ok(vec![(0xC0A8012A, 24, 0xC0A80101)])
    }
}

// ---------------------------------------------------------------------
// Connection.Active
// ---------------------------------------------------------------------

struct FakeActive {
    shared: Shared,
    path: String,
}

#[interface(name = "org.freedesktop.NetworkManager.Connection.Active")]
impl FakeActive {
    #[zbus(property)]
    fn connection(&self) -> fdo::Result<OwnedObjectPath> {
        let st = self.shared.state.lock().unwrap();
        Ok(obj(&st.active_conns[&self.path]))
    }
}

// ---------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------

/// A private `dbus-daemon`. Dropping it kills the daemon.
pub struct Daemon {
    pub addr: String,
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

static DBUS_COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Start a private `dbus-daemon` with permissive policies. Nothing is
/// registered on it — attach the fake NM with [`serve_on`] if the test
/// needs NetworkManager to be present.
pub fn launch_daemon() -> Daemon {
    let n = DBUS_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "breadcrumbs-dbus-{}-{}",
        std::process::id(),
        n
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create dbus dir");
    let config = dir.join("bus.conf");
    std::fs::write(&config, BUS_CONFIG).expect("write bus config");

    let mut child = Command::new("dbus-daemon")
        .arg("--nofork")
        .arg("--nopidfile")
        .arg(format!("--config-file={}", config.display()))
        .arg("--print-address=1")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("dbus-daemon must be installed to run the NetworkManager fake tests");
    let mut line = String::new();
    std::io::BufReader::new(child.stdout.take().expect("dbus stdout"))
        .read_line(&mut line)
        .expect("read dbus-daemon address");
    let addr = line
        .trim()
        .split(';')
        .next()
        .expect("address")
        .to_string();
    Daemon { addr, child }
}

/// A fake NM service served on an existing (usually private) bus.
pub struct FakeNmBus {
    pub addr: String,
    pub state: Arc<Mutex<FakeState>>,
    server: zbus::blocking::Connection,
}

const BUS_CONFIG: &str = r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:tmpdir=/tmp</listen>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#;

/// Serve the fake NetworkManager on the bus at `addr` and claim its name.
/// Subprocesses pointed at the same address via `DBUS_SYSTEM_BUS_ADDRESS`
/// will see this as their system NetworkManager.
pub fn serve_on(addr: &str) -> FakeNmBus {
    let conn = zbus::blocking::connection::Builder::address(addr)
        .expect("connect to private bus")
        .build()
        .expect("build blocking connection");
    let state = Arc::new(Mutex::new(FakeState {
        connectivity: 4,
        wireless_enabled: true,
        next_dev: 1,
        next_ap: 1,
        next_conn: 1,
        next_active: 1,
        ..Default::default()
    }));
    let shared = Shared {
        state: state.clone(),
    };

    conn.object_server()
        .at(NM_PATH, FakeNm {
            shared: shared.clone(),
        })
        .expect("register fake NM");
    conn.object_server()
        .at(SETTINGS_PATH, FakeSettings {
            shared: shared.clone(),
        })
        .expect("register fake settings");
    conn.request_name(NM_DEST).expect("claim NM name");

    FakeNmBus {
        addr: addr.to_string(),
        state,
        server: conn,
    }
}

impl FakeNmBus {
    pub fn addr(&self) -> &str {
        &self.addr
    }

    pub fn reset(&self) {
        let mut st = self.state.lock().unwrap();
        st.devices.clear();
        st.aps.clear();
        st.dev_aps.clear();
        st.connections.clear();
        st.active_conns.clear();
        st.dev_active.clear();
        st.connectivity = 4;
        st.wireless_enabled = true;
        st.land_on = None;
        st.connect_any = false;
        st.fail_next_activations = 0;
        st.calls.clear();
        st.next_dev = 1;
        st.next_ap = 1;
        st.next_conn = 1;
        st.next_active = 1;
    }

    /// Add a Wi-Fi device; returns its object path. `state` is the device
    /// state (e.g. 100 = ACTIVATED).
    pub fn add_wifi_device(&self, iface: &str, state: u32) -> String {
        let (dev_path, ip4_path) = {
            let mut st = self.state.lock().unwrap();
            let id = st.next_dev;
            st.next_dev += 1;
            let dev_path = format!("{DEVICES_PREFIX}/{id}");
            let ip4_path = format!("{IP4_PREFIX}/{id}");
            st.devices.insert(
                dev_path.clone(),
                FakeDeviceData {
                    iface: iface.to_string(),
                    dev_type: DEV_TYPE_WIFI,
                    state,
                    active_ap: None,
                    ip4: ip4_path.clone(),
                },
            );
            (dev_path, ip4_path)
        };
        self.server
            .object_server()
            .at(
                dev_path.as_str(),
                FakeDevice {
                    shared: self.shared(),
                    path: dev_path.clone(),
                },
            )
            .expect("register device");
        self.server
            .object_server()
            .at(
                dev_path.as_str(),
                FakeWireless {
                    shared: self.shared(),
                    path: dev_path.clone(),
                },
            )
            .expect("register wireless");
        self.server
            .object_server()
            .at(
                ip4_path.as_str(),
                FakeIp4 {
                    shared: self.shared(),
                    dev: dev_path.clone(),
                },
            )
            .expect("register ip4");
        dev_path
    }

    fn shared(&self) -> Shared {
        Shared {
            state: self.state.clone(),
        }
    }

    /// Add an access point to a device; returns its object path.
    pub fn add_ap(&self, dev: &str, ssid: &str, strength: u8, sec: Security) -> String {
        let (ap_path, flags, wpa, rsn) = {
            let mut st = self.state.lock().unwrap();
            let id = st.next_ap;
            st.next_ap += 1;
            let ap_path = format!("{APS_PREFIX}/{id}");
            let (flags, wpa, rsn) = sec.to_flags();
            st.aps.insert(
                ap_path.clone(),
                FakeApData {
                    ssid: ssid.as_bytes().to_vec(),
                    strength,
                    flags,
                    wpa,
                    rsn,
                },
            );
            st.dev_aps.entry(dev.to_string()).or_default().push(ap_path.clone());
            (ap_path, flags, wpa, rsn)
        };
        let _ = (flags, wpa, rsn);
        self.server
            .object_server()
            .at(
                ap_path.as_str(),
                FakeAp {
                    shared: self.shared(),
                    path: ap_path.clone(),
                },
            )
            .expect("register AP");
        ap_path
    }

    pub fn set_active_ap(&self, dev: &str, ap: &str) {
        let mut st = self.state.lock().unwrap();
        if let Some(d) = st.devices.get_mut(dev) {
            d.active_ap = Some(ap.to_string());
            d.state = DEV_STATE_ACTIVATED;
        }
    }

    /// When set, any activation lands the device on the AP with this SSID
    /// (simulating an NM autoconnect race).
    pub fn set_land_on(&self, ssid: Option<&str>) {
        self.state.lock().unwrap().land_on = ssid.map(str::to_string);
    }

    /// When enabled, connecting to a network with no visible AP creates one
    /// (hidden-network semantics).
    pub fn set_connect_any(&self, on: bool) {
        self.state.lock().unwrap().connect_any = on;
    }

    /// Make the next `n` activation attempts fail (transient-failure
    /// simulation, e.g. for `init --wait` retries).
    pub fn fail_next_activations(&self, n: u32) {
        self.state.lock().unwrap().fail_next_activations = n;
    }

    pub fn set_connectivity(&self, c: u32) {
        self.state.lock().unwrap().connectivity = c;
    }

    pub fn set_device_state(&self, dev: &str, state: u32) {
        let mut st = self.state.lock().unwrap();
        if let Some(d) = st.devices.get_mut(dev) {
            d.state = state;
        }
    }

    /// Save a wireless connection profile (as `nm` would create one) and
    /// return its path.
    pub fn save_connection(&self, ssid: &str, password: Option<&str>) -> String {
        let mut settings: SettingsMap = HashMap::new();
        let mut conn: HashMap<String, OwnedValue> = HashMap::new();
        conn.insert("id".into(), ov(Value::from(ssid.to_string())));
        conn.insert("type".into(), ov(Value::from("802-11-wireless")));
        conn.insert(
            "uuid".into(),
            ov(Value::from("00000000-0000-4000-8000-000000000001")),
        );
        settings.insert("connection".into(), conn);

        let mut wifi: HashMap<String, OwnedValue> = HashMap::new();
        wifi.insert("ssid".into(), ov(Value::from(ssid.as_bytes().to_vec())));
        wifi.insert("mode".into(), ov(Value::from("infrastructure")));
        settings.insert("802-11-wireless".into(), wifi);

        if let Some(pw) = password {
            let mut sec: HashMap<String, OwnedValue> = HashMap::new();
            sec.insert("key-mgmt".into(), ov(Value::from("wpa-psk")));
            sec.insert("psk".into(), ov(Value::from(pw.to_string())));
            settings.insert("802-11-wireless-security".into(), sec);
        }

        let mut ipv4: HashMap<String, OwnedValue> = HashMap::new();
        ipv4.insert("method".into(), ov(Value::from("auto")));
        settings.insert("ipv4".into(), ipv4);

        let path = {
            let mut st = self.state.lock().unwrap();
            st.calls.push(format!("save {ssid}"));
            let path = format!("{CONNS_PREFIX}/{}", st.next_conn);
            st.next_conn += 1;
            st.connections.insert(path.clone(), settings);
            path
        };
        self.server
            .object_server()
            .at(
                path.as_str(),
                FakeConn {
                    shared: self.shared(),
                    path: path.clone(),
                },
            )
            .expect("register saved connection");
        path
    }

    pub fn calls(&self) -> Vec<String> {
        self.state.lock().unwrap().calls.clone()
    }

    pub fn connection_count(&self) -> usize {
        self.state.lock().unwrap().connections.len()
    }

    pub fn device_state(&self, dev: &str) -> u32 {
        self.state.lock().unwrap().devices.get(dev).map(|d| d.state).unwrap_or(0)
    }

    pub fn active_ssid(&self, dev: &str) -> Option<String> {
        let st = self.state.lock().unwrap();
        let ap = st.devices.get(dev)?.active_ap.clone()?;
        st.aps.get(&ap).map(|a| String::from_utf8_lossy(&a.ssid).into_owned())
    }

    /// SSIDs of every connection activated so far, in activation order.
    pub fn activated_ssids(&self) -> Vec<String> {
        let st = self.state.lock().unwrap();
        st.active_conns
            .values()
            .filter_map(|p| st.connections.get(p).and_then(conn_ssid))
            .collect()
    }
}

// ---------------------------------------------------------------------
// Shared in-process bus (flow_watch tests)
// ---------------------------------------------------------------------

struct SharedBus {
    _daemon: Daemon,
    bus: FakeNmBus,
}

static SHARED: OnceLock<Mutex<SharedBus>> = OnceLock::new();

/// The process-wide fake NM bus for in-process tests. The env var
/// `DBUS_SYSTEM_BUS_ADDRESS` is pointed at it once, so the production
/// `nm` module (which uses `Connection::system()`) reaches it with zero
/// test seams. Tests using this must serialize against each other — the
/// returned guard holds the bus's lock for its whole lifetime.
pub struct SharedNm {
    guard: MutexGuard<'static, SharedBus>,
}

impl std::ops::Deref for SharedNm {
    type Target = FakeNmBus;
    fn deref(&self) -> &FakeNmBus {
        &self.guard.bus
    }
}

pub fn shared() -> SharedNm {
    let bus = SHARED.get_or_init(|| {
        let daemon = launch_daemon();
        let bus = serve_on(&daemon.addr);
        std::env::set_var("DBUS_SYSTEM_BUS_ADDRESS", &daemon.addr);
        Mutex::new(SharedBus { _daemon: daemon, bus })
    });
    let guard = bus.lock().unwrap_or_else(|e| e.into_inner());
    SharedNm { guard }
}

/// Convenience guard used by tests that only need the bus available (no
/// state control) — e.g. classify tests that merely observe "no adapter".
/// Ensures the shared bus is up (and the env var set) before any `nm`
/// call happens.
pub fn ensure_shared() -> SharedNm {
    shared()
}
