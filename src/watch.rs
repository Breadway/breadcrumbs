use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use bread_utils::bread_client::BreadClient;

use crate::bread_events;
use crate::config::Config;
use crate::flow;
use crate::notify::{log, notify, Urgency};
use crate::state::State;
use crate::status::{self};
use crate::tailscale::TsHealth;

/// Coarse health classification the watch loop reacts to each tick. `pub`
/// (and so is [`classify`]) purely so integration tests can drive the real
/// classification logic in-process against a faked [`crate::util::Runner`],
/// instead of only being able to observe it indirectly through the watch
/// loop's side effects.
#[derive(PartialEq, Eq, Clone, Debug)]
pub enum Health {
    Up,
    DownNoNet,
    DownTailscaleManual,
    DownTailscaleOther,
    NoAdapter,
    /// `profile` isn't defined in the config (e.g. state still points at a
    /// custom profile the user deleted from breadcrumbs.toml).
    UnknownProfile,
}

impl Health {
    /// Wire name used in `bread.crumbs.health.changed` — the Rust variant
    /// as a string, not a prettier label.
    pub fn as_str(&self) -> &'static str {
        match self {
            Health::Up => "Up",
            Health::DownNoNet => "DownNoNet",
            Health::DownTailscaleManual => "DownTailscaleManual",
            Health::DownTailscaleOther => "DownTailscaleOther",
            Health::NoAdapter => "NoAdapter",
            Health::UnknownProfile => "UnknownProfile",
        }
    }
}

pub fn classify(cfg: &Config, profile: &str) -> (Health, Option<String>) {
    // Checked before gather(): a profile missing from config would otherwise
    // silently fall back to "tailscale not required" and read as healthy off
    // of nothing but a bare internet check, never surfacing the misconfig.
    if cfg.profile(profile).is_none() {
        return (Health::UnknownProfile, None);
    }
    let s = status::gather(cfg, profile);
    if s.iface.is_none() {
        return (Health::NoAdapter, None);
    }
    let ssid = s.ssid.clone();
    if !s.internet {
        return (Health::DownNoNet, ssid);
    }
    if s.tailscale_required {
        match s.tailscale {
            Some(TsHealth::Ok) => (Health::Up, ssid),
            Some(TsHealth::NeedsLogin) | Some(TsHealth::NotInstalled) => {
                (Health::DownTailscaleManual, ssid)
            }
            Some(_) => (Health::DownTailscaleOther, ssid),
            None => (Health::DownTailscaleManual, ssid),
        }
    } else {
        (Health::Up, ssid)
    }
}

/// Whether a debounced signal is allowed to fire. `None` (never fired) always
/// fires; otherwise it fires only once more than `gap` has elapsed since the
/// last fire. Pulled out as a pure helper so the debounce logic is testable and
/// so the "first event fires immediately" case is expressed without the
/// panic-prone `Instant::now() - gap` seed.
fn debounce_ready(last: Option<Instant>, gap: Duration) -> bool {
    last.map(|t| t.elapsed() > gap).unwrap_or(true)
}

/// Tail `nmcli monitor` and ping the channel on link-state churn so we react
/// to drops within a second instead of waiting out the poll interval.
fn spawn_nm_monitor(tx: mpsc::Sender<()>) {
    thread::spawn(move || loop {
        let child = Command::new("nmcli")
            .arg("monitor")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(_) => {
                thread::sleep(Duration::from_secs(10));
                continue;
            }
        };
        if let Some(out) = child.stdout.take() {
            let reader = BufReader::new(out);
            // `None` means "haven't fired yet, so fire on the first interesting
            // line". Storing an `Option` instead of seeding with
            // `Instant::now() - 10s` avoids a panic: `Instant - Duration`
            // underflows (and panics) when the monotonic clock is younger than
            // the offset, which happens if `watch` starts within ~10s of boot —
            // exactly when the systemd unit (ordered after graphical-session)
            // tends to launch.
            let mut last: Option<Instant> = None;
            for line in reader.lines().map_while(Result::ok) {
                let l = line.to_lowercase();
                let interesting =
                    l.contains("disconnect") || l.contains("unavailable") || l.contains("failed");
                if interesting && debounce_ready(last, Duration::from_millis(1500)) {
                    last = Some(Instant::now());
                    let _ = tx.send(());
                }
            }
        }
        let _ = child.wait();
        // monitor died (NM restart?) — back off and respawn.
        thread::sleep(Duration::from_secs(5));
    });
}

