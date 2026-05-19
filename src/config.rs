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
        fs::write(&path, text).map_err(|e| format!("writing {}: {e}", path.display()))?;
        // Plaintext Wi-Fi passwords live here — keep it owner-only.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
        }
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
}
