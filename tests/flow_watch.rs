//! In-process tests for the actual state machine (`flow::run`) and the watch
//! loop's health classification (`watch::classify`). NetworkManager is a real
//! fake NM D-Bus service on a private bus (see `tests/common::fake_nm`), so
//! every `nm` call is exercised over genuine D-Bus marshalling. Everything
//! else (tailscale, curl/ping, notify) is faked through a
//! `breadcrumbs::util::Runner` (see `tests/common`). This complements
//! `tests/cli.rs`'s black-box coverage (which spawns the real binary) with
//! fast, precise coverage of the logic itself: candidate priority order, the
//! bootstrap+Tailscale gate, and every `watch::Health` transition.

mod common;

use std::collections::BTreeMap;

use bread_utils::bread_client::BreadEvent;
use breadcrumbs::bread_events;
use breadcrumbs::config::{Config, NetworkDef, Profile, Settings};
use breadcrumbs::flow;
use breadcrumbs::state::{self, State};
use breadcrumbs::util::with_runner;
use breadcrumbs::watch::{classify, Health};

use common::fake_nm::{self, Security, SharedNm};
use common::{fail, ok, EnvSandbox, FakeRunner};

fn net(ssid: &str, password: Option<&str>) -> NetworkDef {
    NetworkDef {
        ssid: ssid.to_string(),
        password: password.map(str::to_string),
        dns: None,
        eap: None,
        identity: None,
        ca_cert: None,
        hidden: false,
    }
}

fn hidden_net(ssid: &str, password: Option<&str>) -> NetworkDef {
    NetworkDef {
        ssid: ssid.to_string(),
        password: password.map(str::to_string),
        dns: None,
        eap: None,
        identity: None,
        ca_cert: None,
        hidden: true,
    }
}

fn base_config() -> Config {
    Config {
        settings: Settings::default(),
        networks: Vec::new(),
        profiles: BTreeMap::new(),
    }
}

/// Reset the shared fake-NM bus and put a Wi-Fi device on it with one AP per
/// SSID at the given signal strength. Returns the bus guard (held for the
/// whole test so tests serialize) and the device path.
fn setup_wifi(ssids: &[(&str, u8)]) -> (SharedNm, String) {
    let nm = fake_nm::shared();
    nm.reset();
    let dev = nm.add_wifi_device("wlan0", 100);
    for (ssid, strength) in ssids {
        nm.add_ap(&dev, ssid, *strength, Security::Wpa2);
    }
    (nm, dev)
}

/// A runner that fakes the non-NM subprocesses a successful `flow::run`
/// needs: curl (internet check) and nothing else.
fn healthy_runner() -> FakeRunner {
    FakeRunner::new()
        .with_command("curl")
        .on(|prog, _| prog == "curl", ok("204"))
}

/// The runner used by `classify` tests: internet check + optional tailscale.
fn classify_runner(curl: &str, tailscale_status: Option<&str>) -> FakeRunner {
    let mut r = FakeRunner::new().with_command("curl").on(|p, _| p == "curl", ok(curl));
    if let Some(json) = tailscale_status {
        r = r
            .with_command("tailscale")
            .on(|p, args| p == "tailscale" && args.contains(&"status"), ok(json));
    }
    r
}

/// Make the device report being associated with `ssid` (for classify tests).
fn associate(nm: &SharedNm, dev: &str, ssid: &str) {
    let ap = nm.add_ap(dev, ssid, 80, Security::Wpa2);
    nm.set_active_ap(dev, &ap);
}

fn tailscale_json_ok(exit_node: &str) -> String {
    format!(
        r#"{{"BackendState":"Running","Peer":{{"k1":{{"HostName":"{exit_node}","DNSName":"{exit_node}.ts.net.","Online":true,"ExitNode":true,"ExitNodeOption":true}}}}}}"#
    )
}

fn tailscale_json_missing() -> &'static str {
    r#"{"BackendState":"Running","Peer":{}}"#
}

// ---------------------------------------------------------------------
// flow::run — candidate priority (pass 1 / pass 2)
// ---------------------------------------------------------------------

