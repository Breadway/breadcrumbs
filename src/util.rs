use std::cell::RefCell;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/root"))
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

/// Everything breadcrumbs does to touch the outside world *other than* its
/// own file I/O and env var reads: spawning an external program, and
/// checking whether one is available at all. Every call site in this crate
/// (`nm.rs`, `tailscale.rs`, `status.rs`, `notify.rs`, `app.rs`) goes through
/// the free functions below (`run`/`run_with_stdin`/`run_ok`/
/// `command_exists`), which are thin wrappers dispatching to whatever
/// `Runner` is currently installed in the thread-local slot (`RealRunner` by
/// default).
///
/// Tests swap in a fake implementation via [`with_runner`] so real call
/// chains (`flow::run`, `watch::classify`, …) can be driven in-process
/// against canned output, with no subprocess ever spawned and full
/// visibility into exactly what *would* have been executed: the natural
/// mechanism for asserting things like "no password ever reaches nmcli's
/// argv on a repeat connect" (see the credential tests under `tests/`).
///
/// A thread-local (rather than an explicit parameter threaded through every
/// function) was chosen so the large existing call surface in `nm.rs` et al.
/// didn't need every signature rewritten to carry a `&dyn Runner`; call
/// sites are unchanged, only `util`'s internals dispatch differently. It's
/// safe across `cargo test`'s parallel test threads because each thread gets
/// its own independent slot, defaulting to `RealRunner`, so tests that don't
/// install a fake are unaffected by ones that do.
pub trait Runner {
    fn run(&self, prog: &str, args: &[&str], stdin: Option<&str>, timeout: Duration) -> Output;
    fn command_exists(&self, name: &str) -> bool;
}

struct RealRunner;

impl Runner for RealRunner {
    fn run(&self, prog: &str, args: &[&str], stdin: Option<&str>, timeout: Duration) -> Output {
        spawn_run(prog, args, stdin, timeout)
    }

    fn command_exists(&self, name: &str) -> bool {
        path_lookup_exists(name)
    }
}

fn path_lookup_exists(name: &str) -> bool {
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            if dir.join(name).is_file() {
                return true;
            }
        }
    }
    false
}

thread_local! {
    static RUNNER: RefCell<Box<dyn Runner>> = RefCell::new(Box::new(RealRunner));
}

pub fn command_exists(name: &str) -> bool {
    RUNNER.with(|r| r.borrow().command_exists(name))
}

/// Run a command with a hard timeout. The child is killed if it overruns so a
/// hung subprocess can never wedge the daemon.
pub fn run(prog: &str, args: &[&str], timeout: Duration) -> Output {
    run_with_stdin(prog, args, None, timeout)
}

/// Like [`run`], but feeds `stdin` to the child's standard input.
/// (Wi-Fi secrets no longer go through here: `nm` sends them inside D-Bus
/// payloads, never on a command line.)
pub fn run_with_stdin(prog: &str, args: &[&str], stdin: Option<&str>, timeout: Duration) -> Output {
    RUNNER.with(|r| r.borrow().run(prog, args, stdin, timeout))
}

pub fn run_ok(prog: &str, args: &[&str], timeout: Duration) -> bool {
    run(prog, args, timeout).success
}

/// Swap the thread-local [`Runner`] for `runner` for the duration of `f`,
/// restoring whatever was previously installed afterward, even if `f`
/// panics, so a failing assertion inside a test can't leak a fake runner
/// into whatever test happens to run next on this thread. This is the seam
/// integration tests use to drive real logic without spawning subprocesses.
pub fn with_runner<R, T>(runner: R, f: impl FnOnce() -> T) -> T
where
    R: Runner + 'static,
{
    let prev = RUNNER.with(|r| std::mem::replace(&mut *r.borrow_mut(), Box::new(runner)));
    // The cached UTC offset came from whichever runner was installed when
    // it was read; never let it leak across a runner swap.
    UTC_OFFSET.with(|c| *c.borrow_mut() = None);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    RUNNER.with(|r| *r.borrow_mut() = prev);
    UTC_OFFSET.with(|c| *c.borrow_mut() = None);
    match result {
        Ok(v) => v,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

/// How long to wait for the output reader threads after the child has
/// exited (or been killed). A grandchild that inherited the pipes keeps
/// them open after the child is gone; past this grace the readers are
/// abandoned rather than letting that grandchild wedge the caller.
const READER_GRACE: Duration = Duration::from_secs(2);

fn spawn_reader(pipe: Option<impl Read + Send + 'static>) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = String::new();
        if let Some(mut p) = pipe {
            let _ = p.read_to_string(&mut buf);
        }
        let _ = tx.send(buf);
    });
    rx
}

