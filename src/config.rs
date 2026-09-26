use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::util::home_dir;

fn default_dns() -> String {
    "1.1.1.1".to_string()
}
fn default_connect_wait() -> u32 {
    8
}
fn default_exit_node() -> String {
    String::new()
}
fn default_profile_name() -> String {
    "away".to_string()
}
fn default_watch_interval() -> u64 {
    12
}
fn default_connectivity_url() -> String {
    "http://connectivitycheck.gstatic.com/generate_204".to_string()
}
fn default_ping_host() -> String {
    "1.1.1.1".to_string()
}

/// Parse "HH:MM" (24h) into minutes since midnight; `None` if malformed.
pub fn hhmm_to_minutes(s: &str) -> Option<u32> {
    let (h, m) = s.trim().split_once(':')?;
    let h: u32 = h.parse().ok()?;
    let m: u32 = m.parse().ok()?;
    if h > 23 || m > 59 {
        return None;
    }
    Some(h * 60 + m)
}

/// Does the window `[from, to)` (minutes since midnight) contain `now`?
/// `from > to` means an overnight window (e.g. 22:00–07:00); `from == to`
/// means the whole day.
pub fn window_contains(from: u32, to: u32, now: u32) -> bool {
    if from < to {
        now >= from && now < to
    } else {
        now >= from || now < to
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScheduleEntry {
    /// Profile to switch to while the window is active.
    pub profile: String,
    /// "HH:MM", inclusive start.
    pub from: String,
    /// "HH:MM", exclusive end (`from > to` means overnight, `from == to`
    /// means all day).
    pub to: String,
}

impl ScheduleEntry {
    /// Whether the window contains `now_minutes` (minutes since midnight).
    pub fn contains(&self, now_minutes: u32) -> bool {
        match (hhmm_to_minutes(&self.from), hhmm_to_minutes(&self.to)) {
            (Some(f), Some(t)) => window_contains(f, t, now_minutes),
            _ => false,
        }
    }
}

fn is_false(b: &bool) -> bool {
    !b
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "default_dns")]
    pub dns: String,
    /// Seconds to wait for a connect to reach the ACTIVATED device state.
    /// `nmcli_wait` is accepted as a legacy alias.
    #[serde(default = "default_connect_wait", alias = "nmcli_wait")]
    pub connect_wait: u32,
    #[serde(default = "default_exit_node")]
    pub exit_node: String,
    #[serde(default = "default_profile_name")]
    pub default_profile: String,
    #[serde(default = "default_watch_interval")]
    pub watch_interval: u64,
    #[serde(default = "default_connectivity_url")]
    pub connectivity_url: String,
    #[serde(default = "default_ping_host")]
    pub ping_host: String,
    /// Set the first time the config is saved. Core profiles (`home` /
    /// `work` / `away`) are only backfilled for genuinely fresh or legacy
    /// configs; once the user owns the file, a profile they deliberately
    /// deleted stays deleted instead of being silently resurrected on the
    /// next load. Omits itself from the TOML until set, so existing
    /// configs keep parsing exactly as before.
    #[serde(default, skip_serializing_if = "is_false")]
    pub core_profiles_initialized: bool,
    /// Preferred Wi-Fi interface (e.g. "wlan0"). When set, this exact
    /// device is used if present; otherwise the first Wi-Fi device wins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface: Option<String>,
    /// Priority-ordered fallback exit nodes. Tried in order by the flow;
    /// the first healthy one is selected. Falls back to `exit_node` when
    /// empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exit_nodes: Vec<String>,
    /// Optional time-of-day schedule: at a given time, switch to the listed
    /// profile automatically (respecting a manual-override grace window;
    /// see the watch loop). First matching rule wins; outside every window
    /// nothing is switched.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub schedule: Vec<ScheduleEntry>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            dns: default_dns(),
            connect_wait: default_connect_wait(),
            exit_node: default_exit_node(),
            default_profile: default_profile_name(),
            watch_interval: default_watch_interval(),
            connectivity_url: default_connectivity_url(),
            ping_host: default_ping_host(),
            core_profiles_initialized: false,
            interface: None,
            exit_nodes: Vec::new(),
            schedule: Vec::new(),
        }
    }
}

