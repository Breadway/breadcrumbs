use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::util::home_dir;

fn default_dns() -> String {
    "1.1.1.1".to_string()
}
fn default_nmcli_wait() -> u32 {
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

fn is_false(b: &bool) -> bool {
    !b
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "default_dns")]
    pub dns: String,
    #[serde(default = "default_nmcli_wait")]
    pub nmcli_wait: u32,
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
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            dns: default_dns(),
            nmcli_wait: default_nmcli_wait(),
            exit_node: default_exit_node(),
            default_profile: default_profile_name(),
            watch_interval: default_watch_interval(),
            connectivity_url: default_connectivity_url(),
            ping_host: default_ping_host(),
            core_profiles_initialized: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkDef {
    pub ssid: String,
    /// A password is only needed once. On first successful connect,
    /// NetworkManager durably saves the credential (a new connection
    /// profile, or an updated PSK on an existing one); breadcrumbs then
    /// clears this field and, on the next save, omits the key entirely
    /// rather than writing a plaintext copy that's no longer needed. `None`
    /// means either "NetworkManager already owns this secret" or "this is
    /// an open (unsecured) network" — both cases behave the same way on
    /// connect: no PSK is sent to nmcli at all. When `Some`, the PSK is
    /// fed to `nmcli --ask` on stdin, never as an argv element.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(default)]
    pub hidden: bool,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub settings: Settings,
    /// Saved networks (SSID + optional local password). Persisted to the
    /// separate `networks.toml` file (see [`networks_path`]), not to
    /// `breadcrumbs.toml` — kept here, and still deserialized from a
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
/// `breadcrumbs.toml`. Not meant to be hand-edited — managed via
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
        // configs) — that's fine, see the field doc on `Config::networks`.
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
            // even when networks.toml already exists — otherwise the next
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
        // else: no networks.toml yet — keep whatever legacy inline networks
        // were read from breadcrumbs.toml above (or none, on a genuinely
        // fresh config). The next `save()` writes them to networks.toml and
        // stops writing them into breadcrumbs.toml, completing the migration.

        // Enforce the documented minimum so `list` and the watch loop agree
        // on the poll interval (watch silently clamps to 4 otherwise).
        if cfg.settings.watch_interval < 4 {
            cfg.settings.watch_interval = 4;
        }

        ensure_core_profiles(&mut cfg);
        Ok(cfg)
    }

    /// Persist settings + profiles to `breadcrumbs.toml` and networks to the
    /// separate `networks.toml`, both `0600`. Every mutating command (`add`,
    /// `forget`, `scan`, `profile set`, and `flow::run`'s own credential
    /// clearing) goes through this single method so the two files never
    /// drift out of sync with each other.
    pub fn save(&mut self) -> Result<(), String> {
        // The first save marks the config as user-owned: core profiles are
        // backfilled only for genuinely fresh/legacy configs, never
        // resurrected after the user has edited (or deleted) them.
        self.settings.core_profiles_initialized = true;

        let dir = config_dir();
        fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;

        let text = toml::to_string_pretty(self).map_err(|e| format!("serializing config: {e}"))?;
        let path = config_path();
        fs::write(&path, text).map_err(|e| format!("writing {}: {e}", path.display()))?;
        secure_permissions(&path);

        let nf = NetworksFile {
            networks: self.networks.clone(),
        };
        let net_text =
            toml::to_string_pretty(&nf).map_err(|e| format!("serializing networks: {e}"))?;
        let net_path = networks_path();
        fs::write(&net_path, net_text)
            .map_err(|e| format!("writing {}: {e}", net_path.display()))?;
        secure_permissions(&net_path);

        Ok(())
    }
}

/// Any local Wi-Fi passwords still held (pre-first-connect, or a network
/// breadcrumbs doesn't yet know NetworkManager owns) live in plaintext on
/// disk — keep both config files owner-only. Best-effort: a failure here
/// isn't fatal to saving the config itself.
#[cfg(unix)]
fn secure_permissions(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}
#[cfg(not(unix))]
fn secure_permissions(_path: &std::path::Path) {}

/// Initial skeleton networks generated for a brand-new installation.
/// Passwords are intentionally blank — secrets never live in source.
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
        cfg.profiles.entry(name).or_insert(prof);
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
        // self-heal backfill — only genuinely *missing* core profiles should
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
        assert_eq!(s.nmcli_wait, 8);
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
        // No `password` key at all — e.g. a network whose secret breadcrumbs
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
        // stand-in — the key should be gone, full stop.
        let n = NetworkDef {
            ssid: "Cafe".into(),
            password: None,
            hidden: false,
        };
        let text = toml::to_string_pretty(&n).unwrap();
        assert!(!text.contains("password"), "text: {text}");
    }

    #[test]
    fn network_def_password_round_trips_through_toml() {
        let n = NetworkDef {
            ssid: "Cafe".into(),
            password: Some("hunter2".into()),
            hidden: false,
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
    // than here — this module's tests stay pure per the project's test
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
        // `networks`/`profiles` sections — both must fall back to `#[serde(default)]`.
        let text = r#"[settings]
dns = "9.9.9.9""#;
        let cfg: Config = toml::from_str(text).unwrap();
        assert_eq!(cfg.settings.dns, "9.9.9.9");
        assert!(cfg.networks.is_empty());
        assert!(cfg.profiles.is_empty());
    }
}
