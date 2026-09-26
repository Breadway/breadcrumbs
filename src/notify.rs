use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::time::Duration;

use crate::config::{log_path, state_dir};
use crate::util::{command_exists, run, timestamp};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Urgency {
    Low,
    Normal,
    Critical,
}

impl Urgency {
    fn as_str(self) -> &'static str {
        match self {
            Urgency::Low => "low",
            Urgency::Normal => "normal",
            Urgency::Critical => "critical",
        }
    }
}

const MAX_LOG_BYTES: u64 = 512 * 1024;
const KEEP_LINES: usize = 300;

/// Append a timestamped line to the breadcrumbs log.
///
/// The log carries SSID history (effectively location history), so the
/// state dir is kept `0700` and the log `0600`. Rotation and appends from
/// the watch daemon and one-shot commands are serialised by a dedicated
/// `.log.lock` (not the state dir's `.lock`, which `State::save` holds, so
/// logging can never deadlock against it), and rotation replaces the file
/// atomically so a concurrent append is never lost into a truncated file.
pub fn log(line: &str) {
    let dir = state_dir();
    if fs::create_dir_all(&dir).is_ok() {
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    }
    let _guard = crate::lock::acquire_file(&dir.join(".log.lock")).ok();
    let path = log_path();
    if let Ok(meta) = fs::metadata(&path) {
        if meta.len() > MAX_LOG_BYTES {
            if let Ok(bytes) = fs::read(&path) {
                // Lossy: a stray non-UTF-8 byte must not stop rotation
                // forever and let the log grow without bound.
                let text = String::from_utf8_lossy(&bytes);
                let tail: Vec<&str> = text.lines().rev().take(KEEP_LINES).collect();
                let kept: String = tail.into_iter().rev().collect::<Vec<_>>().join("\n");
                let _ = bread_utils::atomic::write_atomic(&path, &(kept + "\n"), Some(0o600));
            }
        }
    }
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&path)
    {
        // Tighten a log created world-readable by an older version.
        let _ = f.set_permissions(fs::Permissions::from_mode(0o600));
        let _ = writeln!(f, "{} {}", timestamp(), line);
    }
}

/// Desktop notification (best effort) + log entry.
pub fn notify(summary: &str, body: &str, urgency: Urgency) {
    log(&format!(
        "[notify/{}] {} {}",
        urgency.as_str(),
        summary,
        if body.is_empty() {
            String::new()
        } else {
            format!("- {body}")
        }
    ));
    if !command_exists("notify-send") {
        return;
    }
    let timeout_ms = match urgency {
        Urgency::Critical => "0",
        Urgency::Normal => "6000",
        Urgency::Low => "3500",
    };
    let mut args = vec![
        "-a",
        "breadcrumbs",
        "-u",
        urgency.as_str(),
        "-t",
        timeout_ms,
        "-h",
        "string:x-canonical-private-synchronous:breadcrumbs",
        summary,
    ];
    if !body.is_empty() {
        args.push(body);
    }
    let _ = run("notify-send", &args, Duration::from_secs(5));
}