#[test]
fn flow_run_connects_to_first_visible_candidate_in_priority_order() {
    let _env = EnvSandbox::new();
    let (nm, _dev) = setup_wifi(&[("First", 80), ("Second", 80)]);

    let mut cfg = base_config();
    cfg.networks = vec![net("First", Some("pw1")), net("Second", Some("pw2"))];
    cfg.profiles.insert(
        "home".into(),
        Profile {
            networks: vec!["First".into(), "Second".into()],
            ..Default::default()
        },
    );

    let outcome = with_runner(healthy_runner(), || flow::run(&mut cfg, "home"));

    match outcome {
        flow::Outcome::Connected { ssid, note } => {
            assert_eq!(ssid, "First");
            assert_eq!(note, None);
        }
        other => panic!("expected Connected, got {other:?}"),
    }

    // Priority order actually mattered: "Second" was never activated even
    // though it was visible and would have succeeded too.
    assert_eq!(
        nm.activated_ssids(),
        vec!["First".to_string()],
        "Second must not be dialed when First wins"
    );

    // The password used for the winning connect is now NM's problem, not
    // breadcrumbs' — cleared and (via clear_password_if_used) persisted.
    assert_eq!(cfg.network("First").unwrap().password, None);
    // Never touched, so its password is untouched too.
    assert_eq!(
        cfg.network("Second").unwrap().password,
        Some("pw2".to_string())
    );
}

#[test]
fn flow_run_pass2_falls_back_to_hidden_candidate_not_in_scan() {
    let _env = EnvSandbox::new();
    // Nothing is visible; connecting creates the AP on the fly (hidden
    // networks appear only after association).
    let (nm, _dev) = setup_wifi(&[]);
    nm.set_connect_any(true);

    let mut cfg = base_config();
    // "Ghost" is neither visible nor hidden, so pass 1 *and* pass 2 both
    // skip it outright — it should never be dialed.
    cfg.networks = vec![net("Ghost", Some("pw-ghost")), hidden_net("Shadow", Some("pw-shadow"))];
    cfg.profiles.insert(
        "away".into(),
        Profile {
            networks: vec!["Ghost".into(), "Shadow".into()],
            ..Default::default()
        },
    );

    let outcome = with_runner(healthy_runner(), || flow::run(&mut cfg, "away"));

    match outcome {
        flow::Outcome::Connected { ssid, .. } => assert_eq!(ssid, "Shadow"),
        other => panic!("expected Connected to Shadow, got {other:?}"),
    }
    assert_eq!(
        nm.activated_ssids(),
        vec!["Shadow".to_string()],
        "Ghost must never have been dialed"
    );
}

#[test]
fn flow_run_unknown_profile_short_circuits_before_touching_nm() {
    let _env = EnvSandbox::new();
    let nm = fake_nm::shared();
    nm.reset();
    let mut cfg = base_config();

    let runner = FakeRunner::new(); // no rules at all

    let outcome = with_runner(runner, || flow::run(&mut cfg, "does-not-exist"));

    assert!(matches!(outcome, flow::Outcome::UnknownProfile(p) if p == "does-not-exist"));
    // The fake NetworkManager must never be touched for a profile that
    // doesn't exist (no devices, no scans, no activations).
    assert!(
        nm.calls().is_empty(),
        "unknown-profile path should never call NetworkManager: {:?}",
        nm.calls()
    );
}

// ---------------------------------------------------------------------
// flow::run — bootstrap + Tailscale gating
// ---------------------------------------------------------------------

#[test]
fn flow_run_moves_past_bootstrap_once_tailscale_is_healthy() {
    let _env = EnvSandbox::new();
    let (nm, _dev) = setup_wifi(&[("Guest", 80), ("Corp", 80)]);

    let mut cfg = base_config();
    cfg.settings.exit_node = "exitnode".into();
    cfg.networks = vec![net("Guest", Some("guest-pw")), net("Corp", Some("corp-pw"))];
    cfg.profiles.insert(
        "work".into(),
        Profile {
            bootstrap: Some("Guest".into()),
            networks: vec!["Corp".into()],
            tailscale: true,
            ..Default::default()
        },
    );

    let runner = healthy_runner()
        .with_command("tailscale")
        .on_contains("tailscale", "status", ok(&tailscale_json_ok("exitnode")))
        .on_contains("tailscale", "set", ok(""));

    let outcome = with_runner(runner, || flow::run(&mut cfg, "work"));

    match outcome {
        flow::Outcome::Connected { ssid, .. } => assert_eq!(ssid, "Corp"),
        other => panic!("expected Connected to Corp, got {other:?}"),
    }
    // Both the bootstrap and target connects used a local password, so both
    // should have been cleared once NetworkManager took over.
    assert_eq!(cfg.network("Guest").unwrap().password, None);
    assert_eq!(cfg.network("Corp").unwrap().password, None);

    assert_eq!(
        nm.activated_ssids(),
        vec!["Guest".to_string(), "Corp".to_string()],
        "bootstrap must be dialed before the target"
    );
}

