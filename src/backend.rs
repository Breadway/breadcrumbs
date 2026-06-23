//! The seam between breadcrumbs' decision logic and the outside world.
//!
//! Every interaction with NetworkManager, Tailscale, connectivity probes,
//! notifications and logging goes through the [`Backend`] trait. Production code
//! uses [`System`], which delegates to the `nm`/`tailscale`/`status`/`notify`
//! modules that shell out. Tests inject a fake so the connect state machine
//! (`flow`) and watch classifier can be exercised without touching the host.

use std::collections::HashSet;

use crate::config::{Config, NetworkDef};
use crate::nm;
use crate::notify::{self, Urgency};
use crate::status::{self, Connectivity};
use crate::tailscale::{self, TsHealth};

pub trait Backend {
    fn wifi_interface(&self) -> Option<String>;
    fn radio_on(&self);
    fn rescan(&self, iface: &str, ssids: &[String]);
    fn visible_ssids(&self, iface: &str) -> HashSet<String>;
    fn active_ssid(&self, iface: &str) -> Option<String>;
    fn ipv4(&self, iface: &str) -> Option<String>;
    fn device_connected(&self, iface: &str) -> bool;
    fn connect(&self, iface: &str, net: &NetworkDef, wait: u32, dns: &str) -> Result<(), String>;
    fn tailscale_installed(&self) -> bool;
    fn ensure_exit_node(&self, node: &str) -> TsHealth;
    fn tailscale_check(&self, node: &str) -> TsHealth;
    fn connectivity(&self, cfg: &Config) -> Connectivity;
    fn notify(&self, summary: &str, body: &str, urgency: Urgency);
    fn log(&self, line: &str);
}

/// The real backend: every method delegates to the system-facing modules.
pub struct System;

impl Backend for System {
    fn wifi_interface(&self) -> Option<String> {
        nm::wifi_interface()
    }
    fn radio_on(&self) {
        nm::radio_on()
    }
    fn rescan(&self, iface: &str, ssids: &[String]) {
        nm::rescan(iface, ssids)
    }
    fn visible_ssids(&self, iface: &str) -> HashSet<String> {
        nm::visible_ssids(iface)
    }
    fn active_ssid(&self, iface: &str) -> Option<String> {
        nm::active_ssid(iface)
    }
    fn ipv4(&self, iface: &str) -> Option<String> {
        status::ipv4(iface)
    }
    fn device_connected(&self, iface: &str) -> bool {
        nm::device_connected(iface)
    }
    fn connect(&self, iface: &str, net: &NetworkDef, wait: u32, dns: &str) -> Result<(), String> {
        nm::connect_verbose(iface, net, wait, dns)
    }
    fn tailscale_installed(&self) -> bool {
        tailscale::installed()
    }
    fn ensure_exit_node(&self, node: &str) -> TsHealth {
        tailscale::ensure_exit_node(node)
    }
    fn tailscale_check(&self, node: &str) -> TsHealth {
        tailscale::check(node)
    }
    fn connectivity(&self, cfg: &Config) -> Connectivity {
        status::connectivity(cfg)
    }
    fn notify(&self, summary: &str, body: &str, urgency: Urgency) {
        notify::notify(summary, body, urgency)
    }
    fn log(&self, line: &str) {
        notify::log(line)
    }
}