impl Settings {
    /// The profile a time-of-day schedule picks for `now_minutes` (minutes
    /// since midnight), if any; first matching rule wins.
    pub fn scheduled_profile(&self, now_minutes: u32) -> Option<String> {
        self.schedule
            .iter()
            .find(|e| e.contains(now_minutes))
            .map(|e| e.profile.clone())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NetworkDef {
    pub ssid: String,
    /// A password is only needed once. On first successful connect,
    /// NetworkManager durably saves the credential (a new connection
    /// profile, or an updated PSK on an existing one); breadcrumbs then
    /// clears this field and, on the next save, omits the key entirely
    /// rather than writing a plaintext copy that's no longer needed. `None`
    /// means either "NetworkManager already owns this secret" or "this is
    /// an open (unsecured) network": both cases behave the same way on
    /// connect: no secret is sent to NetworkManager at all. When `Some`,
    /// the secret travels inside a D-Bus settings payload, never on argv.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// Per-network DNS override. `None` falls back to `settings.dns`;
    /// an explicitly empty string disables DNS pinning for this network
    /// (and removes a pin breadcrumbs applied earlier).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dns: Option<String>,
    /// WPA-Enterprise (802.1x). When `eap` is set the network is treated as
    /// enterprise: `identity` + `password` (reused) + optional `ca_cert`
    /// path. `eap` is "peap", "ttls" or "tls".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eap: Option<String>,
    /// 802.1x identity (e.g. `user@corp`) for enterprise networks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    /// Path to a CA certificate for 802.1x (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_cert: Option<String>,
    /// Inner (phase 2) authentication for PEAP/TTLS, e.g. "mschapv2"
    /// (the default when unset) or "pap".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase2: Option<String>,
    /// Client certificate path for EAP-TLS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_cert: Option<String>,
    /// Private key path for EAP-TLS. `password` is used as the key's
    /// passphrase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_key: Option<String>,
    #[serde(default)]
    pub hidden: bool,
}

impl NetworkDef {
    /// The DNS to pin for this network: the per-network override if set,
    /// otherwise the global setting.
    pub fn effective_dns<'a>(&'a self, fallback: &'a str) -> &'a str {
        self.dns.as_deref().unwrap_or(fallback)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Profile {
    /// Optional SSID connected first to bootstrap connectivity (e.g. for Tailscale).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bootstrap: Option<String>,
    /// Ordered priority list of SSIDs this profile should end up connected to.
    #[serde(default)]
    pub networks: Vec<String>,
    /// Require a healthy Tailscale + exit node before moving off the bootstrap.
    #[serde(default)]
    pub tailscale: bool,
    /// Per-profile exit node override (falls back to settings.exit_node).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_node: Option<String>,
    /// After the explicit list, also try every other known network.
    #[serde(default)]
    pub include_all_known: bool,
    /// SSIDs whose presence in a scan indicates this location.
    /// Used by `breadcrumbs detect` to guess the active profile.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub detect_ssids: Vec<String>,
    /// Opt-in learning: on a successful connect, the SSID is appended to
    /// `detect_ssids` (bounded) so `breadcrumbs detect` improves without
    /// hand-editing. Off by default to keep detect predictable.
    #[serde(default, skip_serializing_if = "is_false")]
    pub learn: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub settings: Settings,
    /// Saved networks (SSID + optional local password). Persisted to the
    /// separate `networks.toml` file (see [`networks_path`]), not to
    /// `breadcrumbs.toml`, kept here, and still deserialized from a
    /// `[[networks]]` block if one is present in `breadcrumbs.toml`, purely
    /// for backward compatibility with configs written before the secrets
    /// split: an old-format file's inline networks load in on first read and
    /// migrate to `networks.toml` automatically on the next `save()`, no
    /// explicit migration step required.
    #[serde(default, rename = "networks", skip_serializing)]
    pub networks: Vec<NetworkDef>,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}

/// The on-disk shape of `networks.toml`: just the `[[networks]]` array,
/// split out of the main config so a file that's mostly just settings and
/// profiles (the parts people actually hand-edit or dotfile) doesn't also
/// carry whatever plaintext Wi-Fi credentials breadcrumbs still holds.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct NetworksFile {
    #[serde(default, rename = "networks")]
    networks: Vec<NetworkDef>,
}

pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".config"))
        .join("breadcrumbs")
}