fn spawn_run(prog: &str, args: &[&str], stdin: Option<&str>, timeout: Duration) -> Output {
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

    let out_rx = spawn_reader(child.stdout.take());
    let err_rx = spawn_reader(child.stderr.take());

    // Feed stdin on its own thread: a child that never reads its input
    // would otherwise block this write forever (past the pipe buffer),
    // before the timeout below ever gets a chance to fire.
    if let Some(data) = stdin {
        if let Some(mut sink) = child.stdin.take() {
            let data = data.to_string();
            thread::spawn(move || {
                let _ = sink.write_all(data.as_bytes());
                // Drop closes the pipe so the child's read sees EOF.
            });
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

    let stdout = out_rx.recv_timeout(READER_GRACE).unwrap_or_default();
    let stderr = err_rx.recv_timeout(READER_GRACE).unwrap_or_default();

    Output {
        success: status.map(|s| s.success()).unwrap_or(false),
        stdout,
        stderr,
    }
}

thread_local! {
    /// Local UTC offset in seconds and when it was read. Re-read every
    /// [`OFFSET_TTL`] so a DST change or timezone switch is picked up,
    /// without forking `date` for every log line and every watch tick.
    static UTC_OFFSET: RefCell<Option<(Instant, Option<i64>)>> = const { RefCell::new(None) };
}

const OFFSET_TTL: Duration = Duration::from_secs(600);

/// Parse `date +%z` output ("+0930", "-0500") into seconds east of UTC.
fn parse_utc_offset(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.len() != 5 {
        return None;
    }
    let sign = match &s[..1] {
        "+" => 1,
        "-" => -1,
        _ => return None,
    };
    let h: i64 = s[1..3].parse().ok()?;
    let m: i64 = s[3..5].parse().ok()?;
    Some(sign * (h * 3600 + m * 60))
}

fn utc_offset() -> Option<i64> {
    if let Some(cached) = UTC_OFFSET.with(|c| {
        c.borrow()
            .filter(|(at, _)| at.elapsed() < OFFSET_TTL)
            .map(|(_, off)| off)
    }) {
        return cached;
    }
    let o = run("date", &["+%z"], Duration::from_secs(2));
    let off = if o.success {
        parse_utc_offset(&o.stdout)
    } else {
        None
    };
    UTC_OFFSET.with(|c| *c.borrow_mut() = Some((Instant::now(), off)));
    off
}

fn epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64
}

/// Current local "HH:MM" (24h), for the time-of-day schedule. `None` if the
/// local offset can't be determined: the schedule is skipped, never guessed.
pub fn local_hhmm() -> Option<String> {
    let local = epoch_secs() + utc_offset()?;
    Some(fmt_epoch(local)[11..16].to_string())
}

/// Local "YYYY-MM-DD HH:MM:SS", falling back to UTC if the local offset
/// can't be determined.
pub fn timestamp() -> String {
    fmt_epoch(epoch_secs() + utc_offset().unwrap_or(0))
}

/// Seconds on a clock that keeps running while the machine is suspended
/// (`CLOCK_BOOTTIME`, as exposed by `/proc/uptime`). `Instant` is
/// `CLOCK_MONOTONIC` on Linux, which stops during suspend, so it can't be
/// used to notice that the laptop just slept for an hour. Falls back to
/// wall-clock seconds where `/proc/uptime` is unavailable.
pub fn boot_secs() -> f64 {
    std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0)
        })
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
    fn fmt_epoch_pre_1970_is_handled() {
        // The div_euclid/rem_euclid split must stay correct for negative
        // epoch seconds (dates before 1970), not just the common positive case.
        assert_eq!(fmt_epoch(-86_400), "1969-12-31 00:00:00");
    }

    #[test]
    fn fmt_epoch_year_and_month_boundaries() {
        assert_eq!(fmt_epoch(1_704_067_199), "2023-12-31 23:59:59");
        assert_eq!(fmt_epoch(1_735_689_600), "2025-01-01 00:00:00");
        // Last second of October (non-leap-day month boundary).
        assert_eq!(fmt_epoch(1_730_419_199), "2024-10-31 23:59:59");
    }

    #[test]
    fn parse_utc_offset_handles_signs_and_rejects_garbage() {
        assert_eq!(parse_utc_offset("+0930\n"), Some(9 * 3600 + 30 * 60));
        assert_eq!(parse_utc_offset("-0500"), Some(-5 * 3600));
        assert_eq!(parse_utc_offset("+0000"), Some(0));
        assert_eq!(parse_utc_offset("UTC"), None);
        assert_eq!(parse_utc_offset("0930"), None);
    }

    #[test]
    fn run_does_not_wedge_on_a_grandchild_holding_the_pipe() {
        // The child exits at once but leaves a background sleeper holding
        // stdout open. `run` must return after the reader grace period,
        // not wait for the sleeper.
        let start = Instant::now();
        let o = run(
            "sh",
            &["-c", "echo hi; sleep 30 & exit 0"],
            Duration::from_secs(5),
        );
        assert!(o.success);
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn run_times_out_a_child_that_never_reads_its_stdin() {
        // Larger than a pipe buffer: the old in-line write blocked here
        // forever, before the timeout loop started.
        let big = "x".repeat(1 << 20);
        let start = Instant::now();
        let o = run_with_stdin("sleep", &["30"], Some(&big), Duration::from_millis(300));
        assert!(!o.success);
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn command_exists_false_for_bogus_binary() {
        assert!(!command_exists("definitely-not-a-real-binary-xyz123"));
    }

    #[test]
    fn command_exists_true_for_a_real_binary() {
        // `sh` is guaranteed present on any POSIX system this runs on.
        assert!(command_exists("sh"));
    }

    #[test]
    fn run_on_missing_binary_fails_cleanly_instead_of_panicking() {
        let o = run(
            "definitely-not-a-real-binary-xyz123",
            &[],
            Duration::from_secs(1),
        );
        assert!(!o.success);
        assert_eq!(o.stdout, "");
    }
}
