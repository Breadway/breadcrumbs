use std::collections::HashSet;
use std::time::Duration;

use crate::config::NetworkDef;
use crate::util::{run, run_ok};

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
/// escape ':' as '\:' and '\' as '\\' — a plain `splitn(2, ':')` mis-splits
/// any field (device name, connection name, SSID, …) that legitimately
/// contains a colon, so every terse-output parse in this module goes through
/// here rather than splitting on raw bytes. Fields are returned unescaped.
fn split_fields(line: &str) -> Vec<String> {
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
        let fields = split_fields(line);
        if fields.len() >= 2 && fields[1] == "wifi" {
            return Some(fields[0].clone());
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
        let fields = split_fields(line);
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
        let fields = split_fields(line);
        if fields.len() >= 2 && fields[0] == "yes" {
            let s = fields[1].trim().to_string();
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
        let fields = split_fields(line);
        if fields.len() >= 2 && fields[0] == iface {
            return fields[1].starts_with("connected");
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

/// Return the name of the first saved NM connection profile whose name is
/// either exactly `ssid` or `ssid N` (NM's numbered-duplicate convention).
/// Returns `None` if no such profile exists.
fn first_profile_for_ssid(ssid: &str) -> Option<String> {
    let o = run(
        "nmcli",
        &["-t", "-f", "NAME,TYPE", "connection", "show"],
        Duration::from_secs(8),
    );
    if !o.success {
        return None;
    }
    let mut fallback: Option<String> = None;
    for line in o.stdout.lines() {
        let fields = split_fields(line);
        if fields.len() < 2 || !fields[1].contains("wireless") {
            continue;
        }
        let name = fields[0].clone();
        if name == ssid {
            return Some(name);
        }
        if fallback.is_none() {
            if let Some(suffix) = name.strip_prefix(ssid) {
                let s = suffix.trim();
                if !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()) {
                    fallback = Some(name);
                }
            }
        }
    }
    fallback
}

/// Connect to a network and pin DNS. Returns true only if associated.
pub fn connect(iface: &str, net: &NetworkDef, wait: u32, dns: &str) -> bool {
    connect_verbose(iface, net, wait, dns).is_ok()
}

/// Connect to a network and pin DNS. Returns the nmcli error on failure.
///
/// Reuses an existing saved profile for the SSID when one exists (updating its
/// PSK) so that repeated connections do not accumulate numbered duplicates in
/// NetworkManager ("NCC", "NCC 1", "NCC 2", …). Falls back to
/// `nmcli device wifi connect` — which creates a new profile — only when no
/// saved profile is found.
///
/// `net.password` is only sent when `Some`: on the reuse path, `None` means
/// "leave the saved PSK alone" (either NetworkManager already durably owns
/// it, or the network is open); on the create path it means "no password
/// argument at all", which is also how a genuinely open (no-security) SSID
/// is connected. See the field doc on [`NetworkDef::password`] for how a
/// local secret transitions to `None` after its first successful use.
///
/// KNOWN LIMITATION (credential exposure): when a password *is* sent, it's
/// passed to `nmcli` as a plain command-line argument
/// (`802-11-wireless-security.psk <pw>` on the reuse path, `password <pw>`
/// on the create path). For the lifetime of that `nmcli` child, the secret
/// is readable by other local users via `/proc/<pid>/cmdline`.
/// `util::run_with_stdin` exists to feed secrets on stdin instead, but
/// wiring it up correctly needs either verified `nmcli --ask` piped-stdin
/// behavior or NetworkManager's D-Bus secret-agent API — neither of which
/// can be validated without a live NetworkManager connection — so this is
/// left as documented tech debt rather than a guess. In practice this
/// exposure window now only exists on a network's *first* connect: once
/// NetworkManager has the credential, breadcrumbs clears its local copy, so
/// there's nothing left to pass on argv for every subsequent connect.
pub fn connect_verbose(iface: &str, net: &NetworkDef, wait: u32, dns: &str) -> Result<(), String> {
    let wait_s = wait.to_string();

    if let Some(profile) = first_profile_for_ssid(&net.ssid) {
        // Update the saved PSK and, for hidden networks, ensure the flag is set.
        if let Some(pw) = &net.password {
            let _ = run(
                "nmcli",
                &[
                    "connection",
                    "modify",
                    &profile,
                    "802-11-wireless-security.psk",
                    pw.as_str(),
                ],
                Duration::from_secs(6),
            );
        }
        if net.hidden {
            let _ = run(
                "nmcli",
                &[
                    "connection",
                    "modify",
                    &profile,
                    "802-11-wireless.hidden",
                    "yes",
                ],
                Duration::from_secs(6),
            );
        }
        let o = run(
            "nmcli",
            &["--wait", &wait_s, "connection", "up", &profile, "ifname", iface],
            Duration::from_secs(wait as u64 + 15),
        );
        if !o.success {
            let detail = o.stderr.trim().to_string();
            return Err(if detail.is_empty() {
                o.stdout.trim().to_string()
            } else {
                detail
            });
        }
        if let Some(uuid) = active_uuid(iface) {
            enforce_dns(&uuid, iface, dns);
        }
        return Ok(());
    }

    // No saved profile — create one via device wifi connect.
    let hidden = if net.hidden { "yes" } else { "no" };
    let mut args: Vec<&str> = vec![
        "--wait",
        &wait_s,
        "device",
        "wifi",
        "connect",
        net.ssid.as_str(),
    ];
    // Only pass `password` when we actually have one. An empty/missing PSK
    // argument makes nmcli treat the network as open (no security), which is
    // what we want both for genuinely open SSIDs and for a network whose
    // secret NetworkManager should already hold — though the latter case
    // only succeeds if a saved profile in fact exists, which is why we only
    // reach this branch (no saved profile found) when that assumption held.
    if let Some(pw) = &net.password {
        args.push("password");
        args.push(pw.as_str());
    }
    args.push("hidden");
    args.push(hidden);
    args.push("ifname");
    args.push(iface);
    let o = run("nmcli", &args, Duration::from_secs(wait as u64 + 15));
    if !o.success {
        let detail = o.stderr.trim().to_string();
        return Err(if detail.is_empty() {
            o.stdout.trim().to_string()
        } else {
            detail
        });
    }
    if let Some(uuid) = active_uuid(iface) {
        enforce_dns(&uuid, iface, dns);
    }
    Ok(())
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
        let fields = split_fields(line);
        if fields.len() < 2 {
            continue;
        }
        let name = fields[0].clone();
        let typ = &fields[1];
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
    fn split_fields_splits_and_unescapes() {
        // SSID:SIGNAL:SECURITY with an escaped ':' inside the SSID.
        let f = split_fields(r"My\:Net:72:WPA2");
        assert_eq!(f, vec!["My:Net", "72", "WPA2"]);

        // SSID with a space (common in real network names)
        let f = split_fields("My Network:88:WPA2");
        assert_eq!(f, vec!["My Network", "88", "WPA2"]);

        // Empty SSID (hidden) keeps the empty leading field.
        let f = split_fields(":40:WPA3");
        assert_eq!(f, vec!["", "40", "WPA3"]);
    }

    #[test]
    fn split_fields_two_column_with_colon_in_first_field() {
        // A connection NAME or SSID containing a literal ':' must not be
        // mis-split into TYPE — this is what a plain `splitn(2, ':')` gets
        // wrong (e.g. `wifi_interface`/`first_profile_for_ssid` parsing).
        let f = split_fields(r"Office\:5G:802-11-wireless");
        assert_eq!(f, vec!["Office:5G", "802-11-wireless"]);
    }

    #[test]
    fn split_fields_empty_line() {
        assert_eq!(split_fields(""), vec![""]);
    }

    #[test]
    fn split_fields_trailing_backslash_in_field() {
        let f = split_fields(r"trail\\:wifi");
        assert_eq!(f, vec![r"trail\", "wifi"]);
    }
}
