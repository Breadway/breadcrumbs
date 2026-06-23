use crate::backend::Backend;
use crate::config::{Config, NetworkDef};
use crate::notify::Urgency;
use crate::status::Connectivity;
use crate::tailscale::TsHealth;

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
fn connect_and_verify(
    be: &dyn Backend,
    iface: &str,
    def: &NetworkDef,
    cfg: &Config,
) -> Result<(), String> {
    be.connect(iface, def, cfg.settings.nmcli_wait, &cfg.settings.dns)?;
    if !be.device_connected(iface) {
        return Err("device not connected after nmcli success".into());
    }
    Ok(())
}

/// Describe post-association connectivity as an optional caveat note.
fn connectivity_note(be: &dyn Backend, cfg: &Config) -> Option<String> {
    match be.connectivity(cfg) {
        Connectivity::Online => None,
        Connectivity::Portal(Some(url)) => Some(format!("captive portal — sign in at {url}")),
        Connectivity::Portal(None) => Some("captive portal — sign in required".into()),
        Connectivity::Offline => Some("associated but no internet yet".into()),
    }
}

/// Run the connection state machine for `profile_name`.
pub fn run(be: &dyn Backend, cfg: &Config, profile_name: &str) -> Outcome {
    let profile = match cfg.profile(profile_name) {
        Some(p) => p.clone(),
        None => {
            be.notify(
                "breadcrumbs: unknown profile",
                &format!("'{profile_name}' is not defined in breadcrumbs.toml"),
                Urgency::Critical,
            );
            return Outcome::UnknownProfile(profile_name.to_string());
        }
    };

    let iface = match be.wifi_interface() {
        Some(i) => i,
        None => {
            be.notify(
                "breadcrumbs: no Wi-Fi adapter",
                "Hardware issue — Wi-Fi device not found. Manual check needed.",
                Urgency::Critical,
            );
            return Outcome::NoInterface;
        }
    };
    be.radio_on();

    let exit_node = profile
        .exit_node
        .clone()
        .unwrap_or_else(|| cfg.settings.exit_node.clone());
    let candidates = resolve_candidates(cfg, &profile);

    be.log(&format!(
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
    be.rescan(&iface, &scan_targets);
    let visible = be.visible_ssids(&iface);

    // ---- Tailscale-gated profiles (e.g. school) -------------------------
    let mut on_bootstrap = false;
    if profile.tailscale {
        if let Some(bs_ssid) = profile.bootstrap.clone() {
            match cfg.network(&bs_ssid) {
                Some(bdef) => {
                    if visible.contains(&bdef.ssid) || bdef.hidden {
                        match connect_and_verify(be, &iface, bdef, cfg) {
                            Ok(()) => {
                                on_bootstrap = true;
                                be.log(&format!("bootstrap connected: {}", bdef.ssid));
                            }
                            Err(e) => {
                                be.log(&format!("bootstrap connect failed: {} — {e}", bdef.ssid))
                            }
                        }
                    } else {
                        be.log(&format!("bootstrap not in range: {}", bdef.ssid));
                    }
                }
                None => be.log(&format!(
                    "bootstrap SSID '{bs_ssid}' has no credentials in config"
                )),
            }
        }

        let ts = be.ensure_exit_node(&exit_node);
        if !ts.is_ok() {
            let ssid = be.active_ssid(&iface).or_else(|| profile.bootstrap.clone());
            be.notify(
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
        be.log(&format!("tailscale healthy via exit node {exit_node}"));
        // Refresh visibility before moving to the target network.
        be.rescan(&iface, &scan_targets);
    }

    let visible = be.visible_ssids(&iface);

    // ---- Connect to the priority list ----------------------------------
    // Pass 1: visible networks in priority order.
    let mut any_attempted = false;
    for def in &candidates {
        if visible.contains(&def.ssid) {
            any_attempted = true;
            match connect_and_verify(be, &iface, def, cfg) {
                Ok(()) => {
                    let note = connectivity_note(be, cfg);
                    finish_connected(be, &def.ssid, profile_name, &note);
                    return Outcome::Connected {
                        ssid: def.ssid.clone(),
                        note,
                    };
                }
                Err(e) => be.log(&format!("connect failed (visible): {} — {e}", def.ssid)),
            }
        }
    }
    // Pass 2: hidden networks we couldn't see in the scan.
    for def in &candidates {
        if def.hidden && !visible.contains(&def.ssid) {
            any_attempted = true;
            match connect_and_verify(be, &iface, def, cfg) {
                Ok(()) => {
                    let note = connectivity_note(be, cfg);
                    finish_connected(be, &def.ssid, profile_name, &note);
                    return Outcome::Connected {
                        ssid: def.ssid.clone(),
                        note,
                    };
                }
                Err(e) => be.log(&format!("connect failed (hidden): {} — {e}", def.ssid)),
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
        if !be.device_connected(&iface) {
            if let Some(bdef) = profile.bootstrap.as_deref().and_then(|s| cfg.network(s)) {
                match connect_and_verify(be, &iface, bdef, cfg) {
                    Ok(()) => be.log(&format!("bootstrap reconnected: {}", bdef.ssid)),
                    Err(e) => {
                        be.log(&format!("bootstrap reconnect failed: {} — {e}", bdef.ssid));
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
            be.notify("breadcrumbs: using bootstrap", &reason, Urgency::Normal);
            be.log(&format!("flow end: on bootstrap {bs_ssid}; {reason}"));
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
    be.notify(
        "breadcrumbs: no known networks",
        &format!("profile '{profile_name}': none of [{names}] are in range"),
        Urgency::Critical,
    );
    be.log(&format!(
        "flow end: no networks connected (profile={profile_name})"
    ));
    Outcome::NoNetworks
}

fn finish_connected(be: &dyn Backend, ssid: &str, profile: &str, note: &Option<String>) {
    match note {
        None => {
            be.notify(
                "breadcrumbs: connected",
                &format!("{ssid} ({profile})"),
                Urgency::Low,
            );
            be.log(&format!("flow end: connected {ssid} (profile={profile})"));
        }
        Some(n) => {
            be.notify(
                "breadcrumbs: connected (degraded)",
                &format!("{ssid} ({profile}) — {n}"),
                Urgency::Normal,
            );
            be.log(&format!(
                "flow end: connected {ssid} (profile={profile}) note={n}"
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Profile, Settings};
    use crate::tailscale::TsHealth;
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

    // --- a scriptable in-memory Backend for testing the state machine ---

    use std::cell::RefCell;
    use std::collections::HashSet;

    struct Fake {
        iface: Option<String>,
        visible: HashSet<String>,
        connectable: HashSet<String>,
        connected: RefCell<Option<String>>,
        ts: TsHealth,
        conn: Connectivity,
        notes: RefCell<Vec<String>>,
    }

    impl Fake {
        fn new() -> Fake {
            Fake {
                iface: Some("wlan0".into()),
                visible: HashSet::new(),
                connectable: HashSet::new(),
                connected: RefCell::new(None),
                ts: TsHealth::Ok,
                conn: Connectivity::Online,
                notes: RefCell::new(Vec::new()),
            }
        }
        fn set(ssids: &[&str]) -> HashSet<String> {
            ssids.iter().map(|s| s.to_string()).collect()
        }
        fn visible(mut self, ssids: &[&str]) -> Self {
            self.visible = Fake::set(ssids);
            self
        }
        fn connectable(mut self, ssids: &[&str]) -> Self {
            self.connectable = Fake::set(ssids);
            self
        }
        fn ts(mut self, h: TsHealth) -> Self {
            self.ts = h;
            self
        }
        fn conn(mut self, c: Connectivity) -> Self {
            self.conn = c;
            self
        }
        fn no_iface(mut self) -> Self {
            self.iface = None;
            self
        }
        fn notified(&self, needle: &str) -> bool {
            self.notes.borrow().iter().any(|n| n.contains(needle))
        }
    }

    impl Backend for Fake {
        fn wifi_interface(&self) -> Option<String> {
            self.iface.clone()
        }
        fn radio_on(&self) {}
        fn rescan(&self, _: &str, _: &[String]) {}
        fn visible_ssids(&self, _: &str) -> HashSet<String> {
            self.visible.clone()
        }
        fn active_ssid(&self, _: &str) -> Option<String> {
            self.connected.borrow().clone()
        }
        fn ipv4(&self, _: &str) -> Option<String> {
            self.connected.borrow().as_ref().map(|_| "10.0.0.2".into())
        }
        fn device_connected(&self, _: &str) -> bool {
            self.connected.borrow().is_some()
        }
        fn connect(&self, _: &str, net: &NetworkDef, _: u32, _: &str) -> Result<(), String> {
            if self.connectable.contains(&net.ssid) {
                *self.connected.borrow_mut() = Some(net.ssid.clone());
                Ok(())
            } else {
                // A failed association drops the current link, like nmcli.
                *self.connected.borrow_mut() = None;
                Err(format!("cannot connect to {}", net.ssid))
            }
        }
        fn tailscale_installed(&self) -> bool {
            true
        }
        fn ensure_exit_node(&self, _: &str) -> TsHealth {
            self.ts.clone()
        }
        fn tailscale_check(&self, _: &str) -> TsHealth {
            self.ts.clone()
        }
        fn connectivity(&self, _: &Config) -> Connectivity {
            self.conn.clone()
        }
        fn notify(&self, summary: &str, _: &str, _: Urgency) {
            self.notes.borrow_mut().push(summary.to_string());
        }
        fn log(&self, _: &str) {}
    }

    fn with_profile(name: &str, p: Profile) -> Config {
        let mut c = cfg();
        c.profiles.insert(name.to_string(), p);
        c
    }

    fn connected(o: &Outcome) -> (&str, &Option<String>) {
        match o {
            Outcome::Connected { ssid, note } => (ssid.as_str(), note),
            other => panic!("expected Connected, got {other:?}"),
        }
    }

    // --- flow::run state machine ---

    #[test]
    fn run_unknown_profile_returns_unknown_and_notifies() {
        let be = Fake::new();
        let o = run(&be, &cfg(), "ghost");
        assert!(matches!(o, Outcome::UnknownProfile(p) if p == "ghost"));
        assert!(be.notified("unknown profile"));
    }

    #[test]
    fn run_no_interface_returns_no_interface() {
        let be = Fake::new().no_iface();
        let c = with_profile(
            "home",
            Profile {
                networks: vec!["HomeWifi".into()],
                ..Default::default()
            },
        );
        assert!(matches!(run(&be, &c, "home"), Outcome::NoInterface));
    }

    #[test]
    fn run_connects_to_visible_priority_network() {
        let c = with_profile(
            "home",
            Profile {
                networks: vec!["HomeWifi".into()],
                ..Default::default()
            },
        );
        let be = Fake::new()
            .visible(&["HomeWifi", "CafeWifi"])
            .connectable(&["HomeWifi"]);
        let o = run(&be, &c, "home");
        let (ssid, note) = connected(&o);
        assert_eq!(ssid, "HomeWifi");
        assert!(note.is_none());
    }

    #[test]
    fn run_follows_priority_order() {
        let c = with_profile(
            "home",
            Profile {
                networks: vec!["WorkNet".into(), "HomeWifi".into()],
                ..Default::default()
            },
        );
        let be = Fake::new()
            .visible(&["WorkNet", "HomeWifi"])
            .connectable(&["WorkNet", "HomeWifi"]);
        assert_eq!(connected(&run(&be, &c, "home")).0, "WorkNet");
    }

    #[test]
    fn run_skips_failing_network_and_tries_next() {
        let c = with_profile(
            "home",
            Profile {
                networks: vec!["WorkNet".into(), "HomeWifi".into()],
                ..Default::default()
            },
        );
        let be = Fake::new()
            .visible(&["WorkNet", "HomeWifi"])
            .connectable(&["HomeWifi"]); // WorkNet fails
        assert_eq!(connected(&run(&be, &c, "home")).0, "HomeWifi");
    }

    #[test]
    fn run_no_networks_in_range_returns_no_networks() {
        let c = with_profile(
            "home",
            Profile {
                networks: vec!["HomeWifi".into()],
                ..Default::default()
            },
        );
        let be = Fake::new().visible(&["SomeoneElse"]);
        assert!(matches!(run(&be, &c, "home"), Outcome::NoNetworks));
        assert!(be.notified("no known networks"));
    }

    #[test]
    fn run_captive_portal_surfaces_as_note() {
        let c = with_profile(
            "home",
            Profile {
                networks: vec!["HomeWifi".into()],
                ..Default::default()
            },
        );
        let be = Fake::new()
            .visible(&["HomeWifi"])
            .connectable(&["HomeWifi"])
            .conn(Connectivity::Portal(Some("http://login.test".into())));
        let o = run(&be, &c, "home");
        let (_, note) = connected(&o);
        assert!(note.as_ref().unwrap().contains("captive portal"));
        assert!(note.as_ref().unwrap().contains("http://login.test"));
    }

    #[test]
    fn run_offline_after_associate_is_degraded_note() {
        let c = with_profile(
            "home",
            Profile {
                networks: vec!["HomeWifi".into()],
                ..Default::default()
            },
        );
        let be = Fake::new()
            .visible(&["HomeWifi"])
            .connectable(&["HomeWifi"])
            .conn(Connectivity::Offline);
        let o = run(&be, &c, "home");
        let (_, note) = connected(&o);
        assert!(note.as_ref().unwrap().contains("no internet"));
    }

    #[test]
    fn run_tailscale_gated_moves_to_target_when_healthy() {
        let c = with_profile(
            "work",
            Profile {
                bootstrap: Some("CafeWifi".into()),
                networks: vec!["WorkNet".into()],
                tailscale: true,
                ..Default::default()
            },
        );
        let be = Fake::new()
            .visible(&["CafeWifi", "WorkNet"])
            .connectable(&["CafeWifi", "WorkNet"])
            .ts(TsHealth::Ok);
        assert_eq!(connected(&run(&be, &c, "work")).0, "WorkNet");
    }

    #[test]
    fn run_tailscale_unhealthy_stays_on_bootstrap() {
        let c = with_profile(
            "work",
            Profile {
                bootstrap: Some("CafeWifi".into()),
                networks: vec!["WorkNet".into()],
                tailscale: true,
                ..Default::default()
            },
        );
        let be = Fake::new()
            .visible(&["CafeWifi", "WorkNet"])
            .connectable(&["CafeWifi", "WorkNet"])
            .ts(TsHealth::NeedsLogin);
        match run(&be, &c, "work") {
            Outcome::TailscaleError { ssid, health } => {
                assert_eq!(ssid.as_deref(), Some("CafeWifi"));
                assert_eq!(health, TsHealth::NeedsLogin);
            }
            other => panic!("expected TailscaleError, got {other:?}"),
        }
        assert!(be.notified("Tailscale"));
    }

    #[test]
    fn run_tailscale_ok_but_target_out_of_range_keeps_bootstrap() {
        let c = with_profile(
            "work",
            Profile {
                bootstrap: Some("CafeWifi".into()),
                networks: vec!["WorkNet".into()],
                tailscale: true,
                ..Default::default()
            },
        );
        let be = Fake::new()
            .visible(&["CafeWifi"]) // WorkNet not in range
            .connectable(&["CafeWifi"])
            .ts(TsHealth::Ok);
        let o = run(&be, &c, "work");
        let (ssid, note) = connected(&o);
        assert_eq!(ssid, "CafeWifi");
        assert!(note.as_ref().unwrap().contains("not in range"));
    }

    // --- Outcome::ok ---

    #[test]
    fn outcome_ok_true_for_connected_with_and_without_note() {
        assert!(Outcome::Connected {
            ssid: "x".into(),
            note: None
        }
        .ok());
        assert!(Outcome::Connected {
            ssid: "x".into(),
            note: Some("associated but no internet yet".into()),
        }
        .ok());
    }

    #[test]
    fn outcome_ok_false_for_all_error_variants() {
        assert!(!Outcome::NoInterface.ok());
        assert!(!Outcome::NoNetworks.ok());
        assert!(!Outcome::UnknownProfile("p".into()).ok());
        assert!(!Outcome::TailscaleError {
            ssid: None,
            health: TsHealth::NeedsLogin,
        }
        .ok());
        assert!(!Outcome::TailscaleError {
            ssid: Some("boot".into()),
            health: TsHealth::ExitNodeOffline,
        }
        .ok());
    }

    // --- resolve_candidates ---

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

    #[test]
    fn candidates_empty_when_profile_has_no_networks() {
        let c = cfg();
        let p = Profile::default();
        assert!(resolve_candidates(&c, &p).is_empty());
    }

    #[test]
    fn candidates_include_all_known_only_no_explicit_networks() {
        let c = cfg();
        let p = Profile {
            include_all_known: true,
            ..Default::default()
        };
        let got: Vec<&str> = resolve_candidates(&c, &p)
            .iter()
            .map(|n| n.ssid.as_str())
            .collect();
        assert_eq!(got.len(), 4);
        assert_eq!(got[0], "HomeWifi");
    }

    #[test]
    fn candidates_deduplicates_repeated_ssid_in_explicit_list() {
        let c = cfg();
        let p = Profile {
            networks: vec!["HomeWifi".into(), "HomeWifi".into(), "WorkNet".into()],
            ..Default::default()
        };
        let got: Vec<&str> = resolve_candidates(&c, &p)
            .iter()
            .map(|n| n.ssid.as_str())
            .collect();
        assert_eq!(got, vec!["HomeWifi", "WorkNet"]);
    }

    #[test]
    fn candidates_all_unknown_ssids_returns_empty() {
        let c = cfg();
        let p = Profile {
            networks: vec!["Ghost1".into(), "Ghost2".into()],
            ..Default::default()
        };
        assert!(resolve_candidates(&c, &p).is_empty());
    }
}