#[test]
fn flow_run_stays_on_bootstrap_and_never_dials_target_when_tailscale_unhealthy() {
    let _env = EnvSandbox::new();
    let (nm, _dev) = setup_wifi(&[("Guest", 80), ("Corp", 80)]);

    let mut cfg = base_config();
    cfg.settings.exit_node = "exitnode".into();
    cfg.networks = vec![net("Guest", Some("guest-pw")), net("Corp", Some("corp-pw"))];
    cfg.profiles.insert(
        "work".into(),
        Profile {
            bootstrap: Some("Guest".into()),
            networks: vec!["Corp".into()],
            tailscale: true,
            ..Default::default()
        },
    );

    let runner = healthy_runner()
        .with_command("tailscale")
        .on(|p, args| p == "tailscale" && args.contains(&"status"), ok(tailscale_json_missing()))
        .on(|p, args| p == "tailscale" && args.contains(&"set"), ok(""));

    let outcome = with_runner(runner, || flow::run(&mut cfg, "work"));

    match &outcome {
        flow::Outcome::TailscaleError { ssid, health } => {
            assert_eq!(ssid.as_deref(), Some("Guest"));
            assert_eq!(*health, breadcrumbs::tailscale::TsHealth::ExitNodeMissing);
        }
        other => panic!("expected TailscaleError, got {other:?}"),
    }

    assert_eq!(
        nm.activated_ssids(),
        vec!["Guest".to_string()],
        "target network must never be dialed while Tailscale is unhealthy"
    );
    // The bootstrap connect *did* use a password and succeeded, so it's
    // cleared even though the overall flow ends in an error.
    assert_eq!(cfg.network("Guest").unwrap().password, None);
}

// ---------------------------------------------------------------------
// watch::classify — health-state transitions
// ---------------------------------------------------------------------

#[test]
fn classify_reports_unknown_profile_without_touching_nm() {
    let _env = EnvSandbox::new();
    let nm = fake_nm::shared();
    nm.reset();
    let cfg = base_config(); // no profiles at all

    let runner = FakeRunner::new();
    let calls = runner.calls_handle();
    let class = with_runner(runner, || classify(&cfg, "ghost"));

    assert_eq!(class.health, Health::UnknownProfile);
    assert_eq!(class.ssid, None);
    assert!(calls.borrow().is_empty());
    assert!(
        nm.calls().is_empty(),
        "unknown-profile classify must not touch NetworkManager"
    );
}

#[test]
fn classify_reports_no_adapter_when_wifi_interface_absent() {
    let _env = EnvSandbox::new();
    let nm = fake_nm::shared();
    nm.reset(); // no devices at all
    let mut cfg = base_config();
    cfg.profiles.insert("away".into(), Profile::default());

    let class = with_runner(FakeRunner::new(), || classify(&cfg, "away"));
    assert_eq!(class.health, Health::NoAdapter);
}

#[test]
fn classify_reports_down_no_net_when_internet_check_fails() {
    let _env = EnvSandbox::new();
    let (nm, dev) = setup_wifi(&[]);
    associate(&nm, &dev, "HomeWifi");
    let mut cfg = base_config();
    cfg.profiles.insert("away".into(), Profile::default());

    let runner = FakeRunner::new().on(|prog, _| prog == "curl" || prog == "ping", fail(""));
    let class = with_runner(runner, || classify(&cfg, "away"));

    assert_eq!(class.health, Health::DownNoNet);
    assert_eq!(class.ssid, Some("HomeWifi".to_string()));
}

#[test]
fn classify_reports_up_when_healthy_and_tailscale_not_required() {
    let _env = EnvSandbox::new();
    let (nm, dev) = setup_wifi(&[]);
    associate(&nm, &dev, "HomeWifi");
    let mut cfg = base_config();
    cfg.profiles.insert("home".into(), Profile::default()); // tailscale: false

    let class = with_runner(healthy_runner(), || classify(&cfg, "home"));

    assert_eq!(class.health, Health::Up);
    assert_eq!(class.ssid, Some("HomeWifi".to_string()));
}

#[test]
fn classify_reports_down_tailscale_manual_when_not_installed() {
    let _env = EnvSandbox::new();
    let (nm, dev) = setup_wifi(&[]);
    associate(&nm, &dev, "CorpWifi");
    let mut cfg = base_config();
    cfg.profiles.insert(
        "work".into(),
        Profile {
            tailscale: true,
            ..Default::default()
        },
    );

    // No `with_command("tailscale")`, so `tailscale::installed()` is false.
    let class = with_runner(healthy_runner(), || classify(&cfg, "work"));

    assert_eq!(class.health, Health::DownTailscaleManual);
}

