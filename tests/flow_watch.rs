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
use breadcrumbs::state::{self, State};
use breadcrumbs::util::with_runner;
use breadcrumbs::watch::{classify, Health};

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

/// Wires up the nmcli plumbing every `flow::run` call needs regardless of
/// scenario: a Wi-Fi interface exists, radio/rescan calls are no-ops, no
/// saved NM connection profiles exist yet (so every connect takes the
/// "create via `device wifi connect`" path), DNS enforcement succeeds, and
/// the device reports connected after any successful connect attempt.
fn base_nm(visible_ssids: &[&str]) -> FakeRunner {
    let visible = visible_ssids.join("\n");
    // `-f SSID,SIGNAL` lines: all SSIDs at the same (strong) signal, so
    // priority order — not signal — decides between them.
    let with_signal = visible_ssids
        .iter()
        .map(|s| format!("{s}:80"))
        .collect::<Vec<_>>()
        .join("\n");
    let runner = FakeRunner::new()
        .on_contains("nmcli", "DEVICE,TYPE", ok("wlan0:wifi"))
        .on_contains("nmcli", "radio wifi on", ok(""))
        .on_contains("nmcli", "wifi rescan", ok(""))
        // Exact matches: `-f ACTIVE,SSID` queries (which contain the
        // substring "SSID device wifi list") must NOT be answered with the
        // visible list — they go to the stateful rule below.
        .on(
            move |_prog, args| args.join(" ") == "-t -f SSID device wifi list ifname wlan0",
            ok(&visible),
        )
        .on(
            move |_prog, args| args.join(" ") == "-t -f SSID,SIGNAL device wifi list ifname wlan0",
            ok(&with_signal),
        )
        .on_contains("nmcli", "NAME,TYPE", ok("")) // no saved profiles
        .on_contains("nmcli", "GENERAL.CON-UUID", ok("uuid-1"))
        .on_contains("nmcli", "ipv4.ignore-auto-dns", ok(""))
        .on_contains("nmcli", "device reapply", ok(""))
        .on_contains("nmcli", "DEVICE,STATE", ok("wlan0:connected"));
    let calls = runner.calls_handle();
    runner.on_dynamic(
        move |_prog, args| args.join(" ") == "-t -f ACTIVE,SSID device wifi list ifname wlan0",
        move |_prog, _args| {
            // Stateful: answer with the SSID of the most recently dialed
            // connection, so connect_and_verify's post-connect SSID check
            // sees the network that was just activated (bootstrap first,
            // then the target).
            let rec = calls.borrow();
            let ssid = rec.iter().rev().find_map(|call| {
                let j = call.args.join(" ");
                if j.contains("connect") {
                    call.args
                        .iter()
                        .position(|a| a == "connect")
                        .map(|i| call.args[i + 1].clone())
                } else if j.contains("connection up") {
                    call.args
                        .iter()
                        .position(|a| a == "up")
                        .map(|i| call.args[i + 1].clone())
                } else {
                    None
                }
            });
            match ssid {
                Some(s) => ok(&format!("yes:{s}")),
                None => ok(""),
            }
        },
    )
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
    cfg.networks = vec![
        net("First", Some("pw1")),
        net("Second", Some("pw2")),
    ];
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
    let dialed_second = calls
        .borrow()
        .iter()
        .any(|c| c.prog == "nmcli" && c.args.contains(&"connect".to_string()) && c.args.iter().any(|a| a == "Second"));
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
    let class = with_runner(runner, || classify(&cfg, "ghost"));
    let health = class.health;
    let ssid = class.ssid;

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
    let class = with_runner(runner, || classify(&cfg, "away"));
    let health = class.health;

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
    let class = with_runner(runner, || classify(&cfg, "away"));
    let health = class.health;
    let ssid = class.ssid;

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
    let class = with_runner(runner, || classify(&cfg, "home"));
    let health = class.health;
    let ssid = class.ssid;

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
    let class = with_runner(runner, || classify(&cfg, "work"));
    let health = class.health;

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
    let class = with_runner(runner, || classify(&cfg, "work"));
    let health = class.health;

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
    let class = with_runner(runner, || classify(&cfg, "work"));
    let health = class.health;

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
    let class = with_runner(runner, || classify(&cfg, "work"));
    let health = class.health;

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

    let runner = base_nm(&["Corp"]).with_command("tailscale");
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
}

