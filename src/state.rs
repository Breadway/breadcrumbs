use std::fs;

use serde::{Deserialize, Serialize};

use crate::config::{state_dir, state_path, Config};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    pub profile: String,
    #[serde(default)]
    pub updated: String,
}

impl State {
    pub fn load(default_profile: &str) -> State {
        if let Ok(text) = fs::read_to_string(state_path()) {
            if let Ok(s) = toml::from_str::<State>(&text) {
                if !s.profile.is_empty() {
                    return s;
                }
            }
        }
        State {
            profile: default_profile.to_string(),
            updated: String::new(),
        }
    }

    pub fn save(&self) -> Result<(), String> {
        fs::create_dir_all(state_dir()).map_err(|e| format!("creating state dir: {e}"))?;
        let text = toml::to_string_pretty(self).map_err(|e| format!("serializing state: {e}"))?;
        fs::write(state_path(), text).map_err(|e| format!("writing state: {e}"))
    }
}

/// Persist `name` as the active profile if it exists in `cfg`. Shared by the
/// CLI `profile set` path and `bread.command.crumbs.set_profile` so they
/// cannot drift. Does not run [`crate::flow::run`] — the CLI applies
/// afterwards unless `--no-apply`, and the watch daemon picks the new
/// profile up on its next tick.
pub fn set_profile(cfg: &Config, name: &str) -> Result<(), String> {
    if !cfg.profiles.contains_key(name) {
        let avail: Vec<&String> = cfg.profiles.keys().collect();
        return Err(format!("unknown profile '{name}'. Available: {avail:?}"));
    }
    State {
        profile: name.to_string(),
        updated: crate::util::timestamp(),
    }
    .save()?;
    crate::notify::log(&format!("profile set -> {name}"));
    Ok(())
}
