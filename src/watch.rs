use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::flow;
use crate::notify::{log, notify, Urgency};
use crate::state::State;
use crate::status::{self};
use crate::tailscale::TsHealth;

#[derive(PartialEq, Eq, Clone, Debug)]
enum Health {
    Up,
    DownNoNet,
    DownTailscaleManual,
    DownTailscaleOther,
    NoAdapter,
}

fn classify(cfg: &Config, profile: &str) -> (Health, Option<String>) {
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
            let mut last = Instant::now() - Duration::from_secs(10);
            for line in reader.lines().map_while(Result::ok) {
                let l = line.to_lowercase();
                let interesting = l.contains("disconnect")
                    || l.contains("unavailable")
                    || l.contains("failed");
                if interesting && last.elapsed() > Duration::from_millis(1500) {
                    last = Instant::now();
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
    spawn_nm_monitor(tx);

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
            let _ = flow::run(&cfg, &profile);
        }
    }

    let mut prev_health: Option<Health> = None;
    let mut prev_profile = profile.clone();
    let mut fail_streak: u32 = 0;
    let mut last_flow_at: Option<Instant> = None;
    const FLOW_COOLDOWN: u64 = 20;

    loop {
        // Reload config + state so edits and `profile set` take effect live.
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
            prev_profile = profile.clone();
            prev_health = None; // force re-evaluation/recovery for new profile
            last_flow_at = None; // allow immediate recovery on profile change
        }

        let (health, ssid) = classify(&cfg, &profile);
        let transition = prev_health.as_ref() != Some(&health);

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
                    let _ = flow::run(&cfg, &profile);
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
                let elapsed = last_flow_at.map(|t| t.elapsed().as_secs()).unwrap_or(u64::MAX);
                if elapsed >= FLOW_COOLDOWN {
                    log(&format!(
                        "watch: down ({:?}) profile={profile} ssid={:?} — running flow",
                        health, ssid
                    ));
                    let outcome = flow::run(&cfg, &profile);
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
