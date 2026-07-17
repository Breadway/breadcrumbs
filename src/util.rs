use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
///
/// Delegates to `bread_utils::atomic::write_atomic` — this crate's own
/// version of this function was promoted verbatim into the shared
/// `bread-utils` crate (see that crate's `atomic` module doc comment) as
/// the one implementation in the ecosystem that already got this right.
pub fn write_atomic(path: &Path, contents: &str, mode: u32) -> std::io::Result<()> {
    bread_utils::atomic::write_atomic(path, contents, Some(mode))
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

// `Output`, `run`, `run_with_stdin`, and `run_ok` used to be implemented
// directly in this module; that implementation was promoted verbatim into
// `bread_utils::proc` as the shared timeout-guarded subprocess runner for
// the whole ecosystem (several sibling repos shelled out with no timeout at
// all). Re-exported here so every existing `crate::util::{run, ...}` call
// site in this crate keeps working unchanged.
#[allow(unused_imports)] // Output isn't named directly elsewhere in this crate, but stays part of this module's public API.
pub use bread_utils::proc::{run, run_ok, run_with_stdin, Output};

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
