use crate::config::{Config, NetworkDef};
use crate::nm;
use crate::notify::{log, notify, Urgency};
use crate::status::internet_ok;
use crate::tailscale::{self, TsHealth};

#[derive(Debug)]
pub enum Outcome {
    /// Connected to `ssid`; `note` carries any caveat (e.g. on bootstrap only).
    Connected {
        ssid: String,
        note: Option<String>,
    },
    /// Tailscale required but unhealthy; left on `ssid` (bootstrap if available).
    TailscaleError {
        ssid: Option<String>,
        health: TsHealth,
    },
    NoInterface,
    NoNetworks,
    UnknownProfile(String),
}

impl Outcome {
    pub fn ok(&self) -> bool {
        matches!(self, Outcome::Connected { .. })
    }
}

fn resolve_candidates<'a>(cfg: &'a Config, p: &crate::config::Profile) -> Vec<&'a NetworkDef> {
    let mut out: Vec<&NetworkDef> = Vec::new();
    for ssid in &p.networks {
        if let Some(def) = cfg.network(ssid) {
            if !out.iter().any(|d| d.ssid == def.ssid) {
                out.push(def);
            }
        }
    }
    if p.include_all_known {
        for def in &cfg.networks {
            if !out.iter().any(|d| d.ssid == def.ssid) {
                out.push(def);
            }
        }
    }
    out
}

/// Try to connect + confirm it actually carries traffic.
/// Returns Ok(()) on success, Err(reason) on failure.
fn connect_and_verify(iface: &str, def: &NetworkDef, cfg: &Config) -> Result<(), String> {
    nm::connect_verbose(iface, def, cfg.settings.nmcli_wait, &cfg.settings.dns)?;
    if !nm::device_connected(iface) {
        return Err("device not connected after nmcli success".into());
    }
    Ok(())
}

