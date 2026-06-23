use std::time::Duration;

use crate::backend::Backend;
use crate::config::Config;
use crate::tailscale::TsHealth;
use crate::util::{command_exists, run};

/// Result of a connectivity probe. `Portal` distinguishes a captive portal
/// (associated, but traffic is being intercepted) from real internet or a hard
/// outage — the optional string is the portal's sign-in URL when known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Connectivity {
    Online,
    Portal(Option<String>),
    Offline,
}

impl Connectivity {
    pub fn online(&self) -> bool {
        matches!(self, Connectivity::Online)
    }
}

/// Probe connectivity. Only an empty HTTP 204 from the generate_204-style
/// endpoint counts as online; a 200/redirect means a captive portal is
/// intercepting traffic. If the HTTP probe is inconclusive (timeout/5xx) we fall
/// back to ICMP, which reaching the host treats as online.
pub fn connectivity(cfg: &Config) -> Connectivity {
    if command_exists("curl") {
        let o = run(
            "curl",
            &[
                "-s",
                "-o",
                "/dev/null",
                "-w",
                "%{http_code} %{redirect_url}",
                "--max-time",
                "4",
                &cfg.settings.connectivity_url,
            ],
            Duration::from_secs(6),
        );
        let mut parts = o.stdout.split_whitespace();
        let code = parts.next().unwrap_or("");
        let redirect = parts.next().unwrap_or("").trim();
        match code {
            "204" => return Connectivity::Online,
            "200" | "301" | "302" | "303" | "307" | "308" => {
                let url = if redirect.is_empty() {
                    None
                } else {
                    Some(redirect.to_string())
                };
                return Connectivity::Portal(url);
            }
            // 000/timeout/5xx → inconclusive, try ICMP below.
            _ => {}
        }
    }
    let ping = run(
        "ping",
        &["-c", "1", "-W", "2", &cfg.settings.ping_host],
        Duration::from_secs(4),
    )
    .success;
    if ping {
        Connectivity::Online
    } else {
        Connectivity::Offline
    }
}

/// Best-effort IPv4 address of `iface` via nmcli, with the CIDR prefix stripped.
pub fn ipv4(iface: &str) -> Option<String> {
    let o = run(
        "nmcli",
        &["-g", "IP4.ADDRESS", "device", "show", iface],
        Duration::from_secs(6),
    );
    if !o.success {
        return None;
    }
    let s = o.stdout.trim();
    if s.is_empty() {
        None
    } else {
        // nmcli reports "192.168.1.5/24"; drop the prefix length for display.
        let first = s.lines().next().unwrap_or(s).trim();
        Some(first.split('/').next().unwrap_or(first).to_string())
    }
}

pub struct Status {
    pub iface: Option<String>,
    pub ssid: Option<String>,
    pub ip: Option<String>,
    pub internet: bool,
    /// Set when a captive portal was detected; inner string is its URL if known.
    pub portal: Option<String>,
    pub tailscale_required: bool,
    pub tailscale: Option<TsHealth>,
    pub exit_node: String,
}

pub fn gather(be: &dyn Backend, cfg: &Config, profile_name: &str) -> Status {
    let iface = be.wifi_interface();
    let ssid = iface.as_deref().and_then(|i| be.active_ssid(i));
    let ip = iface.as_deref().and_then(|i| be.ipv4(i));

    let conn = be.connectivity(cfg);
    let internet = conn.online();
    let portal = match conn {
        Connectivity::Portal(url) => Some(url.unwrap_or_default()),
        _ => None,
    };

    let prof = cfg.profile(profile_name);
    let ts_required = prof.map(|p| p.tailscale).unwrap_or(false);
    let exit_node = prof
        .and_then(|p| p.exit_node.clone())
        .unwrap_or_else(|| cfg.settings.exit_node.clone());

    let tailscale = if be.tailscale_installed() {
        Some(be.tailscale_check(&exit_node))
    } else {
        None
    };

    Status {
        iface,
        ssid,
        ip,
        internet,
        portal,
        tailscale_required: ts_required,
        tailscale,
        exit_node,
    }
}
