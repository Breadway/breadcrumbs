use std::fs;

use serde::{Deserialize, Serialize};

use crate::config::{state_dir, state_path};

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
