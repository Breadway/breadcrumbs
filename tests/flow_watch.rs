//! In-process tests for the actual state machine (`flow::run`) and the watch
//! loop's health classification (`watch::classify`), driven entirely through
//! a faked `breadcrumbs::util::Runner` (see `tests/common`) — no subprocess
//! is ever spawned. This complements `tests/cli.rs`'s black-box coverage
//! (which spawns the real binary against fake-bin shell scripts) with fast,
//! precise coverage of the logic itself: candidate priority order, the
//! bootstrap+Tailscale gate, and every `watch::Health` transition.

mod common;

use std::collections::BTreeMap;

use bread_utils::bread_client::BreadEvent;
use breadcrumbs::bread_events;
use breadcrumbs::config::{Config, NetworkDef, Profile, Settings};
use breadcrumbs::flow;
use breadcrumbs::nm;
use breadcrumbs::state::{self, State};
use breadcrumbs::util::with_runner;
use breadcrumbs::watch::{classify, Health};

use common::{fail, ok, EnvSandbox, FakeRunner};

fn net(ssid: &str, password: Option<&str>) -> NetworkDef {
    NetworkDef {
        ssid: ssid.to_string(),
        password: password.map(str::to_string),
        hidden: false,
    }
}

fn hidden_net(ssid: &str, password: Option<&str>) -> NetworkDef {
    NetworkDef {
        ssid: ssid.to_string(),
        password: password.map(str::to_string),
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

/// Wires up the nmcli plumbing every `flow::run` call needs regardless of
/// scenario: a Wi-Fi interface exists, radio/rescan calls are no-ops, no
/// saved NM connection profiles exist yet (so every connect takes the
/// "create via `device wifi connect`" path), DNS enforcement succeeds, and
/// the device reports connected after any successful connect attempt.
fn base_nm(visible_ssids: &[&str]) -> FakeRunner {
    let visible = visible_ssids.join("\n");
    FakeRunner::new()
        .on_contains("nmcli", "DEVICE,TYPE", ok("wlan0:wifi"))
        .on_contains("nmcli", "radio wifi on", ok(""))
        .on_contains("nmcli", "wifi rescan", ok(""))
        .on_contains("nmcli", "-f SSID device wifi list", ok(&visible))
        .on_contains("nmcli", "NAME,TYPE", ok("")) // no saved profiles
        .on_contains("nmcli", "GENERAL.CON-UUID", ok("uuid-1"))
        .on_contains("nmcli", "ipv4.ignore-auto-dns", ok(""))
        .on_contains("nmcli", "device reapply", ok(""))
        .on_contains("nmcli", "DEVICE,STATE", ok("wlan0:connected"))
}

/// A successful `device wifi connect <ssid> ...` for every ssid in `ssids`.
fn allow_connects(runner: FakeRunner, ssids: &[&'static str]) -> FakeRunner {
    ssids.iter().fold(runner, |r, ssid| {
        let ssid: &'static str = ssid;
        r.on(
            move |prog, args| prog == "nmcli" && args.contains(&"connect") && args.contains(&ssid),
            ok(""),
        )
    })
}

// ---------------------------------------------------------------------
// flow::run — candidate priority (pass 1 / pass 2)
// ---------------------------------------------------------------------

#[test]
fn flow_run_connects_to_first_visible_candidate_in_priority_order() {
    let _env = EnvSandbox::new();

    let mut cfg = base_config();
    cfg.networks = vec![net("First", Some("pw1")), net("Second", Some("pw2"))];
    cfg.profiles.insert(
        "home".into(),
        Profile {
            networks: vec!["First".into(), "Second".into()],
            ..Default::default()
        },
    );

    let runner = allow_connects(base_nm(&["First", "Second"]), &["First", "Second"])
        .on(|prog, _| prog == "curl", ok("204"))
        .with_command("curl");
    let calls = runner.calls_handle();

    let outcome = with_runner(runner, || flow::run(&mut cfg, "home"));

    match outcome {
        flow::Outcome::Connected { ssid, note } => {
            assert_eq!(ssid, "First");
            assert_eq!(note, None);
        }
        other => panic!("expected Connected, got {other:?}"),
    }

    // Priority order actually mattered: "Second" was never dialed even
    // though it was visible and would have succeeded too.
    let dialed_second = calls.borrow().iter().any(|c| {
        c.prog == "nmcli"
            && c.args.contains(&"connect".to_string())
            && c.args.iter().any(|a| a == "Second")
    });
    assert!(!dialed_second, "connected to Second when First should win");

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

    let mut cfg = base_config();
    // "Ghost" is neither visible nor hidden, so pass 1 *and* pass 2 both
    // skip it outright — it should never be dialed.
    cfg.networks = vec![
        net("Ghost", Some("pw-ghost")),
        hidden_net("Shadow", Some("pw-shadow")),
    ];
    cfg.profiles.insert(
        "away".into(),
        Profile {
            networks: vec!["Ghost".into(), "Shadow".into()],
            ..Default::default()
        },
    );

    // Neither SSID shows up in the scan — "Shadow" is only reachable via the
    // pass-2 "hidden and unseen" path.
    let runner = allow_connects(base_nm(&[]), &["Shadow"])
        .on(|prog, _| prog == "curl", ok("204"))
        .with_command("curl");
    let calls = runner.calls_handle();

    let outcome = with_runner(runner, || flow::run(&mut cfg, "away"));

    match outcome {
        flow::Outcome::Connected { ssid, .. } => assert_eq!(ssid, "Shadow"),
        other => panic!("expected Connected to Shadow, got {other:?}"),
    }
    let dialed_ghost = calls
        .borrow()
        .iter()
        .any(|c| c.args.iter().any(|a| a == "Ghost") && c.args.contains(&"connect".to_string()));
    assert!(!dialed_ghost, "Ghost should never have been dialed");
}

#[test]
fn flow_run_unknown_profile_short_circuits_before_touching_nm() {
    let _env = EnvSandbox::new();
    let mut cfg = base_config();

    let runner = FakeRunner::new(); // no rules at all
    let calls = runner.calls_handle();

    let outcome = with_runner(runner, || flow::run(&mut cfg, "does-not-exist"));

    assert!(matches!(outcome, flow::Outcome::UnknownProfile(p) if p == "does-not-exist"));
    // The only `Runner::run` call on this path is `notify`/`log`'s own
    // `date` timestamp lookup — nmcli (or anything network-related) is
    // never touched for a profile that doesn't exist.
    assert!(
        calls.borrow().iter().all(|c| c.prog != "nmcli"),
        "unknown-profile path should never shell out to nmcli: {:?}",
        calls.borrow()
    );
}

// ---------------------------------------------------------------------
// flow::run — bootstrap + Tailscale gating
// ---------------------------------------------------------------------

fn tailscale_json_ok(exit_node: &str) -> String {
    format!(
        r#"{{"BackendState":"Running","Peer":{{"k1":{{"HostName":"{exit_node}","DNSName":"{exit_node}.ts.net.","Online":true,"ExitNode":true,"ExitNodeOption":true}}}}}}"#
    )
}

fn tailscale_json_missing() -> &'static str {
    r#"{"BackendState":"Running","Peer":{}}"#
}

#[test]
fn flow_run_moves_past_bootstrap_once_tailscale_is_healthy() {
    let _env = EnvSandbox::new();

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

    let runner = allow_connects(base_nm(&["Guest", "Corp"]), &["Guest", "Corp"])
        .with_command("curl")
        .with_command("tailscale")
        .on(|prog, _| prog == "curl", ok("204"))
        .on_contains("tailscale", "status", ok(&tailscale_json_ok("exitnode")))
        .on_contains("tailscale", "set", ok(""));
    let calls = runner.calls_handle();

    let outcome = with_runner(runner, || flow::run(&mut cfg, "work"));

    match outcome {
        flow::Outcome::Connected { ssid, .. } => assert_eq!(ssid, "Corp"),
        other => panic!("expected Connected to Corp, got {other:?}"),
    }
    // Both the bootstrap and target connects used a local password, so both
    // should have been cleared once NetworkManager took over.
    assert_eq!(cfg.network("Guest").unwrap().password, None);
    assert_eq!(cfg.network("Corp").unwrap().password, None);

    let dialed_guest = calls
        .borrow()
        .iter()
        .any(|c| c.args.iter().any(|a| a == "Guest") && c.args.contains(&"connect".to_string()));
    assert!(dialed_guest, "bootstrap should have been dialed first");
}

#[test]
fn flow_run_stays_on_bootstrap_and_never_dials_target_when_tailscale_unhealthy() {
    let _env = EnvSandbox::new();

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

    let runner = allow_connects(base_nm(&["Guest", "Corp"]), &["Guest", "Corp"])
        .with_command("curl")
        .with_command("tailscale")
        .on(|prog, _| prog == "curl", ok("204"))
        .on(
            |prog, args| prog == "tailscale" && args.contains(&"status"),
            ok(tailscale_json_missing()),
        )
        .on(
            |prog, args| prog == "tailscale" && args.contains(&"set"),
            ok(""),
        );
    let calls = runner.calls_handle();

    let outcome = with_runner(runner, || flow::run(&mut cfg, "work"));

    match &outcome {
        flow::Outcome::TailscaleError { ssid, health } => {
            assert_eq!(ssid.as_deref(), Some("Guest"));
            assert_eq!(*health, breadcrumbs::tailscale::TsHealth::ExitNodeMissing);
        }
        other => panic!("expected TailscaleError, got {other:?}"),
    }

    let dialed_corp = calls
        .borrow()
        .iter()
        .any(|c| c.args.iter().any(|a| a == "Corp") && c.args.contains(&"connect".to_string()));
    assert!(
        !dialed_corp,
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
    let cfg = base_config(); // no profiles at all

    let runner = FakeRunner::new();
    let calls = runner.calls_handle();
    let (health, ssid) = with_runner(runner, || classify(&cfg, "ghost"));

    assert_eq!(health, Health::UnknownProfile);
    assert_eq!(ssid, None);
    assert!(calls.borrow().is_empty());
}

#[test]
fn classify_reports_no_adapter_when_wifi_interface_absent() {
    let _env = EnvSandbox::new();
    let mut cfg = base_config();
    cfg.profiles.insert("away".into(), Profile::default());

    // `device status` succeeds but lists no wifi-type device.
    let runner = FakeRunner::new().on_contains("nmcli", "DEVICE,TYPE", ok("eth0:ethernet"));
    let (health, _) = with_runner(runner, || classify(&cfg, "away"));

    assert_eq!(health, Health::NoAdapter);
}

#[test]
fn classify_reports_down_no_net_when_internet_check_fails() {
    let _env = EnvSandbox::new();
    let mut cfg = base_config();
    cfg.profiles.insert("away".into(), Profile::default());

    let runner = FakeRunner::new()
        .on_contains("nmcli", "DEVICE,TYPE", ok("wlan0:wifi"))
        .on_contains("nmcli", "ACTIVE,SSID", ok("yes:HomeWifi"))
        .on_contains("nmcli", "IP4.ADDRESS", ok("192.168.1.50/24"))
        .on(|prog, _| prog == "curl" || prog == "ping", fail(""));
    let (health, ssid) = with_runner(runner, || classify(&cfg, "away"));

    assert_eq!(health, Health::DownNoNet);
    assert_eq!(ssid, Some("HomeWifi".to_string()));
}

#[test]
fn classify_reports_up_when_healthy_and_tailscale_not_required() {
    let _env = EnvSandbox::new();
    let mut cfg = base_config();
    cfg.profiles.insert("home".into(), Profile::default()); // tailscale: false

    let runner = FakeRunner::new()
        .on_contains("nmcli", "DEVICE,TYPE", ok("wlan0:wifi"))
        .on_contains("nmcli", "ACTIVE,SSID", ok("yes:HomeWifi"))
        .on_contains("nmcli", "IP4.ADDRESS", ok("192.168.1.50/24"))
        .with_command("curl")
        .on(|prog, _| prog == "curl", ok("204"));
    let (health, ssid) = with_runner(runner, || classify(&cfg, "home"));

    assert_eq!(health, Health::Up);
    assert_eq!(ssid, Some("HomeWifi".to_string()));
}

#[test]
fn classify_reports_down_tailscale_manual_when_not_installed() {
    let _env = EnvSandbox::new();
    let mut cfg = base_config();
    cfg.profiles.insert(
        "work".into(),
        Profile {
            tailscale: true,
            ..Default::default()
        },
    );

    // No `with_command("tailscale")`, so `tailscale::installed()` is false —
    // `status::gather` never even tries to run the `tailscale` binary.
    let runner = FakeRunner::new()
        .on_contains("nmcli", "DEVICE,TYPE", ok("wlan0:wifi"))
        .on_contains("nmcli", "ACTIVE,SSID", ok("yes:CorpWifi"))
        .on_contains("nmcli", "IP4.ADDRESS", ok("10.0.0.5/24"))
        .with_command("curl")
        .on(|prog, _| prog == "curl", ok("204"));
    let (health, _) = with_runner(runner, || classify(&cfg, "work"));

    assert_eq!(health, Health::DownTailscaleManual);
}

#[test]
fn classify_reports_down_tailscale_manual_when_needs_login() {
    let _env = EnvSandbox::new();
    let mut cfg = base_config();
    cfg.profiles.insert(
        "work".into(),
        Profile {
            tailscale: true,
            ..Default::default()
        },
    );

    let runner = FakeRunner::new()
        .on_contains("nmcli", "DEVICE,TYPE", ok("wlan0:wifi"))
        .on_contains("nmcli", "ACTIVE,SSID", ok("yes:CorpWifi"))
        .on_contains("nmcli", "IP4.ADDRESS", ok("10.0.0.5/24"))
        .with_command("curl")
        .with_command("tailscale")
        .on(|prog, _| prog == "curl", ok("204"))
        .on(
            |prog, args| prog == "tailscale" && args.contains(&"status"),
            ok(r#"{"BackendState":"NeedsLogin"}"#),
        );
    let (health, _) = with_runner(runner, || classify(&cfg, "work"));

    assert_eq!(health, Health::DownTailscaleManual);
}

#[test]
fn classify_reports_down_tailscale_other_when_exit_node_offline() {
    let _env = EnvSandbox::new();
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
    let runner = FakeRunner::new()
        .on_contains("nmcli", "DEVICE,TYPE", ok("wlan0:wifi"))
        .on_contains("nmcli", "ACTIVE,SSID", ok("yes:CorpWifi"))
        .on_contains("nmcli", "IP4.ADDRESS", ok("10.0.0.5/24"))
        .with_command("curl")
        .with_command("tailscale")
        .on(|prog, _| prog == "curl", ok("204"))
        .on(
            |prog, args| prog == "tailscale" && args.contains(&"status"),
            ok(json),
        );
    let (health, _) = with_runner(runner, || classify(&cfg, "work"));

    assert_eq!(health, Health::DownTailscaleOther);
}

#[test]
fn classify_reports_up_when_tailscale_healthy() {
    let _env = EnvSandbox::new();
    let mut cfg = base_config();
    cfg.settings.exit_node = "exitnode".into();
    cfg.profiles.insert(
        "work".into(),
        Profile {
            tailscale: true,
            ..Default::default()
        },
    );

    let runner = FakeRunner::new()
        .on_contains("nmcli", "DEVICE,TYPE", ok("wlan0:wifi"))
        .on_contains("nmcli", "ACTIVE,SSID", ok("yes:CorpWifi"))
        .on_contains("nmcli", "IP4.ADDRESS", ok("10.0.0.5/24"))
        .with_command("curl")
        .with_command("tailscale")
        .on(|prog, _| prog == "curl", ok("204"))
        .on_contains("tailscale", "status", ok(&tailscale_json_ok("exitnode")));
    let (health, _) = with_runner(runner, || classify(&cfg, "work"));

    assert_eq!(health, Health::Up);
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

    let acted = bread_events::handle_command(&command_event(
        "bread.command.crumbs.set_profile",
        serde_json::json!({ "profile": "home" }),
    ));

    assert!(acted, "known profile must persist");
    assert_eq!(State::load("away").profile, "home");
}

#[test]
fn set_profile_command_rejects_unknown_profile() {
    let _env = EnvSandbox::new();
    let cfg = Config::load().expect("fresh config");
    state::set_profile(&cfg, "away").unwrap();

    let acted = bread_events::handle_command(&command_event(
        "bread.command.crumbs.set_profile",
        serde_json::json!({ "profile": "bogus" }),
    ));

    assert!(!acted);
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

    let acted = bread_events::handle_command(&command_event(
        "bread.command.crumbs.set_profile",
        serde_json::json!({}),
    ));

    assert!(!acted);
    assert_eq!(State::load("away").profile, "away");
}

#[test]
fn handle_command_ignores_unrecognized_verb() {
    let _env = EnvSandbox::new();
    let cfg = Config::load().expect("fresh config");
    state::set_profile(&cfg, "away").unwrap();

    let acted = bread_events::handle_command(&command_event(
        "bread.command.crumbs.pin",
        serde_json::json!({}),
    ));

    assert!(!acted);
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

    assert!(!bread_events::handle_command(&command_event(
        "bread.command.clip.clear",
        serde_json::json!({}),
    )));
    assert!(!bread_events::handle_command(&command_event(
        "bread.crumbs.profile.changed",
        serde_json::json!({ "from": "away", "to": "home" }),
    )));
    assert_eq!(State::load("away").profile, "away");
}

// ---------------------------------------------------------------------
// PSK never on argv (first connect feeds nmcli --ask on stdin)
// ---------------------------------------------------------------------

fn assert_psk_not_on_argv(calls: &[common::RecordedCall], psk: &str) {
    for c in calls {
        if c.prog != "nmcli" {
            continue;
        }
        assert!(
            !c.args.iter().any(|a| a == psk),
            "PSK leaked onto nmcli argv: {:?}",
            c.args
        );
    }
}

#[test]
fn connect_verbose_create_feeds_psk_on_stdin_never_argv() {
    let runner = FakeRunner::new()
        .on_contains("nmcli", "NAME,TYPE", ok(""))
        .on_contains("nmcli", "connect", ok(""))
        .on_contains("nmcli", "GENERAL.CON-UUID", ok("uuid-1"))
        .on_contains("nmcli", "ipv4.ignore-auto-dns", ok(""))
        .on_contains("nmcli", "device reapply", ok(""));
    let calls = runner.calls_handle();

    let net = net("Cafe", Some("super-secret-psk"));
    let result = with_runner(runner, || nm::connect_verbose("wlan0", &net, 8, "1.1.1.1"));
    assert!(result.is_ok(), "{result:?}");

    let calls = calls.borrow();
    assert_psk_not_on_argv(&calls, "super-secret-psk");
    let connect = calls
        .iter()
        .find(|c| c.prog == "nmcli" && c.args.iter().any(|a| a == "connect"))
        .expect("expected device wifi connect");
    assert!(
        connect.args.iter().any(|a| a == "--ask"),
        "create path must use --ask: {:?}",
        connect.args
    );
    assert_eq!(connect.stdin.as_deref(), Some("super-secret-psk\n"));
}

#[test]
fn connect_verbose_reuse_feeds_psk_on_stdin_never_argv() {
    let runner = FakeRunner::new()
        .on_contains("nmcli", "NAME,TYPE", ok("Cafe:802-11-wireless"))
        .on_contains("nmcli", "connection modify", ok(""))
        .on_contains("nmcli", "connection up", ok(""))
        .on_contains("nmcli", "GENERAL.CON-UUID", ok("uuid-1"))
        .on_contains("nmcli", "ipv4.ignore-auto-dns", ok(""))
        .on_contains("nmcli", "device reapply", ok(""));
    let calls = runner.calls_handle();

    let net = net("Cafe", Some("super-secret-psk"));
    let result = with_runner(runner, || nm::connect_verbose("wlan0", &net, 8, "1.1.1.1"));
    assert!(result.is_ok(), "{result:?}");

    let calls = calls.borrow();
    assert_psk_not_on_argv(&calls, "super-secret-psk");
    let up = calls
        .iter()
        .find(|c| c.prog == "nmcli" && c.args.iter().any(|a| a == "up"))
        .expect("expected connection up");
    assert!(
        up.args.iter().any(|a| a == "--ask"),
        "reuse path must use --ask: {:?}",
        up.args
    );
    assert_eq!(up.stdin.as_deref(), Some("super-secret-psk\n"));
    // Clearing the stored PSK uses an empty argv value, never the secret.
    let cleared = calls.iter().any(|c| {
        c.prog == "nmcli"
            && c.args.iter().any(|a| a == "802-11-wireless-security.psk")
            && c.args.last().is_some_and(|a| a.is_empty())
    });
    assert!(cleared, "reuse+password should reset stored PSK: {calls:?}");
}
