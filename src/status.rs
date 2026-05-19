use std::time::Duration;

use crate::config::Config;
use crate::nm;
use crate::tailscale::{self, TsHealth};
use crate::util::{command_exists, run};

pub fn internet_ok(cfg: &Config) -> bool {
    if command_exists("curl") {
        let o = run(
            "curl",
            &[
                "-s",
                "-o",
                "/dev/null",
                "-w",
                "%{http_code}",
                "--max-time",
                "4",
                &cfg.settings.connectivity_url,
            ],
            Duration::from_secs(6),
        );
        let code = o.stdout.trim();
        if code == "204" || code == "200" || code == "301" || code == "302" {
            return true;
        }
    }
    // Fallback: ICMP to the configured host.
    run(
        "ping",
        &["-c", "1", "-W", "2", &cfg.settings.ping_host],
        Duration::from_secs(4),
    )
    .success
}

fn ipv4(iface: &str) -> Option<String> {
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
        Some(s.lines().next().unwrap_or(s).trim().to_string())
    }
}

pub struct Status {
    pub iface: Option<String>,
    pub ssid: Option<String>,
    pub ip: Option<String>,
    pub internet: bool,
    pub tailscale_required: bool,
    pub tailscale: Option<TsHealth>,
    pub exit_node: String,
}

pub fn gather(cfg: &Config, profile_name: &str) -> Status {
    let iface = nm::wifi_interface();
    let ssid = iface.as_deref().and_then(nm::active_ssid);
    let ip = iface.as_deref().and_then(ipv4);
    let internet = internet_ok(cfg);

    let prof = cfg.profile(profile_name);
    let ts_required = prof.map(|p| p.tailscale).unwrap_or(false);
    let exit_node = prof
        .and_then(|p| p.exit_node.clone())
        .unwrap_or_else(|| cfg.settings.exit_node.clone());

    let tailscale = if tailscale::installed() {
        Some(tailscale::check(&exit_node))
    } else {
        None
    };

    Status {
        iface,
        ssid,
        ip,
        internet,
        tailscale_required: ts_required,
        tailscale,
        exit_node,
    }
}
