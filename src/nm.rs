use std::collections::HashSet;
use std::time::Duration;

use crate::config::NetworkDef;
use crate::util::{run, run_ok, run_with_stdin};

/// nmcli `-t` escapes `:` and `\` in field values; undo that.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(&n) = chars.peek() {
                out.push(n);
                chars.next();
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// Split one nmcli `-t` line into fields. Fields are ':'-separated but values
/// escape ':' as '\:' and '\' as '\\'.
fn parse_scan_line(line: &str) -> Vec<String> {
    let mut fields: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(&n) = chars.peek() {
                cur.push(n);
                chars.next();
                continue;
            }
        }
        if c == ':' {
            fields.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    fields.push(cur);
    fields
}

pub fn wifi_interface() -> Option<String> {
    let o = run(
        "nmcli",
        &["-t", "-f", "DEVICE,TYPE", "device", "status"],
        Duration::from_secs(8),
    );
    if !o.success {
        return None;
    }
    for line in o.stdout.lines() {
        let parts: Vec<&str> = line.splitn(2, ':').collect();
        if parts.len() == 2 && parts[1] == "wifi" {
            return Some(unescape(parts[0]));
        }
    }
    None
}

pub fn radio_on() {
    let _ = run("nmcli", &["radio", "wifi", "on"], Duration::from_secs(6));
}

pub fn rescan(iface: &str, ssids: &[String]) {
    let mut args: Vec<String> = vec![
        "device".into(),
        "wifi".into(),
        "rescan".into(),
        "ifname".into(),
        iface.into(),
    ];
    for s in ssids {
        args.push("ssid".into());
        args.push(s.clone());
    }
    let argv: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let _ = run("nmcli", &argv, Duration::from_secs(20));
}

pub fn visible_ssids(iface: &str) -> HashSet<String> {
    let o = run(
        "nmcli",
        &[
            "-t", "-f", "SSID", "device", "wifi", "list", "ifname", iface,
        ],
        Duration::from_secs(12),
    );
    let mut set = HashSet::new();
    if !o.success {
        return set;
    }
    for line in o.stdout.lines() {
        let ssid = unescape(line.trim());
        if !ssid.is_empty() {
            set.insert(ssid);
        }
    }
    set
}

#[derive(Debug, Clone)]
pub struct ScanEntry {
    pub ssid: String,
    pub signal: String,
    pub security: String,
}

pub fn scan_list(iface: &str) -> Vec<ScanEntry> {
    let o = run(
        "nmcli",
        &[
            "-t",
            "-f",
            "SSID,SIGNAL,SECURITY",
            "device",
            "wifi",
            "list",
            "ifname",
            iface,
        ],
        Duration::from_secs(12),
    );
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    if !o.success {
        return out;
    }
    for line in o.stdout.lines() {
        let fields = parse_scan_line(line);
        if fields.is_empty() {
            continue;
        }
        let ssid = fields[0].trim().to_string();
        if ssid.is_empty() || !seen.insert(ssid.clone()) {
            continue;
        }
        out.push(ScanEntry {
            ssid,
            signal: fields.get(1).cloned().unwrap_or_default(),
            security: fields.get(2).cloned().unwrap_or_default(),
        });
    }
    out
}

