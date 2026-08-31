use std::time::Duration;

use crate::config::Config;
use crate::nm;
use crate::tailscale::{self, TsHealth};
use crate::util::{command_exists, run};

/// Connectivity verdict. `Portal` is the interesting case: an HTTP response
/// arrived (200/301/302) but it wasn't the 204 the generate_204 endpoint
/// returns for genuine internet — the classic captive/guest-portal
/// signature, and the reason `classify` can tell "no internet at all" from
/// "internet but intercepted".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connectivity {
    Online,
    Portal,
    NoNet,
}

pub fn connectivity(cfg: &Config) -> Connectivity {
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
        // Only a 204 counts as real internet. Captive/guest portals answer
        // 200 (a login page) or 302 (a redirect to it). The default endpoint
        // is generate_204, which returns 204 precisely when traffic isn't
        // being intercepted.
        let code = o.stdout.trim();
        if code == "204" {
            return Connectivity::Online;
        }
        if code == "200" || code == "301" || code == "302" {
            return Connectivity::Portal;
        }
    }
    // Fallback: ICMP to the configured host. A working ping overrides a
    // non-204 curl answer that wasn't portal-shaped (e.g. a 403 from an
    // overzealous firewall); a portal usually blocks ICMP too, so this
    // stays Portal for the genuine case.
    let ping = run(
        "ping",
        &["-c", "1", "-W", "2", &cfg.settings.ping_host],
        Duration::from_secs(4),
    );
    if ping.success {
        Connectivity::Online
    } else {
        Connectivity::NoNet
    }
}

pub fn internet_ok(cfg: &Config) -> bool {
    matches!(connectivity(cfg), Connectivity::Online)
}

pub struct Status {
    pub iface: Option<String>,
    pub ssid: Option<String>,
    pub ip: Option<String>,
    pub internet: bool,
    /// True when traffic is being intercepted (captive/guest portal).
    pub portal: bool,
    pub tailscale_required: bool,
    pub tailscale: Option<TsHealth>,
    pub exit_node: String,
}

pub fn gather(cfg: &Config, profile_name: &str) -> Status {
    let iface = nm::wifi_interface_preferred(cfg.settings.interface.as_deref());
    let ssid = iface.as_deref().and_then(nm::active_ssid);
    let ip = iface.as_deref().and_then(nm::ipv4_address);
    // Skip the (potentially 4s-blocking) connectivity probe when there's no
    // Wi-Fi interface at all: the watch loop classifies NoAdapter and would
    // otherwise burn a network round-trip (curl/ping) every tick for nothing.
    let (internet, portal) = if iface.is_some() {
        match connectivity(cfg) {
            Connectivity::Online => (true, false),
            Connectivity::Portal => (false, true),
            Connectivity::NoNet => (false, false),
        }
    } else {
        (false, false)
    };

    let prof = cfg.profile(profile_name);
    let ts_required = prof.map(|p| p.tailscale).unwrap_or(false);
    let exit_nodes = cfg.exit_nodes_for(profile_name);
    let exit_node = exit_nodes.first().cloned().unwrap_or_default();

    // Checked whenever tailscale is installed so `status`/`doctor` can show
    // it even for non-required profiles; classify only consults it when the
    // profile requires Tailscale.
    let tailscale = if tailscale::installed() {
        Some(tailscale::check(&exit_nodes))
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