/// Sleep up to `dur`, but wake early if `nmcli monitor` signals link churn.
fn wait_for_tick(rx: &Receiver<()>, dur: Duration) {
    match rx.recv_timeout(dur) {
        Ok(()) => {
            // Drain any burst of events so we don't re-fire immediately.
            while rx.try_recv().is_ok() {}
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {}
        // Monitor thread gone (shouldn't happen: we hold the sender) — fall
        // back to a plain sleep so we don't busy-spin.
        Err(mpsc::RecvTimeoutError::Disconnected) => thread::sleep(dur),
    }
}

pub fn run(mut cfg: Config, run_initial: bool) -> i32 {
    let base = cfg.settings.watch_interval.max(4);
    notify(
        "breadcrumbs watcher started",
        "Monitoring Wi-Fi; will auto-recover drops.",
        Urgency::Low,
    );
    log("watch: started");

    let (tx, rx) = mpsc::channel::<()>();
    spawn_nm_monitor(tx.clone());

    // Long-lived, so this uses BreadClient::subscribe (a persistent
    // background thread with its own reconnect/backoff loop). breadd being
    // absent or restarting is transparent: the subscription just quietly
    // stops delivering commands until it reconnects. A successful
    // `set_profile` wakes this loop the same way `nmcli monitor` does, so
    // the new profile is applied on the next tick instead of waiting out
    // the current poll interval.
    let bread = BreadClient::connect(bread_events::APP_ID);
    let wake = tx;
    let _commands = bread.subscribe("bread.command.crumbs.**", move |event| {
        if bread_events::handle_command(&event) {
            let _ = wake.send(());
        }
    });

    let mut profile = State::load(&cfg.settings.default_profile).profile;
    if run_initial {
        // Don't churn an already-working connection on (re)start.
        let (h, _) = classify(&cfg, &profile);
        if h == Health::Up {
            log(&format!(
                "watch: already healthy on start (profile={profile}); skipping initial flow"
            ));
        } else {
            log(&format!("watch: initial flow for profile={profile}"));
            let _ = flow::run(&mut cfg, &profile);
        }
    }

    let mut prev_health: Option<Health> = None;
    let mut prev_profile = profile.clone();
    let mut fail_streak: u32 = 0;
    let mut last_flow_at: Option<Instant> = None;
    const FLOW_COOLDOWN: u64 = 20;

    loop {
        // Reload config + state so edits and `profile set` take effect live.
        // This always runs *before* `flow::run` below (never after, within
        // the same tick), so a password `flow::run` clears-and-saves this
        // iteration is durably on disk by the time the *next* iteration's
        // reload runs — there's no window where a stale (still-has-password)
        // reload could clobber the save, since the two never race on
        // different threads: everything here is sequential on this loop.
        if let Ok(fresh) = Config::load() {
            cfg = fresh;
        }
        profile = State::load(&cfg.settings.default_profile).profile;

        let profile_changed = profile != prev_profile;
        if profile_changed {
            log(&format!(
                "watch: profile changed {prev_profile} -> {profile}"
            ));
            notify(
                "breadcrumbs: profile changed",
                &format!("{prev_profile} -> {profile}"),
                Urgency::Low,
            );
            bread_events::emit_profile_changed(&bread, &prev_profile, &profile);
            prev_profile = profile.clone();
            prev_health = None; // force re-evaluation/recovery for new profile
            last_flow_at = None; // allow immediate recovery on profile change
        }

        let (health, ssid) = classify(&cfg, &profile);
        let transition = prev_health.as_ref() != Some(&health);
        if transition {
            bread_events::emit_health_changed(&bread, &profile, health.as_str(), ssid.as_deref());
        }

        match &health {
            Health::Up => {
                if transition && prev_health.is_some() {
                    notify(
                        "breadcrumbs: back online",
                        &format!(
                            "{} ({profile})",
                            ssid.clone().unwrap_or_else(|| "Wi-Fi".into())
                        ),
                        Urgency::Low,
                    );
                }
                fail_streak = 0;
            }
            Health::NoAdapter => {
                if transition {
                    notify(
                        "breadcrumbs: no Wi-Fi adapter",
                        "Hardware issue — manual check needed.",
                        Urgency::Critical,
                    );
                }
                fail_streak = fail_streak.saturating_add(1);
            }
            Health::UnknownProfile => {
                // flow::run() already notifies + logs the "unknown profile"
                // critical error; re-running it here would just spam that on
                // every tick, so only surface it once per transition.
                if transition || profile_changed {
                    let _ = flow::run(&mut cfg, &profile);
                }
                fail_streak = fail_streak.saturating_add(1);
            }
            Health::DownTailscaleManual => {
                // Can't be auto-fixed (login / not installed). Notify once.
                if transition {
                    notify(
                        "Tailscale Error",
                        "Tailscale needs manual attention (login / install). \
                         Other Wi-Fi automation paused until resolved.",
                        Urgency::Critical,
                    );
                }
                // Re-run flow only on transition so we land on the bootstrap net.
                if transition || profile_changed {
                    let _ = flow::run(&mut cfg, &profile);
                }
                fail_streak = fail_streak.saturating_add(1);
            }
            Health::DownNoNet | Health::DownTailscaleOther => {
                if transition {
                    notify(
                        "breadcrumbs: connection lost",
                        &format!("Recovering ({profile})…"),
                        Urgency::Normal,
                    );
                }
                let elapsed = last_flow_at
                    .map(|t| t.elapsed().as_secs())
                    .unwrap_or(u64::MAX);
                if elapsed >= FLOW_COOLDOWN {
                    log(&format!(
                        "watch: down ({:?}) profile={profile} ssid={:?} — running flow",
                        health, ssid
                    ));
                    let outcome = flow::run(&mut cfg, &profile);
                    log(&format!("watch: recovery outcome = {:?}", outcome));
                    last_flow_at = Some(Instant::now());
                    fail_streak = if outcome.ok() {
                        0
                    } else {
                        fail_streak.saturating_add(1)
                    };
                } else {
                    log(&format!(
                        "watch: down ({:?}) — cooldown ({elapsed}s/{FLOW_COOLDOWN}s), skipping flow",
                        health
                    ));
                }
            }
        }

        prev_health = Some(health);

        // Adaptive backoff: healthy -> base; failing -> grow up to ~6x.
        let mult = 1 + fail_streak.min(5);
        let dur = Duration::from_secs(base * mult as u64);
        wait_for_tick(&rx, dur);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debounce_fires_immediately_when_never_fired() {
        // Regression guard for the old `Instant::now() - Duration::from_secs(10)`
        // seed, which panicked near boot. `None` must fire without any
        // subtraction on the clock.
        assert!(debounce_ready(None, Duration::from_millis(1500)));
    }

    #[test]
    fn debounce_suppresses_immediately_after_firing() {
        let just_now = Instant::now();
        assert!(!debounce_ready(Some(just_now), Duration::from_secs(3600)));
    }

    #[test]
    fn debounce_fires_again_after_gap_elapses() {
        // A zero gap is always already-elapsed, so a prior fire doesn't block.
        let earlier = Instant::now();
        assert!(debounce_ready(Some(earlier), Duration::from_millis(0)));
    }

    #[test]
    fn health_as_str_is_the_variant_name() {
        assert_eq!(Health::Up.as_str(), "Up");
        assert_eq!(Health::DownNoNet.as_str(), "DownNoNet");
        assert_eq!(Health::DownTailscaleManual.as_str(), "DownTailscaleManual");
        assert_eq!(Health::DownTailscaleOther.as_str(), "DownTailscaleOther");
        assert_eq!(Health::NoAdapter.as_str(), "NoAdapter");
        assert_eq!(Health::UnknownProfile.as_str(), "UnknownProfile");
    }
}