pub fn config_path() -> PathBuf {
    config_dir().join("breadcrumbs.toml")
}

/// Where saved networks (SSID + optional local password) live, split out of
/// `breadcrumbs.toml`. Not meant to be hand-edited; managed via
/// `breadcrumbs add` / `scan` / `forget`.
pub fn networks_path() -> PathBuf {
    config_dir().join("networks.toml")
}

pub fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".local").join("state"))
        .join("breadcrumbs")
}

pub fn state_path() -> PathBuf {
    state_dir().join("state.toml")
}

pub fn log_path() -> PathBuf {
    state_dir().join("breadcrumbs.log")
}

impl Config {
    pub fn profile<'a>(&'a self, name: &str) -> Option<&'a Profile> {
        self.profiles.get(name)
    }

    pub fn network<'a>(&'a self, ssid: &str) -> Option<&'a NetworkDef> {
        self.networks.iter().find(|n| n.ssid == ssid)
    }

    /// The effective exit-node list for a profile, in priority order:
    /// per-profile `exit_node`, else `settings.exit_nodes`, else
    /// `settings.exit_node`. Empty entries are filtered out.
    pub fn exit_nodes_for(&self, profile: &str) -> Vec<String> {
        if let Some(p) = self.profiles.get(profile).and_then(|p| p.exit_node.clone()) {
            return vec![p];
        }
        let list = if self.settings.exit_nodes.is_empty() {
            vec![self.settings.exit_node.clone()]
        } else {
            self.settings.exit_nodes.clone()
        };
        list.into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// Load config, creating a skeleton one on first run.
    pub fn load() -> Result<Config, String> {
        let path = config_path();
        if !path.exists() {
            let mut cfg = build_initial_config();
            cfg.save()?;
            return Ok(cfg);
        }
        let text =
            fs::read_to_string(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
        // May carry a legacy inline `[[networks]]` block (pre-split
        // configs); that's fine, see the field doc on `Config::networks`.
        let mut cfg: Config =
            toml::from_str(&text).map_err(|e| format!("parsing {}: {e}", path.display()))?;

        let net_path = networks_path();
        if net_path.exists() {
            let net_text = fs::read_to_string(&net_path)
                .map_err(|e| format!("reading {}: {e}", net_path.display()))?;
            let nf: NetworksFile = toml::from_str(&net_text)
                .map_err(|e| format!("parsing {}: {e}", net_path.display()))?;
            // Merge, don't overwrite: a legacy config can still carry an
            // inline `[[networks]]` block, and those entries must survive
            // even when networks.toml already exists: otherwise the next
            // save() (which writes only networks.toml) would silently drop
            // hand-added inline networks. networks.toml wins on SSID
            // conflicts; inline-only entries are appended and migrated.
            let mut merged = nf.networks;
            for def in std::mem::take(&mut cfg.networks) {
                if !merged.iter().any(|n| n.ssid == def.ssid) {
                    merged.push(def);
                }
            }
            cfg.networks = merged;
        }
        // else: no networks.toml yet, keep whatever legacy inline networks
        // were read from breadcrumbs.toml above (or none, on a genuinely
        // fresh config). The next `save()` writes them to networks.toml and
        // stops writing them into breadcrumbs.toml, completing the migration.

        ensure_core_profiles(&mut cfg);
        Ok(cfg)
    }

    /// Persist settings + profiles to `breadcrumbs.toml` and networks to the
    /// separate `networks.toml`, both `0600`. Every mutating path goes
    /// through this single method so the two files never drift out of sync
    /// with each other.
    ///
    /// This is the raw write: it takes no lock. Code that mutates config
    /// concurrently with other breadcrumbs processes (the watch daemon,
    /// one-shot commands) must go through [`Config::update`], which wraps a
    /// fresh load, the change and this save in the config-dir lock.
    pub fn save(&mut self) -> Result<(), String> {
        // The first save marks the config as user-owned: core profiles are
        // backfilled only for genuinely fresh/legacy configs, never
        // resurrected after the user has edited (or deleted) them.
        self.settings.core_profiles_initialized = true;

        let dir = config_dir();
        fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;

        // Networks first: during a legacy migration the inline networks are
        // dropped from breadcrumbs.toml by the second write, so they must
        // already be safely in networks.toml if that write succeeds and the
        // process dies (or the disk fills) right after.
        //
        // Both writes are atomic (temp file then rename) so a crash
        // mid-write, or a concurrent reader, never sees a truncated file.
        // `0o600` is set on the temp file up front so any still-plaintext
        // Wi-Fi password is never briefly world-readable.
        let nf = NetworksFile {
            networks: self.networks.clone(),
        };
        let net_text =
            toml::to_string_pretty(&nf).map_err(|e| format!("serializing networks: {e}"))?;
        let net_path = networks_path();
        bread_utils::atomic::write_atomic(&net_path, &net_text, Some(0o600))
            .map_err(|e| format!("writing {}: {e}", net_path.display()))?;

        let fresh = toml::to_string_pretty(self).map_err(|e| format!("serializing config: {e}"))?;
        let path = config_path();
        // breadcrumbs.toml is the hand-edited file: patch the existing
        // document in place so the user's comments and layout survive a
        // daemon-initiated save (clearing a password, learning an SSID).
        let text = match fs::read_to_string(&path) {
            Ok(existing) => preserve_layout(&existing, &fresh).unwrap_or(fresh),
            Err(_) => fresh,
        };
        bread_utils::atomic::write_atomic(&path, &text, Some(0o600))
            .map_err(|e| format!("writing {}: {e}", path.display()))?;

        Ok(())
    }

    /// Load the current config, apply `f`, and save it, all while holding
    /// the config-dir lock, so concurrent load, modify, save cycles (the
    /// watch daemon, `add`, `forget`, ...) queue instead of one silently
    /// dropping another's change (CRUMB-10). The lock is held only for this
    /// short cycle, never across a prompt, a connect or a network probe.
    ///
    /// Returns the freshly saved config. Must not be called while this
    /// process already holds the config lock (`flock` is per open file, so
    /// a second acquire in the same process would deadlock).
    pub fn update(f: impl FnOnce(&mut Config) -> Result<(), String>) -> Result<Config, String> {
        let _lock = crate::lock::acquire(&config_dir())
            .map_err(|e| format!("could not lock the config directory: {e}"))?;
        let mut cfg = Config::load()?;
        f(&mut cfg)?;
        cfg.save()?;
        Ok(cfg)
    }

    /// Human-readable problems with the config that would make automation
    /// silently do nothing: dangling profile / network references,
    /// malformed schedule windows, a Tailscale profile with no exit node.
    /// Reported by `doctor`.
    pub fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.profiles.contains_key(&self.settings.default_profile) {
            out.push(format!(
                "settings.default_profile '{}' is not a defined profile",
                self.settings.default_profile
            ));
        }
        for (i, e) in self.settings.schedule.iter().enumerate() {
            if !self.profiles.contains_key(&e.profile) {
                out.push(format!(
                    "schedule[{i}] targets unknown profile '{}'",
                    e.profile
                ));
            }
            for (field, v) in [("from", &e.from), ("to", &e.to)] {
                if hhmm_to_minutes(v).is_none() {
                    out.push(format!(
                        "schedule[{i}].{field} '{v}' is not a valid HH:MM time"
                    ));
                }
            }
        }
        for (name, p) in &self.profiles {
            for ssid in p.networks.iter().chain(p.bootstrap.iter()) {
                if self.network(ssid).is_none() {
                    out.push(format!(
                        "profile '{name}' references '{ssid}', which is not a saved network"
                    ));
                }
            }
            if p.tailscale && self.exit_nodes_for(name).is_empty() {
                out.push(format!(
                    "profile '{name}' requires Tailscale but no exit node is configured"
                ));
            }
        }
        out
    }
}

