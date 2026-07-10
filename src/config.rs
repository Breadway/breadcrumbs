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
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkDef {
    pub ssid: String,
    pub password: String,
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
    #[serde(default, rename = "networks")]
    pub networks: Vec<NetworkDef>,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
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
            let cfg = build_initial_config();
            cfg.save()?;
            return Ok(cfg);
        }
        let text =
            fs::read_to_string(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
        let mut cfg: Config =
            toml::from_str(&text).map_err(|e| format!("parsing {}: {e}", path.display()))?;
        // Self-heal: guarantee the three core profiles always exist.
        ensure_core_profiles(&mut cfg);
        Ok(cfg)
    }

    pub fn save(&self) -> Result<(), String> {
        let dir = config_dir();
        fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
        let text = toml::to_string_pretty(self).map_err(|e| format!("serializing config: {e}"))?;
        let path = config_path();
        // Plaintext Wi-Fi passwords live here: write atomically and owner-only,
        // so there's no torn read and no world-readable window.
        crate::util::write_atomic(&path, &text, 0o600)
            .map_err(|e| format!("writing {}: {e}", path.display()))?;
        Ok(())
    }
}

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
    // Only self-heal a genuinely empty/corrupted profile set. A user who has
    // defined their own profiles (any names, any case) should never have
    // unused core-profile stubs ("home"/"work"/"away") silently padded in
    // alongside them.
    if !cfg.profiles.is_empty() {
        return;
    }
    for (name, prof) in core_profiles() {
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
    fn ensure_core_profiles_does_not_overwrite_existing() {
        let mut cfg = Config {
            settings: Settings::default(),
            networks: vec![],
            profiles: BTreeMap::new(),
        };
        cfg.profiles.insert(
            "home".to_string(),
            Profile {
                tailscale: true,
                exit_node: Some("mynode".into()),
                ..Default::default()
            },
        );
        ensure_core_profiles(&mut cfg);
        let home = cfg.profile("home").unwrap();
        assert!(home.tailscale, "existing field should be preserved");
        assert_eq!(home.exit_node.as_deref(), Some("mynode"));
    }

    #[test]
    fn ensure_core_profiles_does_not_pad_a_customized_profile_set() {
        let mut cfg = Config {
            settings: Settings::default(),
            networks: vec![],
            profiles: BTreeMap::new(),
        };
        cfg.profiles.insert("Home".to_string(), Profile::default());
        cfg.profiles.insert("Away".to_string(), Profile::default());
        cfg.profiles.insert("School".to_string(), Profile::default());
        ensure_core_profiles(&mut cfg);
        // The user's own profile names are untouched, and no unused
        // core-profile stubs (home/work/away) get injected alongside them.
        assert_eq!(cfg.profiles.len(), 3);
        assert!(cfg.profile("home").is_none());
        assert!(cfg.profile("work").is_none());
        assert!(cfg.profile("away").is_none());
    }

    #[test]
    fn network_lookup_found_and_not_found() {
        let mut cfg = build_initial_config();
        cfg.networks.push(NetworkDef {
            ssid: "TestNet".into(),
            password: "secret".into(),
            hidden: false,
        });
        let found = cfg.network("TestNet");
        assert!(found.is_some());
        assert_eq!(found.unwrap().password, "secret");
        assert!(cfg.network("NoSuchSSID").is_none());
    }

    #[test]
    fn profile_lookup_found_and_not_found() {
        let cfg = build_initial_config();
        assert!(cfg.profile("home").is_some());
        assert!(cfg.profile("nonexistent").is_none());
    }

    #[test]
    fn settings_default_values() {
        let s = Settings::default();
        assert_eq!(s.dns, "1.1.1.1");
        assert_eq!(s.nmcli_wait, 8);
        assert!(s.exit_node.is_empty());
        assert_eq!(s.default_profile, "away");
        assert_eq!(s.watch_interval, 12);
        assert!(!s.connectivity_url.is_empty());
        assert!(!s.ping_host.is_empty());
    }

    #[test]
    fn config_toml_roundtrip_with_hidden_network() {
        let mut cfg = build_initial_config();
        cfg.networks.push(NetworkDef {
            ssid: "HiddenNet".into(),
            password: "pw".into(),
            hidden: true,
        });
        cfg.networks.push(NetworkDef {
            ssid: "VisibleNet".into(),
            password: "pw2".into(),
            hidden: false,
        });
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.networks.len(), 2);
        let hidden = back.network("HiddenNet").unwrap();
        assert!(hidden.hidden);
        let visible = back.network("VisibleNet").unwrap();
        assert!(!visible.hidden);
    }

    #[test]
    fn config_toml_roundtrip_with_full_profile_fields() {
        let mut cfg = build_initial_config();
        let work = cfg.profiles.get_mut("work").unwrap();
        work.tailscale = true;
        work.exit_node = Some("myexit".into());
        work.bootstrap = Some("BootstrapSSID".into());
        work.detect_ssids = vec!["WorkWifi".into(), "CorpGuest".into()];
        work.networks = vec!["WorkWifi".into()];
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        let w = back.profile("work").unwrap();
        assert!(w.tailscale);
        assert_eq!(w.exit_node.as_deref(), Some("myexit"));
        assert_eq!(w.bootstrap.as_deref(), Some("BootstrapSSID"));
        assert_eq!(w.detect_ssids, vec!["WorkWifi", "CorpGuest"]);
        assert_eq!(w.networks, vec!["WorkWifi"]);
    }

    #[test]
    fn config_deserialization_applies_settings_defaults_for_missing_fields() {
        let toml_str = r#"
[settings]
dns = "8.8.8.8"
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.settings.dns, "8.8.8.8");
        // Fields not specified should get their defaults.
        assert_eq!(cfg.settings.nmcli_wait, 8);
        assert_eq!(cfg.settings.default_profile, "away");
        assert_eq!(cfg.settings.watch_interval, 12);
    }

    #[test]
    fn network_def_hidden_defaults_to_false() {
        let toml_str = r#"
[[networks]]
ssid = "MyNet"
password = "pass"
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert!(!cfg.networks[0].hidden);
    }
}
