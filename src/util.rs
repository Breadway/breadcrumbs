use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/root"))
}

pub fn command_exists(name: &str) -> bool {
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            if dir.join(name).is_file() {
                return true;
            }
        }
    }
    false
}

#[derive(Debug, Clone)]
pub struct Output {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn failed() -> Output {
        Output {
            success: false,
            stdout: String::new(),
            stderr: String::new(),
        }
    }
}

/// Run a command with a hard timeout. The child is killed if it overruns so a
/// hung nmcli/tailscale can never wedge the daemon.
pub fn run(prog: &str, args: &[&str], timeout: Duration) -> Output {
    run_with_stdin(prog, args, None, timeout)
}

/// Like [`run`], but feeds `stdin` to the child's standard input. Used to hand
/// secrets (e.g. Wi-Fi PSKs) to `nmcli --ask` without exposing them in argv,
/// where any local user could read them via `ps`.
pub fn run_with_stdin(prog: &str, args: &[&str], stdin: Option<&str>, timeout: Duration) -> Output {
    let stdin_cfg = if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    };
    let mut child = match Command::new(prog)
        .args(args)
        .stdin(stdin_cfg)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return Output::failed(),
    };

    if let Some(data) = stdin {
        if let Some(mut sink) = child.stdin.take() {
            let _ = sink.write_all(data.as_bytes());
            // Drop closes the pipe so the child's read sees EOF.
        }
    }

    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();

    let out_handle = thread::spawn(move || {
        let mut buf = String::new();
        if let Some(ref mut p) = stdout_pipe {
            let _ = p.read_to_string(&mut buf);
        }
        buf
    });
    let err_handle = thread::spawn(move || {
        let mut buf = String::new();
        if let Some(ref mut p) = stderr_pipe {
            let _ = p.read_to_string(&mut buf);
        }
        buf
    });

    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(_) => break None,
        }
    };

    let stdout = out_handle.join().unwrap_or_default();
    let stderr = err_handle.join().unwrap_or_default();

    Output {
        success: status.map(|s| s.success()).unwrap_or(false),
        stdout,
        stderr,
    }
}

pub fn run_ok(prog: &str, args: &[&str], timeout: Duration) -> bool {
    run(prog, args, timeout).success
}

/// Local "YYYY-MM-DD HH:MM:SS". Uses `date` for correct local time, falling
/// back to a dependency-free UTC computation if it is unavailable.
pub fn timestamp() -> String {
    let o = run("date", &["+%Y-%m-%d %H:%M:%S"], Duration::from_secs(2));
    if o.success {
        let t = o.stdout.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    timestamp_utc()
}

/// epoch seconds -> "YYYY-MM-DD HH:MM:SS" (UTC), no external deps.
fn timestamp_utc() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    fmt_epoch(secs)
}

/// Format UTC epoch seconds as "YYYY-MM-DD HH:MM:SS" (pure / testable).
fn fmt_epoch(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (h, mi, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);

    // civil_from_days (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_epoch_known_values() {
        assert_eq!(fmt_epoch(0), "1970-01-01 00:00:00");
        // 2001-09-09 01:46:40 UTC
        assert_eq!(fmt_epoch(1_000_000_000), "2001-09-09 01:46:40");
        // 2021-01-01 00:00:00 UTC
        assert_eq!(fmt_epoch(1_609_459_200), "2021-01-01 00:00:00");
        // Leap day 2024-02-29 12:00:00 UTC
        assert_eq!(fmt_epoch(1_709_208_000), "2024-02-29 12:00:00");
    }
}