impl Settings {
    /// The watch loop's poll interval, clamped to the documented minimum of
    /// 4s. Clamped on use rather than rewritten at load, so a save never
    /// silently changes the value the user wrote.
    pub fn poll_interval(&self) -> u64 {
        self.watch_interval.max(4)
    }
}

/// Re-render `fresh` (the serialized config) onto the layout of `existing`
/// (what is on disk): values are updated in place, keeping comments and
/// formatting attached to them; new keys are appended; keys no longer
/// present are removed. `None` if `existing` does not parse, in which case
/// the caller writes `fresh` as is.
fn preserve_layout(existing: &str, fresh: &str) -> Option<String> {
    let mut doc: toml_edit::DocumentMut = existing.parse().ok()?;
    let new: toml_edit::DocumentMut = fresh.parse().ok()?;
    merge_table(doc.as_table_mut(), new.as_table());
    Some(doc.to_string())
}

fn merge_table(old: &mut toml_edit::Table, new: &toml_edit::Table) {
    use toml_edit::Item;
    let stale: Vec<String> = old
        .iter()
        .map(|(k, _)| k.to_string())
        .filter(|k| !new.contains_key(k))
        .collect();
    for k in stale {
        old.remove(&k);
    }
    for (k, nv) in new.iter() {
        match (old.get_mut(k), nv) {
            (Some(Item::Table(ot)), Item::Table(nt)) => merge_table(ot, nt),
            (Some(Item::Value(ov)), Item::Value(nval)) => {
                if ov.to_string().trim() != nval.to_string().trim() {
                    let decor = ov.decor().clone();
                    *ov = nval.clone();
                    *ov.decor_mut() = decor;
                }
            }
            (Some(slot), _) => *slot = nv.clone(),
            (None, _) => {
                old.insert(k, nv.clone());
            }
        }
    }
}