/// Run the connection state machine for `profile_name`.
pub fn run(cfg: &Config, profile_name: &str) -> Outcome {
    let profile = match cfg.profile(profile_name) {
        Some(p) => p.clone(),
        None => {
            notify(
                "breadcrumbs: unknown profile",
                &format!("'{profile_name}' is not defined in breadcrumbs.toml"),
                Urgency::Critical,
            );
            return Outcome::UnknownProfile(profile_name.to_string());
        }
    };

    let iface = match nm::wifi_interface() {
        Some(i) => i,
        None => {
            notify(
                "breadcrumbs: no Wi-Fi adapter",
                "Hardware issue — Wi-Fi device not found. Manual check needed.",
                Urgency::Critical,
            );
            return Outcome::NoInterface;
        }
    };
    nm::radio_on();

    let exit_node = profile
        .exit_node
        .clone()
        .unwrap_or_else(|| cfg.settings.exit_node.clone());
    let candidates = resolve_candidates(cfg, &profile);

    log(&format!(
        "flow start: profile={profile_name} iface={iface} tailscale={} candidates=[{}]",
        profile.tailscale,
        candidates
            .iter()
            .map(|c| c.ssid.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    ));

    // Pre-scan for everything we might want, including the bootstrap SSID.
    let mut scan_targets: Vec<String> = candidates.iter().map(|c| c.ssid.clone()).collect();
    if let Some(bs) = &profile.bootstrap {
        scan_targets.push(bs.clone());
    }
    nm::rescan(&iface, &scan_targets);
    let visible = nm::visible_ssids(&iface);

    // ---- Tailscale-gated profiles (e.g. school) -------------------------
    let mut on_bootstrap = false;
    if profile.tailscale {
        if let Some(bs_ssid) = profile.bootstrap.clone() {
            match cfg.network(&bs_ssid) {
                Some(bdef) => {
                    if visible.contains(&bdef.ssid) || bdef.hidden {
                        match connect_and_verify(&iface, bdef, cfg) {
                            Ok(()) => {
                                on_bootstrap = true;
                                log(&format!("bootstrap connected: {}", bdef.ssid));
                            }
                            Err(e) => {
                                log(&format!("bootstrap connect failed: {} — {e}", bdef.ssid))
                            }
                        }
                    } else {
                        log(&format!("bootstrap not in range: {}", bdef.ssid));
                    }
                }
                None => log(&format!(
                    "bootstrap SSID '{bs_ssid}' has no credentials in config"
                )),
            }
        }

        let ts = tailscale::ensure_exit_node(&exit_node);
        if !ts.is_ok() {
            let ssid = nm::active_ssid(&iface).or_else(|| profile.bootstrap.clone());
            notify(
                "Tailscale Error",
                &format!(
                    "{} — staying on {}",
                    ts.describe(),
                    ssid.clone().unwrap_or_else(|| "Wi-Fi".into())
                ),
                Urgency::Critical,
            );
            return Outcome::TailscaleError { ssid, health: ts };
        }
        log(&format!("tailscale healthy via exit node {exit_node}"));
        // Refresh visibility before moving to the target network.
        nm::rescan(&iface, &scan_targets);
    }

    let visible = nm::visible_ssids(&iface);

    // ---- Connect to the priority list ----------------------------------
    // Pass 1: visible networks in priority order.
    let mut any_attempted = false;
    for def in &candidates {
        if visible.contains(&def.ssid) {
            any_attempted = true;
            match connect_and_verify(&iface, def, cfg) {
                Ok(()) => {
                    let note = if internet_ok(cfg) {
                        None
                    } else {
                        Some("associated but no internet yet".to_string())
                    };
                    finish_connected(&def.ssid, profile_name, &note);
                    return Outcome::Connected {
                        ssid: def.ssid.clone(),
                        note,
                    };
                }
                Err(e) => log(&format!("connect failed (visible): {} — {e}", def.ssid)),
            }
        }
    }
    // Pass 2: hidden networks we couldn't see in the scan.
    for def in &candidates {
        if def.hidden && !visible.contains(&def.ssid) {
            any_attempted = true;
            match connect_and_verify(&iface, def, cfg) {
                Ok(()) => {
                    let note = if internet_ok(cfg) {
                        None
                    } else {
                        Some("associated but no internet yet".to_string())
                    };
                    finish_connected(&def.ssid, profile_name, &note);
                    return Outcome::Connected {
                        ssid: def.ssid.clone(),
                        note,
                    };
                }
                Err(e) => log(&format!("connect failed (hidden): {} — {e}", def.ssid)),
            }
        }
    }

    // ---- Nothing in the priority list connected ------------------------
    if on_bootstrap {
        // The failed candidate connect attempt disconnected us from bootstrap.
        // Re-establish it before returning so the claimed state is real.
        let bs_ssid = profile
            .bootstrap
            .clone()
            .unwrap_or_else(|| "bootstrap".into());
        if !nm::device_connected(&iface) {
            if let Some(bdef) = profile.bootstrap.as_deref().and_then(|s| cfg.network(s)) {
                match connect_and_verify(&iface, bdef, cfg) {
                    Ok(()) => log(&format!("bootstrap reconnected: {}", bdef.ssid)),
                    Err(e) => {
                        log(&format!("bootstrap reconnect failed: {} — {e}", bdef.ssid));
                        on_bootstrap = false;
                    }
                }
            }
        }
        if on_bootstrap {
            let reason = if any_attempted {
                format!("target network connect failed — staying on {bs_ssid} (Tailscale OK)")
            } else {
                format!("target network not in range — staying on {bs_ssid} (Tailscale OK)")
            };
            notify("breadcrumbs: using bootstrap", &reason, Urgency::Normal);
            log(&format!("flow end: on bootstrap {bs_ssid}; {reason}"));
            return Outcome::Connected {
                ssid: bs_ssid,
                note: Some(reason),
            };
        }
    }

    let names = candidates
        .iter()
        .map(|c| c.ssid.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    notify(
        "breadcrumbs: no known networks",
        &format!("profile '{profile_name}': none of [{names}] are in range"),
        Urgency::Critical,
    );
    log(&format!(
        "flow end: no networks connected (profile={profile_name})"
    ));
    Outcome::NoNetworks
}

fn finish_connected(ssid: &str, profile: &str, note: &Option<String>) {
    match note {
        None => {
            notify(
                "breadcrumbs: connected",
                &format!("{ssid} ({profile})"),
                Urgency::Low,
            );
            log(&format!("flow end: connected {ssid} (profile={profile})"));
        }
        Some(n) => {
            notify(
                "breadcrumbs: connected (degraded)",
                &format!("{ssid} ({profile}) — {n}"),
                Urgency::Normal,
            );
            log(&format!(
                "flow end: connected {ssid} (profile={profile}) note={n}"
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Profile, Settings};
    use std::collections::BTreeMap;

    fn net(ssid: &str) -> NetworkDef {
        NetworkDef {
            ssid: ssid.into(),
            password: "x".into(),
            hidden: false,
        }
    }

    fn cfg() -> Config {
        Config {
            settings: Settings::default(),
            networks: vec![
                net("HomeWifi"),
                net("WorkNet"),
                net("CafeWifi"),
                net("FallbackNet"),
            ],
            profiles: BTreeMap::new(),
        }
    }

    #[test]
    fn candidates_follow_priority_order() {
        let c = cfg();
        let p = Profile {
            networks: vec!["FallbackNet".into(), "HomeWifi".into()],
            ..Default::default()
        };
        let got: Vec<&str> = resolve_candidates(&c, &p)
            .iter()
            .map(|n| n.ssid.as_str())
            .collect();
        assert_eq!(got, vec!["FallbackNet", "HomeWifi"]);
    }

    #[test]
    fn include_all_known_appends_remaining_without_dupes() {
        let c = cfg();
        let p = Profile {
            networks: vec!["HomeWifi".into()],
            include_all_known: true,
            ..Default::default()
        };
        let got: Vec<&str> = resolve_candidates(&c, &p)
            .iter()
            .map(|n| n.ssid.as_str())
            .collect();
        assert_eq!(got[0], "HomeWifi");
        assert_eq!(got.len(), 4);
        assert!(got.contains(&"WorkNet"));
        // No duplicate of the explicitly-listed network.
        assert_eq!(got.iter().filter(|s| **s == "HomeWifi").count(), 1);
    }

    #[test]
    fn unknown_ssids_in_profile_are_skipped() {
        let c = cfg();
        let p = Profile {
            networks: vec!["Ghost".into(), "WorkNet".into()],
            ..Default::default()
        };
        let got: Vec<&str> = resolve_candidates(&c, &p)
            .iter()
            .map(|n| n.ssid.as_str())
            .collect();
        assert_eq!(got, vec!["WorkNet"]);
    }
}
