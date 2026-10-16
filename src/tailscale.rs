use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::notify::{log, notify, Urgency};
use crate::util::{command_exists, run};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TsHealth {
    /// Backend Running and the requested exit node is selected + online.
    Ok,
    NotInstalled,
    NeedsLogin,
    Stopped,
    /// The exit node host is not present / not advertising as an exit node.
    ExitNodeMissing,
    /// The exit node exists but is offline.
    ExitNodeOffline,
    /// The profile requires an exit node but none is configured
    /// (`settings.exit_node` / per-profile `exit_node` empty). Cannot be
    /// auto-fixed; the user must configure one.
    NoExitNode,
    Error(String),
}

impl TsHealth {
    pub fn is_ok(&self) -> bool {
        matches!(self, TsHealth::Ok)
    }

    /// Wire name for `bread.crumbs.*` event payloads (variant name, like
    /// `Health::as_str`).
    pub fn state_str(&self) -> &'static str {
        match self {
            TsHealth::Ok => "Ok",
            TsHealth::NotInstalled => "NotInstalled",
            TsHealth::NeedsLogin => "NeedsLogin",
            TsHealth::Stopped => "Stopped",
            TsHealth::ExitNodeMissing => "ExitNodeMissing",
            TsHealth::ExitNodeOffline => "ExitNodeOffline",
            TsHealth::NoExitNode => "NoExitNode",
            TsHealth::Error(_) => "Error",
        }
    }

    pub fn describe(&self) -> String {
        match self {
            TsHealth::Ok => "ok".into(),
            TsHealth::NotInstalled => "tailscale not installed".into(),
            TsHealth::NeedsLogin => "not logged in (run: tailscale up)".into(),
            TsHealth::Stopped => "backend stopped".into(),
            TsHealth::ExitNodeMissing => "exit node not found in tailnet".into(),
            TsHealth::ExitNodeOffline => "exit node is offline".into(),
            TsHealth::NoExitNode => "no exit node configured".into(),
            TsHealth::Error(e) => format!("error: {e}"),
        }
    }
}

pub fn installed() -> bool {
    command_exists("tailscale")
}

fn status_json() -> Option<Value> {
    let o = run("tailscale", &["status", "--json"], Duration::from_secs(8));
    if o.stdout.trim().is_empty() {
        return None;
    }
    serde_json::from_str(&o.stdout).ok()
}