/// Initial skeleton networks generated for a brand-new installation.
/// Passwords are intentionally blank: secrets never live in source.
/// Users fill them via `breadcrumbs add`, `breadcrumbs scan`, or
/// `breadcrumbs edit`, or by copying `breadcrumbs.example.toml`.
fn canonical_networks() -> Vec<NetworkDef> {
    Vec::new()
}

/// Starter profiles generated for a brand-new installation.
/// These give users working examples of the three common location patterns:
/// a home profile, a profile requiring Tailscale (e.g. a workplace or school),
/// and an "away" catch-all. All network lists start empty; users populate them
/// via `breadcrumbs add --to <profile>` or `breadcrumbs edit`.
fn core_profiles() -> BTreeMap<String, Profile> {
    let mut p = BTreeMap::new();
    p.insert(
        "home".to_string(),
        Profile {
            bootstrap: None,
            networks: vec![],
            tailscale: false,
            exit_node: None,
            include_all_known: false,
            detect_ssids: vec![],
            learn: false,
        },
    );
    p.insert(
        "work".to_string(),
        Profile {
            bootstrap: None,
            networks: vec![],
            tailscale: false,
            exit_node: None,
            include_all_known: false,
            detect_ssids: vec![],
            learn: false,
        },
    );
    p.insert(
        "away".to_string(),
        Profile {
            bootstrap: None,
            networks: vec![],
            tailscale: false,
            exit_node: None,
            include_all_known: true,
            detect_ssids: vec![],
            learn: false,
        },
    );
    p
}

