use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/root"))
}

/// Atomically replace `path` with `contents`: write a sibling temp file (created
/// with `mode` on unix) and `rename` it over the target. Avoids torn reads by a
/// concurrent reader (the watch daemon reloads config every tick) and never
/// leaves a half-written file behind on crash. Because the temp file is created
/// with `mode` up front, secrets never exist world-readable even briefly.
pub fn write_atomic(path: &Path, contents: &str, mode: u32) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir)?;
    let stem = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("breadcrumbs");
    let tmp = dir.join(format!(".{stem}.tmp.{}", std::process::id()));

    let mut open = fs::OpenOptions::new();
    open.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;

    let res = (|| {
        let mut f = open.open(&tmp)?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res
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
        // Pin the C locale so message text we parse (nmcli states, monitor
        // lines) is stable English regardless of the user's LANG. SSID/value
        // bytes are unaffected.
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .stdin(stdin_cfg)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return Output::failed(),
    };

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

    // Feed stdin only after the reader threads are draining stdout/stderr, so a
    // child that writes more than a pipe buffer before consuming stdin can't
    // deadlock against our blocking write.
    if let Some(data) = stdin {
        if let Some(mut sink) = child.stdin.take() {
            let _ = sink.write_all(data.as_bytes());
            // Drop closes the pipe so the child's read sees EOF.
        }
    }

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

    #[test]
    fn fmt_epoch_year_2000_century_divisible_by_400_leap() {
        // 2000-01-01 00:00:00 UTC — divisible by 400, so it IS a leap year.
        assert_eq!(fmt_epoch(946_684_800), "2000-01-01 00:00:00");
    }

    #[test]
    fn fmt_epoch_end_of_year_boundary() {
        // 2023-12-31 23:59:59 UTC
        assert_eq!(fmt_epoch(1_704_067_199), "2023-12-31 23:59:59");
    }

    #[test]
    fn fmt_epoch_negative_before_unix_epoch() {
        // 1969-12-31 23:59:59 UTC
        assert_eq!(fmt_epoch(-1), "1969-12-31 23:59:59");
        // 1969-12-31 00:00:00 UTC
        assert_eq!(fmt_epoch(-86_400), "1969-12-31 00:00:00");
    }

    #[test]
    fn fmt_epoch_february_non_leap_year_boundary() {
        // 2023-02-28 00:00:00 UTC (2023 is not a leap year)
        assert_eq!(fmt_epoch(1_677_542_400), "2023-02-28 00:00:00");
        // 2023-03-01 00:00:00 UTC — next day after Feb 28 in non-leap year
        assert_eq!(fmt_epoch(1_677_628_800), "2023-03-01 00:00:00");
    }

    #[test]
    fn fmt_epoch_century_non_leap_year_1900_equivalent() {
        // 1900 is NOT a leap year (div by 100 but not 400).
        // 1900-03-01 00:00:00 UTC: days from epoch = (1900-1970)*365.25 ≈ use known anchor.
        // 2100-02-28 00:00:00 UTC = epoch 4107456000; next day is Mar 1 (not Feb 29).
        // We verify via the leap day boundary: 2100-02-28 + 86400 must be 2100-03-01.
        assert_eq!(fmt_epoch(4_107_456_000), "2100-02-28 00:00:00");
        assert_eq!(fmt_epoch(4_107_456_000 + 86_400), "2100-03-01 00:00:00");
    }

    #[test]
    fn fmt_epoch_midnight_vs_end_of_day() {
        // 2022-06-15 00:00:00 UTC
        assert_eq!(fmt_epoch(1_655_251_200), "2022-06-15 00:00:00");
        // 2022-06-15 23:59:59 UTC
        assert_eq!(fmt_epoch(1_655_337_599), "2022-06-15 23:59:59");
    }

    #[test]
    fn fmt_epoch_time_of_day_components() {
        // 1970-01-01 01:02:03 UTC
        assert_eq!(fmt_epoch(3723), "1970-01-01 01:02:03");
        // 1970-01-01 23:59:59 UTC
        assert_eq!(fmt_epoch(86_399), "1970-01-01 23:59:59");
    }
}
