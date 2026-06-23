use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use crate::config::{state_dir, NetworkDef};
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

/// Return true if `name` is NetworkManager's numbered-duplicate convention for
/// `ssid`: exactly `ssid` followed by a space and one or more decimal digits
/// (e.g. "MyNet 1", "MyNet 2"). A name like "MyNet1" (no space) is a distinct
/// SSID and must not match.
fn is_numbered_nm_duplicate(name: &str, ssid: &str) -> bool {
    if let Some(suffix) = name.strip_prefix(ssid) {
        if let Some(digits) = suffix.strip_prefix(' ') {
            return !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit());
        }
    }
    false
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
        // NAME may itself contain an (escaped) ':', so a plain splitn would
        // mis-split it — parse the line the same way nmcli escapes it.
        let fields = parse_scan_line(line);
        if fields.len() < 2 || !fields[1].contains("wireless") {
            continue;
        }
        let name = fields[0].clone();
        if name == ssid {
            return Some(name);
        }
        if fallback.is_none() && is_numbered_nm_duplicate(&name, ssid) {
            fallback = Some(name);
        }
    }
    fallback
}

/// Write the PSK to a 0600 file in the `setting.property:value` format nmcli's
/// `passwd-file` expects, so the secret reaches NetworkManager without ever
/// appearing in argv (where any local user could read it via `ps`). Returns the
/// path; the caller is responsible for removing it.
fn write_psk_file(password: &str) -> Option<PathBuf> {
    use std::io::Write;
    // Prefer $XDG_RUNTIME_DIR (per-user tmpfs, mode 0700, wiped on logout) for a
    // transient secret; fall back to the on-disk state dir only if it's unset.
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(state_dir);
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("breadcrumbs.psk.{}", std::process::id()));
    let mut f = {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&path)
                .ok()?
        }
        #[cfg(not(unix))]
        {
            std::fs::File::create(&path).ok()?
        }
    };
    f.write_all(format!("802-11-wireless-security.psk:{password}\n").as_bytes())
        .ok()?;
    Some(path)
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
pub fn connect_verbose(iface: &str, net: &NetworkDef, wait: u32, dns: &str) -> Result<(), String> {
    let wait_s = wait.to_string();

    if let Some(profile) = first_profile_for_ssid(&net.ssid) {
        // Ensure the hidden flag is set — this carries no secret, so passing it
        // in argv is safe.
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
        // Supply the PSK to the activation via a 0600 passwd-file rather than
        // argv. Harmless if NM already has a matching secret stored.
        let psk_file = if net.password.is_empty() {
            None
        } else {
            write_psk_file(&net.password)
        };
        let psk_path = psk_file.as_ref().map(|p| p.display().to_string());
        let mut args: Vec<&str> = vec![
            "--wait",
            &wait_s,
            "connection",
            "up",
            &profile,
            "ifname",
            iface,
        ];
        if let Some(ref p) = psk_path {
            args.push("passwd-file");
            args.push(p.as_str());
        }
        let o = run("nmcli", &args, Duration::from_secs(wait as u64 + 15));
        if let Some(p) = psk_file {
            let _ = std::fs::remove_file(p);
        }
        if o.success {
            if let Some(uuid) = active_uuid(iface) {
                enforce_dns(&uuid, iface, dns);
            }
            return Ok(());
        }
        // Activation failed (e.g. the saved secret is stale). Fall through to a
        // fresh connect below, which re-supplies the PSK over stdin.
    }

    // No saved profile (or reuse failed) — create/refresh via device wifi
    // connect. `--ask` makes nmcli read the PSK from stdin instead of argv.
    let hidden = if net.hidden { "yes" } else { "no" };
    let args = [
        "--ask",
        "--wait",
        &wait_s,
        "device",
        "wifi",
        "connect",
        net.ssid.as_str(),
        "hidden",
        hidden,
        "ifname",
        iface,
    ];
    let stdin = format!("{}\n", net.password);
    let o = run_with_stdin(
        "nmcli",
        &args,
        Some(&stdin),
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
        // NAME may contain an escaped ':' — parse rather than naive-split.
        let fields = parse_scan_line(line);
        if fields.len() < 2 || !fields[1].contains("wireless") {
            continue;
        }
        let name = fields[0].clone();
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
    fn unescape_empty_string() {
        assert_eq!(unescape(""), "");
    }

    #[test]
    fn unescape_multiple_consecutive_escapes() {
        assert_eq!(unescape(r"a\:b\:c"), "a:b:c");
    }

    #[test]
    fn unescape_double_backslash_produces_single() {
        assert_eq!(unescape(r"a\\b"), r"a\b");
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

    #[test]
    fn parse_scan_line_single_field_no_separators() {
        let f = parse_scan_line("OnlySSID");
        assert_eq!(f, vec!["OnlySSID"]);
    }

    #[test]
    fn parse_scan_line_empty_input_yields_one_empty_field() {
        let f = parse_scan_line("");
        assert_eq!(f, vec![""]);
    }

    #[test]
    fn parse_scan_line_all_empty_fields() {
        // Three colons → four empty fields.
        let f = parse_scan_line(":::");
        assert_eq!(f, vec!["", "", "", ""]);
    }

    #[test]
    fn parse_scan_line_multiple_escaped_colons_in_ssid() {
        let f = parse_scan_line(r"a\:b\:c:80:WPA3");
        assert_eq!(f, vec!["a:b:c", "80", "WPA3"]);
    }

    #[test]
    fn parse_scan_line_backslash_escape_then_colon_separator() {
        // "abc\:60:WPA2" — \: is an escaped colon inside the SSID, not a separator.
        let f = parse_scan_line(r"abc\:60:WPA2");
        assert_eq!(f, vec!["abc:60", "WPA2"]);
    }

    #[test]
    fn is_numbered_nm_duplicate_exact_match_is_not_duplicate() {
        assert!(!is_numbered_nm_duplicate("Net", "Net"));
    }

    #[test]
    fn is_numbered_nm_duplicate_space_digits_matches() {
        assert!(is_numbered_nm_duplicate("Net 1", "Net"));
        assert!(is_numbered_nm_duplicate("My Network 12", "My Network"));
    }

    #[test]
    fn is_numbered_nm_duplicate_no_space_does_not_match() {
        // "Net1" is a distinct SSID, not a numbered duplicate of "Net".
        assert!(!is_numbered_nm_duplicate("Net1", "Net"));
        assert!(!is_numbered_nm_duplicate("HomeWifi2", "HomeWifi"));
    }

    #[test]
    fn is_numbered_nm_duplicate_non_numeric_suffix_does_not_match() {
        assert!(!is_numbered_nm_duplicate("Net foo", "Net"));
        assert!(!is_numbered_nm_duplicate("Net 1x", "Net"));
    }

    #[test]
    fn is_numbered_nm_duplicate_empty_digits_does_not_match() {
        // "Net " (trailing space only, no digits) must not match.
        assert!(!is_numbered_nm_duplicate("Net ", "Net"));
    }

    #[test]
    fn is_numbered_nm_duplicate_unrelated_name_does_not_match() {
        assert!(!is_numbered_nm_duplicate("OtherNet 1", "Net"));
    }
}