pub fn active_ssid(iface: &str) -> Option<String> {
    let o = run(
        "nmcli",
        &[
            "-t",
            "-f",
            "ACTIVE,SSID",
            "device",
            "wifi",
            "list",
            "ifname",
            iface,
        ],
        Duration::from_secs(8),
    );
    if !o.success {
        return None;
    }
    for line in o.stdout.lines() {
        let parts: Vec<&str> = line.splitn(2, ':').collect();
        if parts.len() == 2 && parts[0] == "yes" {
            let s = unescape(parts[1].trim());
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    None
}

pub fn device_connected(iface: &str) -> bool {
    let o = run(
        "nmcli",
        &["-t", "-f", "DEVICE,STATE", "device", "status"],
        Duration::from_secs(6),
    );
    if !o.success {
        return false;
    }
    for line in o.stdout.lines() {
        let parts: Vec<&str> = line.splitn(2, ':').collect();
        if parts.len() == 2 && unescape(parts[0]) == iface {
            return parts[1].starts_with("connected");
        }
    }
    false
}

fn active_uuid(iface: &str) -> Option<String> {
    let o = run(
        "nmcli",
        &["-g", "GENERAL.CON-UUID", "device", "show", iface],
        Duration::from_secs(6),
    );
    if !o.success {
        return None;
    }
    let u = o.stdout.trim().to_string();
    if u.is_empty() {
        None
    } else {
        Some(u)
    }
}

fn enforce_dns(uuid: &str, iface: &str, dns: &str) {
    if dns.trim().is_empty() {
        return;
    }
    let ok = run_ok(
        "nmcli",
        &[
            "connection",
            "modify",
            uuid,
            "ipv4.ignore-auto-dns",
            "yes",
            "ipv4.dns",
            dns,
        ],
        Duration::from_secs(8),
    );
    if ok {
        let _ = run(
            "nmcli",
            &["device", "reapply", iface],
            Duration::from_secs(8),
        );
    }
}

/// Connect to a network and pin DNS. Returns true only if associated.
pub fn connect(iface: &str, net: &NetworkDef, wait: u32, dns: &str) -> bool {
    let wait_s = wait.to_string();
    let hidden = if net.hidden { "yes" } else { "no" };
    // `--ask` makes nmcli read the PSK from stdin instead of taking it on the
    // command line, so the password never appears in `ps`/`/proc`.
    let args = [
        "--wait",
        &wait_s,
        "--ask",
        "device",
        "wifi",
        "connect",
        net.ssid.as_str(),
        "hidden",
        hidden,
        "ifname",
        iface,
    ];
    let secret = format!("{}\n", net.password);
    let o = run_with_stdin(
        "nmcli",
        &args,
        Some(&secret),
        Duration::from_secs(wait as u64 + 15),
    );
    if !o.success {
        return false;
    }
    if let Some(uuid) = active_uuid(iface) {
        enforce_dns(&uuid, iface, dns);
    }
    true
}

/// Delete every saved connection profile whose name or 802-11-wireless SSID
/// matches `ssid` (used by `breadcrumbs forget` to purge stale entries).
pub fn delete_connections_for_ssid(ssid: &str) -> bool {
    let list = run(
        "nmcli",
        &["-t", "-f", "NAME,TYPE", "connection", "show"],
        Duration::from_secs(8),
    );
    if !list.success {
        return false;
    }
    let mut removed = false;
    for line in list.stdout.lines() {
        let parts: Vec<&str> = line.splitn(2, ':').collect();
        if parts.len() < 2 {
            continue;
        }
        let name = unescape(parts[0]);
        let typ = parts[1];
        if !typ.contains("wireless") {
            continue;
        }
        let conn_ssid = run(
            "nmcli",
            &["-g", "802-11-wireless.ssid", "connection", "show", &name],
            Duration::from_secs(6),
        );
        let conn_ssid = conn_ssid.stdout.trim();
        if (name == ssid || conn_ssid == ssid)
            && run_ok(
                "nmcli",
                &["connection", "delete", "id", &name],
                Duration::from_secs(8),
            )
        {
            removed = true;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescape_handles_nmcli_escaping() {
        assert_eq!(unescape("plain"), "plain");
        assert_eq!(unescape(r"a\:b"), "a:b");
        assert_eq!(unescape(r"back\\slash"), r"back\slash");
        assert_eq!(unescape("trailing\\"), "trailing\\");
    }

    #[test]
    fn parse_scan_line_splits_and_unescapes() {
        // SSID:SIGNAL:SECURITY with an escaped ':' inside the SSID.
        let f = parse_scan_line(r"My\:Net:72:WPA2");
        assert_eq!(f, vec!["My:Net", "72", "WPA2"]);

        // SSID with a space (common in real network names)
        let f = parse_scan_line("My Network:88:WPA2");
        assert_eq!(f, vec!["My Network", "88", "WPA2"]);

        // Empty SSID (hidden) keeps the empty leading field.
        let f = parse_scan_line(":40:WPA3");
        assert_eq!(f, vec!["", "40", "WPA3"]);
    }
}
