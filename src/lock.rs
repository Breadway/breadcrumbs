//! Cross-process advisory locking for the config and state directories.
//!
//! `breadcrumbs watch` runs continuously alongside one-shot commands
//! (`add`, `forget`, `scan`, `profile set`, and `flow::run`'s own
//! credential clearing). Each does a load, modify, write cycle over the
//! same `breadcrumbs.toml` / `networks.toml` / `state.toml`. Without
//! serialisation two concurrent cycles can interleave and the later write
//! silently drops the earlier one's change.
//!
//! Holding an exclusive `flock(2)` on a sidecar `.lock` file for the whole
//! cycle makes concurrent cycles queue instead. `Config::load` / `save` and
//! `State::load` themselves stay lock free: the lock is taken by the small
//! number of call sites that own a full cycle (`app::real_main`, the
//! `watch` loop) so the same process never tries to take it twice and
//! deadlock. Writes are also atomic (temp file then rename), so a reader
//! that does not take the lock still never sees a half-written file.

use std::fs::OpenOptions;
use std::path::Path;

/// Held for the duration of a load, modify, write cycle. Dropping it (or
/// the process exiting) releases the lock. The `.lock` file is left on
/// disk: creating and unlinking it per acquisition would reintroduce a
/// race on the path itself.
#[must_use = "the lock is released as soon as the guard is dropped"]
pub struct Guard {
    _file: std::fs::File,
}

/// Take an exclusive advisory lock on `<dir>/.lock`, creating the directory
/// and the file if needed. Blocks until the lock is free.
///
/// An error here is unusual (a filesystem with no `flock` support, a
/// permissions problem on the config dir). Callers treat it as non-fatal:
/// they log and proceed unlocked rather than refuse to run.
pub fn acquire(dir: &Path) -> Result<Guard, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let path = dir.join(".lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        // The lock file's contents are irrelevant; never truncate it.
        .truncate(false)
        .open(&path)
        .map_err(|e| format!("opening {}: {e}", path.display()))?;
    file.lock()
        .map_err(|e| format!("locking {}: {e}", path.display()))?;
    Ok(Guard { _file: file })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_then_release_lets_a_second_acquire_through() {
        let dir =
            std::env::temp_dir().join(format!("breadcrumbs-lock-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let g = acquire(&dir).expect("first acquire");
        drop(g);
        let _g2 = acquire(&dir).expect("second acquire after release");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_second_acquire_waits_until_the_first_guard_is_dropped() {
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        let dir =
            std::env::temp_dir().join(format!("breadcrumbs-lock-wait-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let held = acquire(&dir).expect("first acquire");
        let hold_for = Duration::from_millis(150);

        let (tx, rx) = mpsc::channel();
        let dir2 = dir.clone();
        // Captured here, before the spawn, and moved into the waiter rather than
        // each side timing itself: the waiter starting its own clock only after
        // the OS actually schedules it (vs. this thread's recv_timeout, which
        // starts ticking immediately) let it race ahead by tens of microseconds,
        // occasionally landing `waited` a hair under `hold_for` even though the
        // lock genuinely blocked for the full duration (see CRUMB-10 CI flake).
        let start = Instant::now();
        let waiter = std::thread::spawn(move || {
            let _g = acquire(&dir2).expect("second acquire");
            tx.send(start.elapsed()).unwrap();
        });

        // The waiter must not get the lock while we still hold it.
        assert!(
            rx.recv_timeout(hold_for).is_err(),
            "second acquire returned while the first guard was still held"
        );
        drop(held);

        let waited = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("waiter finished");
        assert!(
            waited >= hold_for,
            "second acquire returned after {waited:?}, expected to block for at least {hold_for:?}"
        );
        waiter.join().unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lock_file_is_reused_not_recreated() {
        let dir =
            std::env::temp_dir().join(format!("breadcrumbs-lock-reuse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        drop(acquire(&dir).expect("acquire"));
        assert!(
            dir.join(".lock").exists(),
            ".lock should persist between acquisitions"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