fn backend_state(v: &Value) -> String {
    v.get("BackendState")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Does any peer named `node` advertise as an exit node, and is it online /
/// currently selected?
fn exit_node_state(
    v: &Value,
    node: &str,
) -> (
    bool, /*exists*/
    bool, /*online*/
    bool, /*selected*/
) {
    // Strong signal: ExitNodeStatus is populated when an exit node is active.
    let ens_online = v
        .get("ExitNodeStatus")
        .and_then(|e| e.get("Online"))
        .and_then(Value::as_bool);

    let want = node.trim().to_lowercase();
    let mut exists = false;
    let mut online = false;
    let mut selected = false;

    if let Some(peers) = v.get("Peer").and_then(Value::as_object) {
        for (_k, p) in peers {
            let host = p
                .get("HostName")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_lowercase();
            let dns = p
                .get("DNSName")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim_end_matches('.')
                .to_lowercase();
            // Exit nodes can be configured by hostname, MagicDNS name or
            // Tailscale IP (`tailscale set --exit-node` accepts all three).
            let by_ip = p
                .get("TailscaleIPs")
                .and_then(Value::as_array)
                .is_some_and(|ips| ips.iter().any(|ip| ip.as_str() == Some(want.as_str())));
            let matches = host == want
                || dns == want.trim_end_matches('.')
                || dns.split('.').next() == Some(want.as_str())
                || by_ip;
            let advertises = p
                .get("ExitNodeOption")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if matches && advertises {
                exists = true;
                online = p.get("Online").and_then(Value::as_bool).unwrap_or(false);
                selected = p.get("ExitNode").and_then(Value::as_bool).unwrap_or(false);
                break;
            }
        }
    }

    if selected {
        if let Some(o) = ens_online {
            online = o;
        }
    }
    (exists, online, selected)
}

fn extract_url(line: &str) -> Option<String> {
    line.split_whitespace()
        .find(|s| s.starts_with("https://"))
        .map(str::to_string)
}

static LOGIN_INFLIGHT: AtomicBool = AtomicBool::new(false);

/// Kick off browser-based Tailscale login in the background and return
/// immediately. The watch loop must never block on interactive auth, so the
/// actual login + browser flow runs on its own thread; the caller just
/// keeps reporting `NeedsLogin` (→ stay on the bootstrap network) until a
/// later poll observes the backend come up. The guard collapses repeated
/// triggers into a single in-flight attempt.
fn login_and_open() {
    if LOGIN_INFLIGHT.swap(true, Ordering::SeqCst) {
        return;
    }
    thread::spawn(|| {
        run_login();
        LOGIN_INFLIGHT.store(false, Ordering::SeqCst);
    });
}

enum LoginAttempt {
    /// Login ran (URL opened and finished, timed out, or nothing to do).
    Done,
    /// The command was refused before producing a URL (not an operator,
    /// or sudo wants a password): try the next way.
    Refused(String),
}

/// Log in to Tailscale. Tries plain `tailscale login` first, which works
/// without root when the user is the Tailscale operator
/// (`tailscale set --operator=$USER`), then `sudo -n tailscale login`,
/// which works with a passwordless sudo rule. `-n` because this runs from
/// a daemon with no terminal: a sudo that wants a password fails at once
/// instead of hanging.
fn run_login() {
    let attempts: [(&str, &[&str]); 2] = [
        ("tailscale", &["login"]),
        ("sudo", &["-n", "tailscale", "login"]),
    ];
    let mut reasons = Vec::new();
    for (prog, args) in attempts {
        match attempt_login(prog, args) {
            LoginAttempt::Done => return,
            LoginAttempt::Refused(why) => reasons.push(format!("{prog}: {why}")),
        }
    }
    log(&format!(
        "tailscale login: could not start login ({}); allow it with \
         `sudo tailscale set --operator=$USER`",
        reasons.join("; ")
    ));
    notify(
        "Tailscale: login needed",
        "Run `tailscale login` (breadcrumbs lacks permission to start it).",
        Urgency::Normal,
    );
}

fn attempt_login(prog: &str, args: &[&str]) -> LoginAttempt {
    let mut child = match Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return LoginAttempt::Refused(format!("spawn failed: {e}")),
    };

    let (tx, rx) = mpsc::channel::<String>();
    let pipes: [Option<Box<dyn std::io::Read + Send>>; 2] = [
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
    ];
    for pipe in pipes.into_iter().flatten() {
        let tx = tx.clone();
        thread::spawn(move || {
            for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        });
    }
    drop(tx);

    // Wait for the auth URL, noticing an early exit (refused, or already
    // logged in) instead of sitting out the whole 30s.
    let start = Instant::now();
    let mut output = Vec::new();
    let url = loop {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(line) => {
                if let Some(url) = extract_url(&line) {
                    break Some(url);
                }
                output.push(line);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) | Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if let Ok(Some(status)) = child.try_wait() {
            if status.success() {
                return LoginAttempt::Done;
            }
            // Give the readers a moment to deliver the error text.
            thread::sleep(Duration::from_millis(100));
            output.extend(rx.try_iter());
            return LoginAttempt::Refused(output.join(" ").trim().to_string());
        }
        if start.elapsed() >= Duration::from_secs(30) {
            break None;
        }
    };

    match url {
        Some(url) => {
            log(&format!("tailscale login: opening {url}"));
            notify(
                "Tailscale: login required",
                "Opening browser to authenticate.",
                Urgency::Normal,
            );
            let _ = Command::new("xdg-open")
                .arg(&url)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        }
        None => log("tailscale login: no URL received within 30s"),
    }

    // Wait up to 5 min for the user to complete browser auth.
    let start = Instant::now();
    let timeout = Duration::from_secs(300);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() >= timeout {
                    log("tailscale login: timed out waiting for auth");
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                thread::sleep(Duration::from_millis(500));
            }
            Err(_) => break,
        }
    }
    LoginAttempt::Done
}

