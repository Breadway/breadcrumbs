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
    Error(String),
}

impl TsHealth {
    pub fn is_ok(&self) -> bool {
        matches!(self, TsHealth::Ok)
    }

    pub fn describe(&self) -> String {
        match self {
            TsHealth::Ok => "ok".into(),
            TsHealth::NotInstalled => "tailscale not installed".into(),
            TsHealth::NeedsLogin => "not logged in (run: tailscale up)".into(),
            TsHealth::Stopped => "backend stopped".into(),
            TsHealth::ExitNodeMissing => "exit node not found in tailnet".into(),
            TsHealth::ExitNodeOffline => "exit node is offline".into(),
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
                .to_lowercase();
            let matches = host == want || dns.split('.').next() == Some(want.as_str());
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
/// actual `sudo tailscale login` + browser flow runs on its own thread; the
/// caller just keeps reporting `NeedsLogin` (→ stay on the bootstrap network)
/// until a later poll observes the backend come up. The guard collapses
/// repeated triggers into a single in-flight attempt.
fn login_and_open() {
    if LOGIN_INFLIGHT.swap(true, Ordering::SeqCst) {
        return;
    }
    thread::spawn(|| {
        run_login();
        LOGIN_INFLIGHT.store(false, Ordering::SeqCst);
    });
}

/// Run `sudo tailscale login`, open the auth URL in the browser, and block
/// until login completes (up to 5 minutes). Always called on a worker thread.
fn run_login() {
    let mut child = match Command::new("sudo")
        .args(["tailscale", "login"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            log(&format!("tailscale login: spawn failed: {e}"));
            return;
        }
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let (tx, rx) = mpsc::channel::<String>();
    let tx2 = tx.clone();

    thread::spawn(move || {
        if let Some(r) = stdout {
            for line in BufReader::new(r).lines().map_while(Result::ok) {
                if let Some(url) = extract_url(&line) {
                    let _ = tx.send(url);
                }
            }
        }
    });
    thread::spawn(move || {
        if let Some(r) = stderr {
            for line in BufReader::new(r).lines().map_while(Result::ok) {
                if let Some(url) = extract_url(&line) {
                    let _ = tx2.send(url);
                }
            }
        }
    });

    if let Ok(url) = rx.recv_timeout(Duration::from_secs(30)) {
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
    } else {
        log("tailscale login: no URL received within 30s");
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
}

/// Bring Tailscale to a state where `node` is the active, online exit node.
/// Performs at most one bring-up/login and one `tailscale set` attempt.
pub fn ensure_exit_node(node: &str) -> TsHealth {
    if !installed() {
        return TsHealth::NotInstalled;
    }

    let v = match status_json() {
        Some(v) => v,
        None => return TsHealth::Error("could not read tailscale status".into()),
    };

    match backend_state(&v).as_str() {
        "NeedsLogin" | "NoState" => {
            login_and_open();
        }
        "Stopped" => {
            // Daemon not running — bring it up, then check if it needs auth.
            let _ = run("tailscale", &["up"], Duration::from_secs(20));
            if let Some(v2) = status_json() {
                if matches!(backend_state(&v2).as_str(), "NeedsLogin" | "NoState") {
                    login_and_open();
                }
            }
        }
        "Running" => {}
        "" => return TsHealth::Error("empty backend state".into()),
        _ => {}
    }

    // Select the exit node (idempotent).
    let _ = run(
        "tailscale",
        &["set", &format!("--exit-node={node}")],
        Duration::from_secs(10),
    );

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

    let (exists, online, selected) = exit_node_state(&v, node);
    if !exists {
        TsHealth::ExitNodeMissing
    } else if !online {
        TsHealth::ExitNodeOffline
    } else if !selected {
        // Online and present but our set didn't take — treat as missing/selectable error.
        TsHealth::Error("exit node not selected".into())
    } else {
        TsHealth::Ok
    }
}

/// Lightweight health check without trying to (re)configure anything.
pub fn check(node: &str) -> TsHealth {
    if !installed() {
        return TsHealth::NotInstalled;
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
    let (exists, online, selected) = exit_node_state(&v, node);
    if !exists {
        TsHealth::ExitNodeMissing
    } else if !online {
        TsHealth::ExitNodeOffline
    } else if !selected {
        TsHealth::Error("exit node not selected".into())
    } else {
        TsHealth::Ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ts_health_is_ok_only_for_ok_variant() {
        assert!(TsHealth::Ok.is_ok());
        assert!(!TsHealth::NotInstalled.is_ok());
        assert!(!TsHealth::NeedsLogin.is_ok());
        assert!(!TsHealth::Stopped.is_ok());
        assert!(!TsHealth::ExitNodeMissing.is_ok());
        assert!(!TsHealth::ExitNodeOffline.is_ok());
        assert!(!TsHealth::Error("x".into()).is_ok());
    }

    #[test]
    fn ts_health_describe_covers_all_variants() {
        assert_eq!(TsHealth::Ok.describe(), "ok");
        assert!(TsHealth::NotInstalled.describe().contains("not installed"));
        assert!(TsHealth::NeedsLogin.describe().contains("not logged in"));
        assert!(TsHealth::Stopped.describe().contains("stopped"));
        assert!(TsHealth::ExitNodeMissing.describe().contains("not found"));
        assert!(TsHealth::ExitNodeOffline.describe().contains("offline"));
        let msg = TsHealth::Error("boom".into()).describe();
        assert!(msg.contains("boom"));
    }

    #[test]
    fn extract_url_finds_https_url() {
        assert_eq!(
            extract_url("To authenticate, visit https://login.tailscale.com/a/xxx"),
            Some("https://login.tailscale.com/a/xxx".into())
        );
    }

    #[test]
    fn extract_url_returns_none_when_no_url() {
        assert_eq!(extract_url("Waiting for login..."), None);
        assert_eq!(extract_url(""), None);
    }

    #[test]
    fn extract_url_picks_first_https_token() {
        let line = "Try https://first.example.com https://second.example.com";
        assert_eq!(extract_url(line), Some("https://first.example.com".into()));
    }

    #[test]
    fn extract_url_does_not_match_plain_http() {
        assert_eq!(extract_url("see http://example.com for info"), None);
    }

    #[test]
    fn backend_state_extraction() {
        assert_eq!(
            backend_state(&json!({"BackendState": "Running"})),
            "Running"
        );
        assert_eq!(backend_state(&json!({})), "");
    }

    #[test]
    fn backend_state_all_known_values() {
        for state in ["Running", "NeedsLogin", "NoState", "Stopped"] {
            assert_eq!(backend_state(&json!({"BackendState": state})), state);
        }
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
    fn exit_node_state_empty_peer_map() {
        let v = json!({ "BackendState": "Running", "Peer": {} });
        assert_eq!(exit_node_state(&v, "anynode"), (false, false, false));
    }

    #[test]
    fn exit_node_state_no_peer_field() {
        let v = json!({ "BackendState": "Running" });
        assert_eq!(exit_node_state(&v, "anynode"), (false, false, false));
    }

    #[test]
    fn exit_node_state_case_insensitive_hostname() {
        let v = json!({
            "Peer": {
                "k1": { "HostName": "MYNODE", "DNSName": "mynode.ts.net.",
                        "Online": true, "ExitNode": true, "ExitNodeOption": true }
            }
        });
        let (exists, online, selected) = exit_node_state(&v, "mynode");
        assert!(exists && online && selected);
    }

    #[test]
    fn exit_node_status_overrides_peer_online_when_selected() {
        // ExitNodeStatus.Online=false should override the peer's Online=true
        // when the peer is the currently-selected exit node.
        let v = json!({
            "ExitNodeStatus": { "Online": false },
            "Peer": {
                "k1": { "HostName": "exitnode", "DNSName": "exitnode.ts.net.",
                        "Online": true, "ExitNode": true, "ExitNodeOption": true }
            }
        });
        let (exists, online, selected) = exit_node_state(&v, "exitnode");
        assert!(exists);
        assert!(
            !online,
            "ExitNodeStatus.Online=false should override peer Online"
        );
        assert!(selected);
    }

    #[test]
    fn exit_node_state_wrong_node_name_not_matched() {
        let v = json!({
            "Peer": {
                "k1": { "HostName": "othernode", "DNSName": "othernode.ts.net.",
                        "Online": true, "ExitNode": true, "ExitNodeOption": true }
            }
        });
        assert_eq!(exit_node_state(&v, "exitnode"), (false, false, false));
    }
}