#[test]
fn classify_reports_down_tailscale_manual_when_no_exit_node_configured() {
    // An unset exit node needs human action (config edit), so it must
    // classify as DownTailscaleManual — not DownTailscaleOther, which would
    // make the watcher spin auto-recovery forever.
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
        .on(|prog, _| prog == "curl", ok("204"));
    let class = with_runner(runner, || classify(&cfg, "work"));
    let health = class.health;
    let ssid = class.ssid;

    assert_eq!(health, Health::DownTailscaleManual);
    assert_eq!(ssid, Some("CorpWifi".to_string()));
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
    let health =
        with_runner(runner, || {
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
    let mut cfg = base_config();
    cfg.networks = vec![net("First", Some("pw1"))];
    cfg.profiles.insert(
        "home".into(),
        Profile {
            networks: vec!["First".into()],
            ..Default::default()
        },
    );

    let runner = FakeRunner::new()
        .on_contains("nmcli", "DEVICE,TYPE", ok("wlan0:wifi"))
        .on_contains("nmcli", "radio wifi on", ok(""))
        .on_contains("nmcli", "wifi rescan", ok(""))
        .on(
            |_p, args| args.join(" ") == "-t -f SSID device wifi list ifname wlan0",
            ok("First"),
        )
        .on(
            |_p, args| args.join(" ") == "-t -f SSID,SIGNAL device wifi list ifname wlan0",
            ok("First:80"),
        )
        .on_contains("nmcli", "NAME,TYPE", ok(""))
        .on_contains("nmcli", "GENERAL.CON-UUID", ok("uuid-1"))
        .on_contains("nmcli", "ipv4.ignore-auto-dns", ok(""))
        .on_contains("nmcli", "device reapply", ok(""))
        .on_contains("nmcli", "DEVICE,STATE", ok("wlan0:connected"))
        // The connect itself succeeds...
        .on_contains("nmcli", "device wifi connect First", ok(""))
        // ...but the device reports being on a different network.
        .on(
            |_p, args| args.join(" ") == "-t -f ACTIVE,SSID device wifi list ifname wlan0",
            ok("yes:OtherNet"),
        );
    let outcome = with_runner(runner, || flow::run(&mut cfg, "home"));

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
    // One line per BSSID: the same SSID broadcast by several APs must show
    // once, at its strongest signal (not the first, possibly weak, listing).
    let _env = EnvSandbox::new();
    let runner = FakeRunner::new().on_contains(
        "nmcli",
        "SSID,SIGNAL,SECURITY",
        ok("Cafe:40:WPA2\nCafe:80:WPA2\nOffice:60:WPA3\nCafe:90 %:WPA2\n:70:WPA2"),
    );
    let list = with_runner(runner, || breadcrumbs::nm::scan_list("wlan0"));

    assert_eq!(list.len(), 2, "dedup by SSID, hidden (empty SSID) skipped: {list:?}");
    let cafe = list.iter().find(|e| e.ssid == "Cafe").unwrap();
    assert_eq!(cafe.signal, "90 %", "strongest signal wins");
    let office = list.iter().find(|e| e.ssid == "Office").unwrap();
    assert_eq!(office.signal, "60");
}

// ---------------------------------------------------------------------
// New features: signal-aware selection, per-network DNS, learning,
// captive portals, exit-node failover, preferred interface, enterprise
// (802.1x) connect.
// ---------------------------------------------------------------------

#[test]
fn flow_run_prefers_strongest_visible_signal_over_priority_order() {
    let _env = EnvSandbox::new();
    let mut cfg = base_config();
    cfg.networks = vec![net("Weak", Some("pw1")), net("Strong", Some("pw2"))];
    cfg.profiles.insert(
        "home".into(),
        Profile {
            networks: vec!["Weak".into(), "Strong".into()],
            ..Default::default()
        },
    );

    // "Weak" is listed first (higher priority), but "Strong" has the better
    // signal — signal-aware selection must dial Strong first.
    let runner = FakeRunner::new()
        .on_contains("nmcli", "DEVICE,TYPE", ok("wlan0:wifi"))
        .on_contains("nmcli", "radio wifi on", ok(""))
        .on_contains("nmcli", "wifi rescan", ok(""))
        .on(
            |_p, args| args.join(" ") == "-t -f SSID device wifi list ifname wlan0",
            ok("Weak\nStrong"),
        )
        .on(
            |_p, args| args.join(" ") == "-t -f SSID,SIGNAL device wifi list ifname wlan0",
            ok("Weak:40\nStrong:90"),
        )
        .on_contains("nmcli", "NAME,TYPE", ok(""))
        .on_contains("nmcli", "GENERAL.CON-UUID", ok("uuid-1"))
        .on_contains("nmcli", "ipv4.ignore-auto-dns", ok(""))
        .on_contains("nmcli", "device reapply", ok(""))
        .on_contains("nmcli", "DEVICE,STATE", ok("wlan0:connected"))
        .on(
            |_p, args| args.join(" ") == "-t -f ACTIVE,SSID device wifi list ifname wlan0",
            ok("yes:Strong"),
        )
        .on(
            |p, args| p == "nmcli" && args.contains(&"connect") && args.contains(&"Strong"),
            ok(""),
        )
        .on(|p, _| p == "curl", ok("204"))
        .with_command("curl");
    let calls = runner.calls_handle();

    let outcome = with_runner(runner, || flow::run(&mut cfg, "home"));
    match &outcome {
        flow::Outcome::Connected { ssid, .. } => assert_eq!(ssid, "Strong"),
        other => panic!("expected Connected to Strong, got {other:?}"),
    }

    let dialed = |s: &str| {
        calls.borrow().iter().any(|c| {
            c.args.contains(&"connect".to_string()) && c.args.iter().any(|a| a == s)
        })
    };
    assert!(dialed("Strong"), "the stronger network must be dialed");
    assert!(
        !dialed("Weak"),
        "the weaker network must not be dialed despite higher priority"
    );
}

#[test]
fn flow_run_pins_per_network_dns_override() {
    let _env = EnvSandbox::new();
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

    let runner = allow_connects(base_nm(&["Home"]), &["Home"])
        .on(|p, _| p == "curl", ok("204"))
        .with_command("curl");
    let calls = runner.calls_handle();
    let outcome = with_runner(runner, || flow::run(&mut cfg, "home"));
    assert!(matches!(outcome, flow::Outcome::Connected { .. }));

    // The DNS-pinning `connection modify` must carry the per-network override,
    // not the global 1.1.1.1.
    let dns_arg = calls.borrow().iter().any(|c| {
        c.args.join(" ").contains("ipv4.dns") && c.args.iter().any(|a| a == "9.9.9.9")
    });
    assert!(dns_arg, "per-network DNS override must reach nmcli");
}

#[test]
fn flow_run_appends_learned_ssid_to_detect_ssids() {
    let _env = EnvSandbox::new();
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

    let runner = allow_connects(base_nm(&["Home"]), &["Home"])
        .on(|p, _| p == "curl", ok("204"))
        .with_command("curl");
    let outcome = with_runner(runner, || flow::run(&mut cfg, "home"));
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
    let mut cfg = base_config();
    cfg.profiles.insert("home".into(), Profile::default());

    let runner = FakeRunner::new()
        .on_contains("nmcli", "DEVICE,TYPE", ok("wlan0:wifi"))
        .on_contains("nmcli", "ACTIVE,SSID", ok("yes:HomeWifi"))
        .on_contains("nmcli", "IP4.ADDRESS", ok("192.168.1.50/24"))
        .with_command("curl")
        .on(|p, _| p == "curl", ok("200"));
    let class = with_runner(runner, || classify(&cfg, "home"));

    assert_eq!(class.health, Health::CaptivePortal);
    assert_eq!(class.ssid, Some("HomeWifi".to_string()));
}

#[test]
fn ensure_exit_node_failover_tries_nodes_in_priority_order() {
    let _env = EnvSandbox::new();
    // The status never shows nodeA; it always shows nodeB selected + online.
    // ensure_exit_node must therefore try nodeA (fail), then nodeB (succeed),
    // in that exact priority order.
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
    let runner = FakeRunner::new().on_contains(
        "nmcli",
        "DEVICE,TYPE",
        ok("wlan0:wifi\nwlan1:wifi"),
    );
    let iface = with_runner(runner, || {
        breadcrumbs::nm::wifi_interface_preferred(Some("wlan1"))
    });
    assert_eq!(iface.as_deref(), Some("wlan1"));
}

#[test]
fn wifi_interface_preferred_falls_back_to_first_wifi_when_pref_missing() {
    let runner = FakeRunner::new().on_contains(
        "nmcli",
        "DEVICE,TYPE",
        ok("wlan0:wifi\nwlan1:wifi"),
    );
    let iface = with_runner(runner, || {
        breadcrumbs::nm::wifi_interface_preferred(Some("wlan9"))
    });
    assert_eq!(iface.as_deref(), Some("wlan0"));
}

#[test]
fn visible_signals_dedups_by_strongest_signal() {
    let runner = FakeRunner::new().on_contains(
        "nmcli",
        "SSID,SIGNAL",
        ok("Cafe:40\nCafe:85\nOffice:60\n:90"),
    );
    let map = with_runner(runner, || breadcrumbs::nm::visible_signals("wlan0"));
    assert_eq!(map.get("Cafe"), Some(&85));
    assert_eq!(map.get("Office"), Some(&60));
    assert!(!map.contains_key(""), "hidden/empty SSID must be skipped");
}

#[test]
fn connect_verbose_enterprise_creates_8021x_profile() {
    let _env = EnvSandbox::new();
    let mut def = net("Corp", Some("pw"));
    def.eap = Some("peap".into());
    def.identity = Some("user@corp".into());
    def.ca_cert = Some("/etc/ca.pem".into());

    // No saved profile (NAME,TYPE empty), so the enterprise create path runs.
    let runner = FakeRunner::new()
        .on_contains("nmcli", "NAME,TYPE", ok(""))
        .on(
            |p, args| p == "nmcli" && args.contains(&"add") && args.contains(&"connection"),
            ok(""),
        )
        .on(|p, args| p == "nmcli" && args.contains(&"up"), ok(""))
        .on_contains("nmcli", "GENERAL.CON-UUID", ok("uuid-1"))
        .on_contains("nmcli", "ipv4.ignore-auto-dns", ok(""))
        .on_contains("nmcli", "device reapply", ok(""));
    let calls = runner.calls_handle();

    let res = with_runner(runner, || {
        breadcrumbs::nm::connect_verbose("wlan0", &def, 8, "1.1.1.1")
    });
    assert!(res.is_ok(), "enterprise connect should succeed: {res:?}");

    let calls_ref = calls.borrow();
    let add = calls_ref
        .iter()
        .find(|c| c.args.contains(&"add".to_string()) && c.args.contains(&"connection".to_string()))
        .expect("enterprise path must create a profile via `connection add`");
    let joined = add.args.join(" ");
    assert!(joined.contains("wpa-eap"));
    assert!(joined.contains("peap"));
    assert!(joined.contains("user@corp"));
    assert!(joined.contains("/etc/ca.pem"));
    assert!(joined.contains("802-1x.password"));
}