fn ensure_core_profiles(cfg: &mut Config) {
    // Backfill missing core profiles only until the user has taken
    // ownership of the config (`core_profiles_initialized` is set by the
    // first save). After that, a profile the user deliberately deleted
    // stays deleted.
    if cfg.settings.core_profiles_initialized {
        return;
    }
    for (name, prof) in core_profiles() {
        // Case-insensitive: a user's "Home" already is the home profile,
        // so don't add a confusing lowercase twin next to it.
        if cfg.profiles.keys().any(|k| k.eq_ignore_ascii_case(&name)) {
            continue;
        }
        cfg.profiles.insert(name, prof);
    }
}

/// The config generated on first run: no networks, the three core profiles.
/// Users populate it via `breadcrumbs add` / `edit` / `scan`.
fn build_initial_config() -> Config {
    Config {
        settings: Settings::default(),
        networks: canonical_networks(),
        profiles: core_profiles(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_config_is_empty_with_core_profiles() {
        let cfg = build_initial_config();
        assert!(cfg.networks.is_empty());
        assert_eq!(cfg.profiles.len(), 3);
        assert!(cfg.profile("home").is_some());
        assert!(cfg.profile("work").is_some());
        assert!(cfg.profile("away").is_some());
        assert!(cfg.profile("away").unwrap().include_all_known);
    }

    #[test]
    fn config_toml_roundtrip() {
        let cfg = build_initial_config();
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.networks.len(), cfg.networks.len());
        assert_eq!(back.profiles.len(), 3);
        assert!(back.profile("home").is_some());
        assert!(back.profile("away").is_some());
    }

    #[test]
    fn ensure_core_profiles_backfills_missing() {
        let mut cfg = Config {
            settings: Settings::default(),
            networks: vec![],
            profiles: BTreeMap::new(),
        };
        ensure_core_profiles(&mut cfg);
        assert!(cfg.profile("home").is_some());
        assert!(cfg.profile("work").is_some());
        assert!(cfg.profile("away").is_some());
    }

    #[test]
    fn ensure_core_profiles_skips_backfill_once_initialized() {
        // After the first save the config is user-owned: a deliberately
        // deleted core profile must stay deleted instead of being
        // resurrected on every load.
        let mut cfg = Config {
            settings: Settings {
                core_profiles_initialized: true,
                ..Default::default()
            },
            networks: vec![],
            profiles: BTreeMap::new(),
        };
        ensure_core_profiles(&mut cfg);
        assert!(cfg.profiles.is_empty(), "no backfill once user-owned");
    }

    #[test]
    fn ensure_core_profiles_preserves_user_customized_core_profile() {
        // A user-edited "home" (custom SSIDs) must not be clobbered by the
        // self-heal backfill; only genuinely *missing* core profiles should
        // be inserted.
        let mut profiles = BTreeMap::new();
        profiles.insert(
            "home".to_string(),
            Profile {
                networks: vec!["CustomSSID".into()],
                ..Default::default()
            },
        );
        let mut cfg = Config {
            settings: Settings::default(),
            networks: vec![],
            profiles,
        };
        ensure_core_profiles(&mut cfg);
        assert_eq!(
            cfg.profile("home").unwrap().networks,
            vec!["CustomSSID".to_string()]
        );
        // Still backfills the ones that were actually missing.
        assert!(cfg.profile("work").is_some());
        assert!(cfg.profile("away").is_some());
    }

    #[test]
    fn settings_default_matches_documented_defaults() {
        let s = Settings::default();
        assert_eq!(s.dns, "1.1.1.1");
        assert_eq!(s.connect_wait, 8);
        assert_eq!(s.default_profile, "away");
        assert_eq!(s.watch_interval, 12);
        assert_eq!(s.ping_host, "1.1.1.1");
        assert!(s.exit_node.is_empty());
    }

    #[test]
    fn network_def_hidden_defaults_false_when_omitted() {
        let text = r#"ssid = "Cafe"
password = "pw""#;
        let n: NetworkDef = toml::from_str(text).unwrap();
        assert!(!n.hidden);
    }

    #[test]
    fn network_def_password_defaults_to_none_when_key_absent() {
        // No `password` key at all: e.g. a network whose secret breadcrumbs
        // already cleared after NetworkManager took it over.
        let text = r#"ssid = "Cafe"
hidden = false"#;
        let n: NetworkDef = toml::from_str(text).unwrap();
        assert_eq!(n.password, None);
    }

    #[test]
    fn network_def_omits_password_key_entirely_when_none() {
        // Round-tripping a cleared password must not write `password = ""`
        // (which would read back as "has an empty secret") or any other
        // stand-in; the key should be gone, full stop.
        let n = NetworkDef {
            ssid: "Cafe".into(),
            password: None,
            dns: None,
            eap: None,
            identity: None,
            ca_cert: None,
            hidden: false,
            ..Default::default()
        };
        let text = toml::to_string_pretty(&n).unwrap();
        assert!(!text.contains("password"), "text: {text}");
    }

    #[test]
    fn network_def_password_round_trips_through_toml() {
        let n = NetworkDef {
            ssid: "Cafe".into(),
            password: Some("hunter2".into()),
            dns: None,
            eap: None,
            identity: None,
            ca_cert: None,
            hidden: false,
            ..Default::default()
        };
        let text = toml::to_string_pretty(&n).unwrap();
        assert!(text.contains("hunter2"));
        let back: NetworkDef = toml::from_str(&text).unwrap();
        assert_eq!(back.password, Some("hunter2".to_string()));
    }

    // Note: `Config::save`/`Config::load`'s real filesystem behavior (the
    // networks.toml split, and a cleared password actually landing on disk)
    // is covered by `tests/cli.rs`'s Sandbox-isolated integration tests
    // (`networks_are_stored_separately_from_settings_and_profiles`,
    // `password_is_cleared_after_first_connect_and_never_sent_again`) rather
    // than here; this module's tests stay pure per the project's test
    // discipline (no real fs/env/subprocess access from a `#[cfg(test)]`
    // unit test).

    #[test]
    fn profile_default_has_no_bootstrap_or_tailscale() {
        let p = Profile::default();
        assert!(p.bootstrap.is_none());
        assert!(!p.tailscale);
        assert!(!p.include_all_known);
        assert!(p.networks.is_empty());
        assert!(p.detect_ssids.is_empty());
    }

    #[test]
    fn malformed_toml_fails_to_parse() {
        let bad = "this is not [ valid toml";
        assert!(toml::from_str::<Config>(bad).is_err());
    }

    #[test]
    fn config_with_only_settings_defaults_networks_and_profiles() {
        // A hand-written config that only sets `[settings]` shouldn't require
        // `networks`/`profiles` sections; both must fall back to `#[serde(default)]`.
        let text = r#"[settings]
dns = "9.9.9.9""#;
        let cfg: Config = toml::from_str(text).unwrap();
        assert_eq!(cfg.settings.dns, "9.9.9.9");
        assert!(cfg.networks.is_empty());
        assert!(cfg.profiles.is_empty());
    }

    #[test]
    fn exit_nodes_for_prioritizes_profile_then_list_then_single_node() {
        let mut cfg = build_initial_config();
        cfg.settings.exit_node = "global".into();
        cfg.settings.exit_nodes = vec!["listA".into(), "listB".into()];
        cfg.profiles.get_mut("home").unwrap().exit_node = Some("profile".into());

        // Per-profile override wins outright.
        assert_eq!(cfg.exit_nodes_for("home"), vec!["profile".to_string()]);

        // Otherwise the priority list is used verbatim.
        cfg.profiles.get_mut("home").unwrap().exit_node = None;
        assert_eq!(
            cfg.exit_nodes_for("home"),
            vec!["listA".to_string(), "listB".to_string()]
        );

        // Without a list, the single setting is the (one-element) fallback.
        cfg.settings.exit_nodes = vec![];
        assert_eq!(cfg.exit_nodes_for("home"), vec!["global".to_string()]);
    }

    #[test]
    fn exit_nodes_for_filters_empty_and_whitespace_entries() {
        let mut cfg = build_initial_config();
        cfg.settings.exit_nodes = vec!["  ".into(), "nodeA".into(), "".into(), " nodeB ".into()];
        assert_eq!(
            cfg.exit_nodes_for("home"),
            vec!["nodeA".to_string(), "nodeB".to_string()]
        );
    }

    #[test]
    fn hhmm_to_minutes_parses_and_rejects_malformed() {
        assert_eq!(hhmm_to_minutes("09:30"), Some(570));
        assert_eq!(hhmm_to_minutes("00:00"), Some(0));
        assert_eq!(hhmm_to_minutes("23:59"), Some(1439));
        assert_eq!(hhmm_to_minutes("9:30"), Some(570)); // lenient about padding
        assert_eq!(hhmm_to_minutes("24:00"), None); // hour out of range
        assert_eq!(hhmm_to_minutes("12:60"), None); // minute out of range
        assert_eq!(hhmm_to_minutes("0930"), None); // no colon
        assert_eq!(hhmm_to_minutes(""), None);
    }

    #[test]
    fn window_contains_handles_same_day_and_overnight() {
        // Same-day window 09:00–17:00 (end exclusive).
        assert!(window_contains(540, 1020, 600));
        assert!(!window_contains(540, 1020, 1020));
        assert!(!window_contains(540, 1020, 500));
        // Overnight window 22:00–07:00.
        assert!(window_contains(1320, 420, 1380)); // 23:00
        assert!(window_contains(1320, 420, 60)); // 01:00
        assert!(!window_contains(1320, 420, 720)); // 12:00
    }

    #[test]
    fn scheduled_profile_returns_first_matching_rule() {
        let mut cfg = build_initial_config();
        cfg.settings.schedule = vec![
            ScheduleEntry {
                profile: "work".into(),
                from: "09:00".into(),
                to: "17:00".into(),
            },
            ScheduleEntry {
                profile: "home".into(),
                from: "09:30".into(),
                to: "18:00".into(),
            },
        ];
        // 10:00 matches both; the first rule (work) wins.
        assert_eq!(cfg.settings.scheduled_profile(600), Some("work".into()));
        // Outside every window → no schedule applies.
        assert_eq!(cfg.settings.scheduled_profile(60), None);
    }

    #[test]
    fn effective_dns_uses_per_network_override_then_global_fallback() {
        let n = NetworkDef {
            ssid: "x".into(),
            password: None,
            dns: Some("9.9.9.9".into()),
            eap: None,
            identity: None,
            ca_cert: None,
            hidden: false,
            ..Default::default()
        };
        assert_eq!(n.effective_dns("1.1.1.1"), "9.9.9.9");

        let n2 = NetworkDef {
            dns: None,
            ..n.clone()
        };
        assert_eq!(n2.effective_dns("1.1.1.1"), "1.1.1.1");

        // An explicit empty string is a valid per-network opt-out.
        let n3 = NetworkDef {
            dns: Some(String::new()),
            ..n.clone()
        };
        assert_eq!(n3.effective_dns("1.1.1.1"), "");
    }

    #[test]
    fn enterprise_fields_round_trip_and_omit_when_none() {
        let n = NetworkDef {
            ssid: "Corp".into(),
            password: Some("pw".into()),
            dns: None,
            eap: Some("peap".into()),
            identity: Some("user@corp".into()),
            ca_cert: Some("/etc/ca.pem".into()),
            hidden: false,
            ..Default::default()
        };
        let text = toml::to_string_pretty(&n).unwrap();
        assert!(text.contains("eap") && text.contains("identity") && text.contains("ca_cert"));
        let back: NetworkDef = toml::from_str(&text).unwrap();
        assert_eq!(back.eap.as_deref(), Some("peap"));
        assert_eq!(back.identity.as_deref(), Some("user@corp"));
        assert_eq!(back.ca_cert.as_deref(), Some("/etc/ca.pem"));

        let plain = NetworkDef {
            eap: None,
            identity: None,
            ca_cert: None,
            ..n
        };
        let t2 = toml::to_string_pretty(&plain).unwrap();
        assert!(!t2.contains("eap") && !t2.contains("identity") && !t2.contains("ca_cert"));
    }
}