#[test]
fn classify_reports_down_tailscale_manual_when_needs_login() {
    let _env = EnvSandbox::new();
    let (nm, dev) = setup_wifi(&[]);
    associate(&nm, &dev, "CorpWifi");
    let mut cfg = base_config();
    cfg.profiles.insert(
        "work".into(),
        Profile {
            tailscale: true,
            ..Default::default()
        },
    );

    let runner = classify_runner("204", Some(r#"{"BackendState":"NeedsLogin"}"#));
    let class = with_runner(runner, || classify(&cfg, "work"));

    assert_eq!(class.health, Health::DownTailscaleManual);
}

#[test]
fn classify_reports_down_tailscale_other_when_exit_node_offline() {
    let _env = EnvSandbox::new();
    let (nm, dev) = setup_wifi(&[]);
    associate(&nm, &dev, "CorpWifi");
    let mut cfg = base_config();
    cfg.settings.exit_node = "exitnode".into();
    cfg.profiles.insert(
        "work".into(),
        Profile {
            tailscale: true,
            ..Default::default()
        },
    );

    let json = r#"{"BackendState":"Running","Peer":{"k1":{"HostName":"exitnode","Online":false,"ExitNode":false,"ExitNodeOption":true}}}"#;
    let runner = classify_runner("204", Some(json));
    let class = with_runner(runner, || classify(&cfg, "work"));

    assert_eq!(class.health, Health::DownTailscaleOther);
}

#[test]
fn classify_reports_up_when_tailscale_healthy() {
    let _env = EnvSandbox::new();
    let (nm, dev) = setup_wifi(&[]);
    associate(&nm, &dev, "CorpWifi");
    let mut cfg = base_config();
    cfg.settings.exit_node = "exitnode".into();
    cfg.profiles.insert(
        "work".into(),
        Profile {
            tailscale: true,
            ..Default::default()
        },
    );

    let runner = classify_runner("204", Some(&tailscale_json_ok("exitnode")));
    let class = with_runner(runner, || classify(&cfg, "work"));

    assert_eq!(class.health, Health::Up);
}

// ---------------------------------------------------------------------
// bread.command.crumbs.set_profile — persists via the same path as the
// CLI, and must not depend on breadd being reachable (`emit` is
// fire-and-forget).
// ---------------------------------------------------------------------

fn command_event(event: &str, data: serde_json::Value) -> BreadEvent {
    BreadEvent {
        event: event.to_string(),
        timestamp: 0,
        data,
    }
}

#[test]
fn set_profile_command_persists_even_with_no_daemon_reachable() {
    let _env = EnvSandbox::new();
    let cfg = Config::load().expect("fresh config");
    state::set_profile(&cfg, "away").unwrap();
    assert_eq!(State::load("away").profile, "away");

    // handle_command only parses/validates (no file I/O on the subscription
    // thread); the loop thread then applies the action.
    let action = bread_events::handle_command(&command_event(
        "bread.command.crumbs.set_profile",
        serde_json::json!({ "profile": "home" }),
    ));
    assert!(
        matches!(action, bread_events::CommandAction::SetProfile(n) if n == "home"),
        "a known profile must yield a SetProfile action"
    );
    bread_events::apply_set_profile("home");
    assert_eq!(State::load("away").profile, "home");
}

#[test]
fn set_profile_command_rejects_unknown_profile() {
    let _env = EnvSandbox::new();
    let cfg = Config::load().expect("fresh config");
    state::set_profile(&cfg, "away").unwrap();

    let action = bread_events::handle_command(&command_event(
        "bread.command.crumbs.set_profile",
        serde_json::json!({ "profile": "bogus" }),
    ));
    assert!(matches!(action, bread_events::CommandAction::SetProfile(n) if n == "bogus"));
    // The rejection happens when the loop thread applies it: state is
    // untouched and the failure event is emitted (a no-op without breadd).
    bread_events::apply_set_profile("bogus");
    assert_eq!(
        State::load("away").profile,
        "away",
        "a rejected set_profile must not touch state"
    );
}

#[test]
fn set_profile_command_rejects_missing_profile_field() {
    let _env = EnvSandbox::new();
    let cfg = Config::load().expect("fresh config");
    state::set_profile(&cfg, "away").unwrap();

    let action = bread_events::handle_command(&command_event(
        "bread.command.crumbs.set_profile",
        serde_json::json!({}),
    ));
    assert!(matches!(action, bread_events::CommandAction::Ignore));
    assert_eq!(State::load("away").profile, "away");
}

#[test]
fn handle_command_ignores_unrecognized_verb() {
    let _env = EnvSandbox::new();
    let cfg = Config::load().expect("fresh config");
    state::set_profile(&cfg, "away").unwrap();

    let action = bread_events::handle_command(&command_event(
        "bread.command.crumbs.pin",
        serde_json::json!({}),
    ));
    assert!(matches!(action, bread_events::CommandAction::Ignore));
    assert_eq!(
        State::load("away").profile,
        "away",
        "an unrecognized verb must not touch state"
    );
}

#[test]
fn handle_command_ignores_events_outside_its_own_command_namespace() {
    let _env = EnvSandbox::new();
    let cfg = Config::load().expect("fresh config");
    state::set_profile(&cfg, "away").unwrap();

    assert!(matches!(
        bread_events::handle_command(&command_event(
            "bread.command.clip.clear",
            serde_json::json!({}),
        )),
        bread_events::CommandAction::Ignore
    ));
    assert!(matches!(
        bread_events::handle_command(&command_event(
            "bread.crumbs.profile.changed",
            serde_json::json!({ "from": "away", "to": "home" }),
        )),
        bread_events::CommandAction::Ignore
    ));
    assert_eq!(State::load("away").profile, "away");
}

// ---------------------------------------------------------------------
// Regression tests for the audit fixes.
// ---------------------------------------------------------------------

#[test]
fn flow_run_reports_no_exit_node_and_never_clears_selection() {
    // A tailscale profile with no exit node configured must report
    // TsHealth::NoExitNode — and must never run `tailscale set --exit-node=`
    // with an empty value, which would clear the user's current selection.
    let _env = EnvSandbox::new();
    let (nm, _dev) = setup_wifi(&[("Corp", 80)]);

    let mut cfg = base_config();
    cfg.networks = vec![net("Corp", Some("corp-pw"))];
    cfg.profiles.insert(
        "work".into(),
        Profile {
            networks: vec!["Corp".into()],
            tailscale: true,
            ..Default::default()
        },
    );

    let runner = FakeRunner::new().with_command("tailscale");
    let calls = runner.calls_handle();
    let outcome = with_runner(runner, || flow::run(&mut cfg, "work"));

    match &outcome {
        flow::Outcome::TailscaleError { health, .. } => {
            assert_eq!(*health, breadcrumbs::tailscale::TsHealth::NoExitNode);
        }
        other => panic!("expected TailscaleError(NoExitNode), got {other:?}"),
    }
    assert!(
        !calls.borrow().iter().any(|c| c.prog == "tailscale"),
        "with no exit node configured, tailscale must not be touched: {:?}",
        calls.borrow()
    );
    assert!(
        nm.activated_ssids().is_empty(),
        "with no exit node configured, no network must be dialed: {:?}",
        nm.activated_ssids()
    );
}

#[test]
fn classify_reports_down_tailscale_manual_when_no_exit_node_configured() {
    // An unset exit node needs human action (config edit), so it must
    // classify as DownTailscaleManual — not DownTailscaleOther, which would
    // make the watcher spin auto-recovery forever.
    let _env = EnvSandbox::new();
    let (nm, dev) = setup_wifi(&[]);
    associate(&nm, &dev, "CorpWifi");
    let mut cfg = base_config();
    cfg.profiles.insert(
        "work".into(),
        Profile {
            tailscale: true,
            ..Default::default()
        },
    );

    let runner = classify_runner("204", None).with_command("tailscale");
    let class = with_runner(runner, || classify(&cfg, "work"));

    assert_eq!(class.health, Health::DownTailscaleManual);
    assert_eq!(class.ssid, Some("CorpWifi".to_string()));
}

#[test]
fn ensure_exit_node_attempts_to_start_unreachable_daemon() {
    // `tailscale status --json` with empty stdout is the "daemon not
    // running" signature (the error goes to stderr). ensure_exit_node must
    // try `tailscale up` and re-read instead of bailing out with an opaque
    // error — the old dead-code path that made the Stopped recovery
    // unreachable.
    let _env = EnvSandbox::new();
    let runner = FakeRunner::new()
        .with_command("tailscale")
        .on(|p, args| p == "tailscale" && args.contains(&"status"), ok(""))
        .on(|p, args| p == "tailscale" && args.contains(&"up"), ok(""));
    let calls = runner.calls_handle();
    let health = with_runner(runner, || {
        breadcrumbs::tailscale::ensure_exit_node(&["exitnode".to_string()])
    });
    assert!(
        matches!(health, breadcrumbs::tailscale::TsHealth::Error(_)),
        "daemon still unreachable after `up` → Error, got {health:?}"
    );
    let tailscale_calls: Vec<String> = calls
        .borrow()
        .iter()
        .filter(|c| c.prog == "tailscale")
        .map(|c| c.args.join(" "))
        .collect();
    assert!(
        tailscale_calls.iter().any(|c| c.starts_with("up")),
        "must attempt `tailscale up` when the daemon is unreachable: {tailscale_calls:?}"
    );
}

#[test]
fn flow_run_fails_when_device_lands_on_wrong_ssid() {
    // NM autoconnect race: the connect succeeds but the device ends up on a
    // *different* network than requested. flow must not report Connected to
    // the requested SSID, and must not clear its password.
    let _env = EnvSandbox::new();
    let (nm, _dev) = setup_wifi(&[("First", 80), ("OtherNet", 90)]);
    nm.set_land_on(Some("OtherNet"));

    let mut cfg = base_config();
    cfg.networks = vec![net("First", Some("pw1"))];
    cfg.profiles.insert(
        "home".into(),
        Profile {
            networks: vec!["First".into()],
            ..Default::default()
        },
    );

    let outcome = with_runner(healthy_runner(), || flow::run(&mut cfg, "home"));

    assert!(
        !matches!(outcome, flow::Outcome::Connected { .. }),
        "must not report Connected when the device is on a different SSID: {outcome:?}"
    );
    assert_eq!(
        cfg.network("First").unwrap().password,
        Some("pw1".to_string()),
        "password must not be cleared for a network that was never joined"
    );
}

#[test]
fn run_quiet_suppresses_notifications_that_run_emits() {
    // The watch loop calls flow::run_quiet so a persistent failure doesn't
    // re-notify on every retry; the CLI keeps flow::run's notifications.
    let _env = EnvSandbox::new();
    let nm = fake_nm::shared();
    nm.reset();
    let mut cfg = base_config(); // no profiles → UnknownProfile path notifies

    let runner = FakeRunner::new().with_command("notify-send");
    let calls = runner.calls_handle();
    with_runner(runner, || flow::run_quiet(&mut cfg, "ghost"));
    assert!(
        !calls.borrow().iter().any(|c| c.prog == "notify-send"),
        "run_quiet must not fire desktop notifications: {:?}",
        calls.borrow()
    );

    let runner = FakeRunner::new().with_command("notify-send");
    let calls = runner.calls_handle();
    with_runner(runner, || flow::run(&mut cfg, "ghost"));
    assert!(
        calls.borrow().iter().any(|c| c.prog == "notify-send"),
        "run (CLI path) must still notify: {:?}",
        calls.borrow()
    );
}

#[test]
fn internet_ok_requires_204_and_falls_back_to_ping() {
    // Only 204 counts as internet: captive/guest portals answer 200/301/302
    // with a login page or redirect, so those must not report healthy.
    let _env = EnvSandbox::new();
    let cfg = base_config();

    let r = FakeRunner::new().with_command("curl").on(|p, _| p == "curl", ok("204"));
    assert!(with_runner(r, || breadcrumbs::status::internet_ok(&cfg)));

    for code in ["200", "301", "302"] {
        let r = FakeRunner::new()
            .with_command("curl")
            .on(|p, _| p == "curl", ok(code))
            .on(|p, _| p == "ping", fail(""));
        assert!(
            !with_runner(r, || breadcrumbs::status::internet_ok(&cfg)),
            "{code} must not count as internet"
        );
    }

    // curl absent → the ping fallback decides.
    let r = FakeRunner::new().with_command("ping").on(|p, _| p == "ping", ok(""));
    assert!(with_runner(r, || breadcrumbs::status::internet_ok(&cfg)));
}

#[test]
fn scan_list_dedups_by_ssid_keeping_strongest_signal() {
    // One entry per SSID, at its strongest signal (not the first, possibly
    // weak, listing). Hidden (empty-SSID) APs are skipped.
    let _env = EnvSandbox::new();
    let (nm, dev) = setup_wifi(&[]);
    nm.add_ap(&dev, "Cafe", 40, Security::Wpa2);
    nm.add_ap(&dev, "Cafe", 80, Security::Wpa2);
    nm.add_ap(&dev, "Office", 60, Security::Wpa3);
    nm.add_ap(&dev, "Cafe", 90, Security::Wpa2);

    let list = breadcrumbs::nm::scan_list("wlan0");
    assert_eq!(list.len(), 2, "dedup by SSID: {list:?}");
    let cafe = list.iter().find(|e| e.ssid == "Cafe").unwrap();
    assert_eq!(cafe.signal, "90", "strongest signal wins");
    let office = list.iter().find(|e| e.ssid == "Office").unwrap();
    assert_eq!(office.signal, "60");
    assert_eq!(office.security, "WPA3");
}

// ---------------------------------------------------------------------
// New features: signal-aware selection, per-network DNS, learning,
// captive portals, exit-node failover, preferred interface, enterprise
// (802.1x) connect.
// ---------------------------------------------------------------------

#[test]
fn flow_run_prefers_strongest_visible_signal_over_priority_order() {
    let _env = EnvSandbox::new();
    // "Weak" is listed first (higher priority), but "Strong" has the better
    // signal — signal-aware selection must dial Strong first.
    let (nm, _dev) = setup_wifi(&[("Weak", 40), ("Strong", 90)]);

    let mut cfg = base_config();
    cfg.networks = vec![net("Weak", Some("pw1")), net("Strong", Some("pw2"))];
    cfg.profiles.insert(
        "home".into(),
        Profile {
            networks: vec!["Weak".into(), "Strong".into()],
            ..Default::default()
        },
    );

    let outcome = with_runner(healthy_runner(), || flow::run(&mut cfg, "home"));
    match &outcome {
        flow::Outcome::Connected { ssid, .. } => assert_eq!(ssid, "Strong"),
        other => panic!("expected Connected to Strong, got {other:?}"),
    }

    assert_eq!(
        nm.activated_ssids(),
        vec!["Strong".to_string()],
        "the stronger network must be dialed, and only it"
    );
}

#[test]
fn flow_run_pins_per_network_dns_override() {
    let _env = EnvSandbox::new();
    let (nm, _dev) = setup_wifi(&[("Home", 80)]);

    let mut cfg = base_config();
    cfg.settings.dns = "1.1.1.1".into();
    let mut def = net("Home", Some("pw"));
    def.dns = Some("9.9.9.9".into());
    cfg.networks = vec![def];
    cfg.profiles.insert(
        "home".into(),
        Profile {
            networks: vec!["Home".into()],
            ..Default::default()
        },
    );

    let outcome = with_runner(healthy_runner(), || flow::run(&mut cfg, "home"));
    assert!(matches!(outcome, flow::Outcome::Connected { .. }));

    // The DNS-pinned profile must carry the per-network override, not the
    // global 1.1.1.1.
    let st = nm.state.lock().unwrap();
    let conn = st.connections.values().next().expect("a profile was saved");
    let dns = conn
        .get("ipv4")
        .and_then(|m| m.get("dns"))
        .and_then(fake_nm::value_str_list);
    assert_eq!(dns, Some(vec!["9.9.9.9".to_string()]));
}

#[test]
fn flow_run_appends_learned_ssid_to_detect_ssids() {
    let _env = EnvSandbox::new();
    let (_nm, _dev) = setup_wifi(&[("Home", 80)]);

    let mut cfg = base_config();
    cfg.networks = vec![net("Home", Some("pw"))];
    cfg.profiles.insert(
        "home".into(),
        Profile {
            networks: vec!["Home".into()],
            learn: true,
            ..Default::default()
        },
    );

    let outcome = with_runner(healthy_runner(), || flow::run(&mut cfg, "home"));
    assert!(matches!(outcome, flow::Outcome::Connected { .. }));

    assert_eq!(
        cfg.profile("home").unwrap().detect_ssids,
        vec!["Home".to_string()],
        "a successful connect on a learn=true profile must record the SSID"
    );
}

#[test]
fn classify_reports_captive_portal_when_connectivity_returns_200() {
    let _env = EnvSandbox::new();
    let (nm, dev) = setup_wifi(&[]);
    associate(&nm, &dev, "HomeWifi");
    let mut cfg = base_config();
    cfg.profiles.insert("home".into(), Profile::default());

    let runner = classify_runner("200", None);
    let class = with_runner(runner, || classify(&cfg, "home"));

    assert_eq!(class.health, Health::CaptivePortal);
    assert_eq!(class.ssid, Some("HomeWifi".to_string()));
}

#[test]
fn ensure_exit_node_failover_tries_nodes_in_priority_order() {
    // The status never shows nodeA; it always shows nodeB selected + online.
    // ensure_exit_node must therefore try nodeA (fail), then nodeB (succeed),
    // in that exact priority order.
    let _env = EnvSandbox::new();
    let json = r#"{"BackendState":"Running","Peer":{"k1":{"HostName":"nodeB","DNSName":"nodeB.ts.net.","Online":true,"ExitNode":true,"ExitNodeOption":true}}}"#;
    let runner = FakeRunner::new()
        .with_command("tailscale")
        .on_contains("tailscale", "status", ok(json))
        .on(|p, args| p == "tailscale" && args.contains(&"set"), ok(""));
    let calls = runner.calls_handle();

    let health = with_runner(runner, || {
        breadcrumbs::tailscale::ensure_exit_node(&["nodeA".into(), "nodeB".into()])
    });
    assert_eq!(health, breadcrumbs::tailscale::TsHealth::Ok);

    let sets: Vec<String> = calls
        .borrow()
        .iter()
        .filter(|c| c.args.iter().any(|a| a == "set"))
        .map(|c| c.args.join(" "))
        .collect();
    assert_eq!(
        sets,
        vec!["set --exit-node=nodeA".to_string(), "set --exit-node=nodeB".to_string()],
        "failover must try nodes in priority order"
    );
}

#[test]
fn wifi_interface_preferred_picks_named_device_over_first_wifi() {
    let _env = EnvSandbox::new();
    let nm = fake_nm::shared();
    nm.reset();
    nm.add_wifi_device("wlan0", 100);
    nm.add_wifi_device("wlan1", 100);

    assert_eq!(breadcrumbs::nm::wifi_interface_preferred(Some("wlan1")).as_deref(), Some("wlan1"));
}

#[test]
fn wifi_interface_preferred_falls_back_to_first_wifi_when_pref_missing() {
    let _env = EnvSandbox::new();
    let nm = fake_nm::shared();
    nm.reset();
    nm.add_wifi_device("wlan0", 100);
    nm.add_wifi_device("wlan1", 100);

    assert_eq!(breadcrumbs::nm::wifi_interface_preferred(Some("wlan9")).as_deref(), Some("wlan0"));
}

#[test]
fn visible_signals_dedups_by_strongest_signal() {
    let _env = EnvSandbox::new();
    let (nm, dev) = setup_wifi(&[]);
    nm.add_ap(&dev, "Cafe", 40, Security::Wpa2);
    nm.add_ap(&dev, "Cafe", 85, Security::Wpa2);
    nm.add_ap(&dev, "Office", 60, Security::Wpa2);

    let map = breadcrumbs::nm::visible_signals("wlan0");
    assert_eq!(map.get("Cafe"), Some(&85));
    assert_eq!(map.get("Office"), Some(&60));
}

#[test]
fn connect_verbose_enterprise_creates_8021x_profile() {
    let _env = EnvSandbox::new();
    let (nm, dev) = setup_wifi(&[]);
    nm.add_ap(&dev, "Corp", 80, Security::Enterprise);

    let mut def = net("Corp", Some("pw"));
    def.eap = Some("peap".into());
    def.identity = Some("user@corp".into());
    def.ca_cert = Some("/etc/ca.pem".into());

    let res = breadcrumbs::nm::connect_verbose("wlan0", &def, 8, "1.1.1.1");
    assert!(res.is_ok(), "enterprise connect should succeed: {res:?}");

    let st = nm.state.lock().unwrap();
    let (_, settings) = st.connections.iter().next().expect("a profile was saved");
    let x1 = settings.get("802-1x").expect("802-1x section");
    assert_eq!(
        x1.get("identity").and_then(|v| v.downcast_ref::<String>().ok()).as_deref(),
        Some("user@corp")
    );
    let eap = x1.get("eap").and_then(fake_nm::value_str_list);
    assert_eq!(eap.as_deref(), Some(&["peap".to_string()][..]));
    // ca-cert is a GBytes (`ay`) holding the conventional `file://` URI for
    // a filesystem path — never a bare string.
    let ca = x1.get("ca-cert").and_then(fake_nm::value_bytes);
    assert_eq!(ca.as_deref(), Some(b"file:///etc/ca.pem".as_slice()));
    let sec = settings.get("802-11-wireless-security").expect("security section");
    assert_eq!(
        sec.get("key-mgmt").and_then(|v| v.downcast_ref::<String>().ok()).as_deref(),
        Some("wpa-eap")
    );
}
