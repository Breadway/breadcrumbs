use std::fs::{self, OpenOptions};
use std::io::Write;
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

pub fn log(line: &str) {
    let _ = fs::create_dir_all(state_dir());
    let path = log_path();
    if let Ok(meta) = fs::metadata(&path) {
        if meta.len() > MAX_LOG_BYTES {
            if let Ok(text) = fs::read_to_string(&path) {
                let tail: Vec<&str> = text.lines().rev().take(300).collect();
                let kept: String = tail.into_iter().rev().collect::<Vec<_>>().join("\n");
                let _ = fs::write(&path, kept + "\n");
            }
        }
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
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