/// Strip whitespace and drop empty entries from the acceptable-node list.
fn effective_nodes(nodes: &[String]) -> Vec<String> {
    nodes
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Given a status JSON and the acceptable exit nodes, report `Ok` when the
/// active selection is one of them and online; otherwise the closest
/// actionable failure: not-selected (present + online, flow must select),
/// offline, or missing.
fn exit_node_health(nodes: &[String], v: &Value) -> TsHealth {
    let mut any_exists = false;
    let mut any_online = false;
    let mut any_selected = false;
    for node in nodes {
        let (exists, online, selected) = exit_node_state(v, node);
        if exists {
            any_exists = true;
            if online {
                any_online = true;
            }
            if selected {
                any_selected = true;
            }
        }
    }
    if any_selected && any_online {
        TsHealth::Ok
    } else if any_selected {
        // The active exit node is one of ours but offline.
        TsHealth::ExitNodeOffline
    } else if any_online {
        // Present + online but not selected: the flow will select it.
        TsHealth::Error("exit node not selected".into())
    } else if any_exists {
        TsHealth::ExitNodeOffline
    } else {
        TsHealth::ExitNodeMissing
    }
}

/// Run `tailscale set --exit-node=<node>`, falling back to `sudo -n` when
/// the direct call is refused. Without the fallback, a missing operator
/// (`sudo tailscale set --operator=$USER` never run) makes every recovery
/// loop report "exit node not selected" forever while holding the Wi-Fi on
/// the bootstrap network (CRUMB-22): the set is silently refused, the
/// re-read status still shows unselected, and the watch retries on cooldown.
fn set_exit_node(node: &str) {
    let arg = format!("--exit-node={node}");
    let out = run("tailscale", &["set", arg.as_str()], Duration::from_secs(10));
    let refused = !out.success
        || out.stdout.to_lowercase().contains("denied")
        || out.stderr.to_lowercase().contains("denied");
    if !refused {
        return;
    }
    log(&format!(
        "tailscale set refused ({}); retrying via sudo -n (avoid with `sudo tailscale set --operator=$USER`)",
        out.stderr.trim()
    ));
    let sudo_out = run(
        "sudo",
        &["-n", "tailscale", "set", arg.as_str()],
        Duration::from_secs(10),
    );
    if !sudo_out.success {
        log(&format!(
            "sudo -n tailscale set failed: {}",
            sudo_out.stderr.trim()
        ));
    }
}

/// Bring Tailscale to a state where one of `nodes` (priority order) is the
/// active, online exit node. Performs at most one bring-up/login, then at
/// most one `tailscale set` per online node until one takes (each set falls
/// back to `sudo -n` when refused for lack of operator rights); a healthy
/// selection is left untouched.
pub fn ensure_exit_node(nodes: &[String]) -> TsHealth {
    if !installed() {
        return TsHealth::NotInstalled;
    }
    let eff = effective_nodes(nodes);
    if eff.is_empty() {
        // Never run `tailscale set --exit-node=` with an empty node: that
        // would clear the user's current exit-node selection. This is a
        // config error, surfaced as its own health state.
        return TsHealth::NoExitNode;
    }

    let v = match status_json() {
        Some(v) => v,
        None => {
            // Daemon unreachable, usually *not running*: a stopped daemon
            // prints its error to stderr and leaves stdout empty, which is
            // exactly why this branch used to be dead code (the
            // BackendState "Stopped" case below only fires when the daemon
            // is up but the backend is stopped). Try to bring it up before
            // giving up; `tailscale up` is idempotent when already running
            // and fails fast (no sudo prompt: stdin is /dev/null) when
            // the caller lacks permission to manage the daemon.
            let _ = run("tailscale", &["up"], Duration::from_secs(20));
            match status_json() {
                Some(v2) => v2,
                None => return TsHealth::Error("could not read tailscale status".into()),
            }
        }
    };

    let v = match backend_state(&v).as_str() {
        "NeedsLogin" | "NoState" => {
            login_and_open();
            v
        }
        "Stopped" => {
            // Daemon not running: bring it up, then check if it needs auth.
            let _ = run("tailscale", &["up"], Duration::from_secs(20));
            match status_json() {
                Some(v2) => {
                    if matches!(backend_state(&v2).as_str(), "NeedsLogin" | "NoState") {
                        login_and_open();
                    }
                    v2
                }
                None => v,
            }
        }
        "" => return TsHealth::Error("empty backend state".into()),
        _ => v,
    };

    // Failover, in priority order. A node that is already selected and
    // online is kept as is (no `tailscale set` churn on every recovery
    // run); an offline or unknown node is skipped rather than selected,
    // since selecting a dead exit node blackholes all traffic until the
    // next node is tried. A higher-priority node that is online wins over
    // a lower-priority one that is currently selected (fail-back).
    let mut current = v;
    for node in &eff {
        let (exists, online, selected) = exit_node_state(&current, node);
        if selected && online {
            return TsHealth::Ok;
        }
        if !(exists && online) {
            continue;
        }
        set_exit_node(node);
        // The switch takes a moment to show up in `status`.
        for attempt in 0..3 {
            if attempt > 0 {
                thread::sleep(Duration::from_millis(300));
            }
            if let Some(v2) = status_json() {
                let ok = exit_node_health(std::slice::from_ref(node), &v2).is_ok();
                current = v2;
                if ok {
                    return TsHealth::Ok;
                }
            }
        }
    }

    let v = match status_json() {
        Some(v) => v,
        None => return TsHealth::Error("could not re-read tailscale status".into()),
    };
    match backend_state(&v).as_str() {
        "Running" => {}
        "NeedsLogin" | "NoState" => return TsHealth::NeedsLogin,
        "Stopped" => return TsHealth::Stopped,
        other => return TsHealth::Error(format!("backend state: {other}")),
    }
    exit_node_health(&eff, &v)
}

/// Lightweight health check without trying to (re)configure anything.
pub fn check(nodes: &[String]) -> TsHealth {
    if !installed() {
        return TsHealth::NotInstalled;
    }
    let eff = effective_nodes(nodes);
    if eff.is_empty() {
        // Read-only check, so never runs `tailscale set`: an empty node is
        // a config error, not something this probe can fix.
        return TsHealth::NoExitNode;
    }
    let v = match status_json() {
        Some(v) => v,
        None => return TsHealth::Error("status unavailable".into()),
    };
    match backend_state(&v).as_str() {
        "Running" => {}
        "NeedsLogin" | "NoState" => return TsHealth::NeedsLogin,
        "Stopped" => return TsHealth::Stopped,
        other => return TsHealth::Error(format!("backend state: {other}")),
    }
    exit_node_health(&eff, &v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn backend_state_extraction() {
        assert_eq!(
            backend_state(&json!({"BackendState": "Running"})),
            "Running"
        );
        assert_eq!(backend_state(&json!({})), "");
    }

    #[test]
    fn exit_node_healthy_and_selected() {
        let v = json!({
            "BackendState": "Running",
            "ExitNodeStatus": { "Online": true },
            "Peer": {
                "k1": { "HostName": "exitnode", "DNSName": "exitnode.ts.net.",
                        "Online": true, "ExitNode": true, "ExitNodeOption": true }
            }
        });
        assert_eq!(exit_node_state(&v, "exitnode"), (true, true, true));
    }

    #[test]
    fn exit_node_present_but_offline() {
        let v = json!({
            "BackendState": "Running",
            "Peer": {
                "k1": { "HostName": "exitnode", "DNSName": "exitnode.ts.net.",
                        "Online": false, "ExitNode": false, "ExitNodeOption": true }
            }
        });
        assert_eq!(exit_node_state(&v, "exitnode"), (true, false, false));
    }

    #[test]
    fn exit_node_missing_when_not_advertised() {
        let v = json!({
            "BackendState": "Running",
            "Peer": {
                "k1": { "HostName": "exitnode", "DNSName": "exitnode.ts.net.",
                        "Online": true, "ExitNode": false, "ExitNodeOption": false }
            }
        });
        assert_eq!(exit_node_state(&v, "exitnode"), (false, false, false));
    }

    #[test]
    fn exit_node_matches_via_dns_first_label() {
        let v = json!({
            "Peer": {
                "k1": { "HostName": "box-1", "DNSName": "exitnode.example.ts.net.",
                        "Online": true, "ExitNode": true, "ExitNodeOption": true }
            }
        });
        let (exists, online, selected) = exit_node_state(&v, "exitnode");
        assert!(exists && online && selected);
    }

    #[test]
    fn exit_node_present_online_but_not_selected() {
        let v = json!({
            "Peer": {
                "k1": { "HostName": "exitnode", "DNSName": "exitnode.ts.net.",
                        "Online": true, "ExitNode": false, "ExitNodeOption": true }
            }
        });
        assert_eq!(exit_node_state(&v, "exitnode"), (true, true, false));
    }

    #[test]
    fn exit_node_lookup_is_case_insensitive() {
        let v = json!({
            "Peer": {
                "k1": { "HostName": "ExitNode", "DNSName": "ExitNode.ts.net.",
                        "Online": true, "ExitNode": true, "ExitNodeOption": true }
            }
        });
        assert_eq!(exit_node_state(&v, "EXITNODE"), (true, true, true));
    }

    #[test]
    fn exit_node_matches_full_fqdn_case_insensitively_and_ignores_final_dot() {
        let v = json!({
            "Peer": {
                "k1": { "HostName": "box-1", "DNSName": "ExitNode.Example.TS.NET.",
                        "Online": true, "ExitNode": true, "ExitNodeOption": true }
            }
        });
        assert_eq!(
            exit_node_state(&v, "exitnode.example.ts.net"),
            (true, true, true)
        );
        assert_eq!(
            exit_node_state(&v, "EXITNODE.EXAMPLE.TS.NET."),
            (true, true, true)
        );
    }

    #[test]
    fn exit_node_fqdn_matching_does_not_match_a_different_domain() {
        let v = json!({
            "Peer": {
                "k1": { "HostName": "box-1", "DNSName": "exitnode.example.ts.net.",
                        "Online": true, "ExitNode": true, "ExitNodeOption": true }
            }
        });
        assert_eq!(
            exit_node_state(&v, "exitnode.example.org"),
            (false, false, false)
        );
    }

    #[test]
    fn exit_node_state_with_no_peers_is_all_false() {
        let v = json!({ "BackendState": "Running" });
        assert_eq!(exit_node_state(&v, "exitnode"), (false, false, false));
    }

    #[test]
    fn exit_node_state_empty_node_name_matches_nothing() {
        let v = json!({
            "Peer": {
                "k1": { "HostName": "exitnode", "DNSName": "exitnode.ts.net.",
                        "Online": true, "ExitNode": true, "ExitNodeOption": true }
            }
        });
        assert_eq!(exit_node_state(&v, ""), (false, false, false));
    }

    #[test]
    fn exit_node_status_online_only_applies_when_selected() {
        // ExitNodeStatus.Online reflects the currently *active* exit node:
        // it must not leak into the reported state of a peer that merely
        // matches by name but isn't the one actually selected.
        let v = json!({
            "ExitNodeStatus": { "Online": true },
            "Peer": {
                "k1": { "HostName": "other", "DNSName": "other.ts.net.",
                        "Online": false, "ExitNode": false, "ExitNodeOption": true }
            }
        });
        assert_eq!(exit_node_state(&v, "other"), (true, false, false));
    }

    #[test]
    fn ts_health_is_ok_only_for_ok_variant() {
        assert!(TsHealth::Ok.is_ok());
        assert!(!TsHealth::NotInstalled.is_ok());
        assert!(!TsHealth::NeedsLogin.is_ok());
        assert!(!TsHealth::Stopped.is_ok());
        assert!(!TsHealth::ExitNodeMissing.is_ok());
        assert!(!TsHealth::ExitNodeOffline.is_ok());
        assert!(!TsHealth::NoExitNode.is_ok());
        assert!(!TsHealth::Error("x".into()).is_ok());
    }

    #[test]
    fn ts_health_describe_is_human_readable() {
        assert_eq!(TsHealth::Ok.describe(), "ok");
        assert_eq!(
            TsHealth::NeedsLogin.describe(),
            "not logged in (run: tailscale up)"
        );
        assert_eq!(TsHealth::NoExitNode.describe(), "no exit node configured");
        assert_eq!(TsHealth::Error("boom".into()).describe(), "error: boom");
    }

    #[test]
    fn extract_url_finds_https_token_among_others() {
        assert_eq!(
            extract_url("To authenticate, visit: https://login.tailscale.com/abc"),
            Some("https://login.tailscale.com/abc".to_string())
        );
        assert_eq!(extract_url("no url on this line"), None);
    }

    #[test]
    fn effective_nodes_trims_and_drops_empties() {
        assert_eq!(
            effective_nodes(&["  a  ".into(), "".into(), "b".into()]),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn exit_node_health_ok_when_any_node_selected_and_online() {
        let v = json!({
            "BackendState": "Running",
            "Peer": {
                "k1": { "HostName": "nodeB", "DNSName": "nodeB.ts.net.",
                        "Online": true, "ExitNode": true, "ExitNodeOption": true }
            }
        });
        assert_eq!(
            exit_node_health(&["nodeA".into(), "nodeB".into()], &v),
            TsHealth::Ok
        );
    }

    #[test]
    fn exit_node_health_reports_not_selected_when_a_node_is_online_but_unselected() {
        let v = json!({
            "Peer": {
                "k1": { "HostName": "nodeA", "DNSName": "nodeA.ts.net.",
                        "Online": true, "ExitNode": false, "ExitNodeOption": true }
            }
        });
        assert_eq!(
            exit_node_health(&["nodeA".into()], &v),
            TsHealth::Error("exit node not selected".into())
        );
    }

    #[test]
    fn exit_node_health_reports_offline_when_all_exist_but_none_online() {
        let v = json!({
            "Peer": {
                "k1": { "HostName": "nodeA", "DNSName": "nodeA.ts.net.",
                        "Online": false, "ExitNode": false, "ExitNodeOption": true }
            }
        });
        assert_eq!(
            exit_node_health(&["nodeA".into()], &v),
            TsHealth::ExitNodeOffline
        );
    }

    #[test]
    fn exit_node_health_reports_missing_when_no_node_present() {
        let v = json!({ "BackendState": "Running" });
        assert_eq!(
            exit_node_health(&["nodeA".into()], &v),
            TsHealth::ExitNodeMissing
        );
    }

    #[test]
    fn state_str_is_the_variant_name() {
        assert_eq!(TsHealth::Ok.state_str(), "Ok");
        assert_eq!(TsHealth::NotInstalled.state_str(), "NotInstalled");
        assert_eq!(TsHealth::NeedsLogin.state_str(), "NeedsLogin");
        assert_eq!(TsHealth::Stopped.state_str(), "Stopped");
        assert_eq!(TsHealth::ExitNodeMissing.state_str(), "ExitNodeMissing");
        assert_eq!(TsHealth::ExitNodeOffline.state_str(), "ExitNodeOffline");
        assert_eq!(TsHealth::NoExitNode.state_str(), "NoExitNode");
        assert_eq!(TsHealth::Error("x".into()).state_str(), "Error");
    }
}
