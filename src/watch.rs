use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use crate::backend::{Backend, System};
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
    /// Associated, but a captive portal is intercepting traffic (manual login).
    CaptivePortal,
    DownTailscaleManual,
    DownTailscaleOther,
    NoAdapter,
}

fn classify(be: &dyn Backend, cfg: &Config, profile: &str) -> (Health, status::Status) {
    let s = status::gather(be, cfg, profile);
    let health = if s.iface.is_none() {
        Health::NoAdapter
    } else if s.portal.is_some() {
        Health::CaptivePortal
    } else if !s.internet {
        Health::DownNoNet
    } else if s.tailscale_required {
        match s.tailscale {
            Some(TsHealth::Ok) => Health::Up,
            Some(TsHealth::NeedsLogin) | Some(TsHealth::NotInstalled) => {
                Health::DownTailscaleManual
            }
            Some(_) => Health::DownTailscaleOther,
            None => Health::DownTailscaleManual,
        }
    } else {
        Health::Up
    };
    (health, s)
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
                let interesting =
                    l.contains("disconnect") || l.contains("unavailable") || l.contains("failed");
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

    let be = System;
    let mut profile = State::load(&cfg.settings.default_profile).profile;
    if run_initial {
        // Don't churn an already-working connection on (re)start.
        let (h, _) = classify(&be, &cfg, &profile);
        if h == Health::Up {
            log(&format!(
                "watch: already healthy on start (profile={profile}); skipping initial flow"
            ));
        } else {
            log(&format!("watch: initial flow for profile={profile}"));
            let _ = flow::run(&be, &cfg, &profile);
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

        let (health, s) = classify(&be, &cfg, &profile);
        let ssid = s.ssid.clone();
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
            Health::CaptivePortal => {
                // Associated but gated behind a sign-in page we can't automate;
                // notify once and don't hammer flow (reconnecting won't help).
                if transition {
                    let body = match s.portal.as_deref().filter(|u| !u.is_empty()) {
                        Some(url) => format!("Sign in to continue: {url}"),
                        None => format!("Sign in to continue ({profile})."),
                    };
                    notify("breadcrumbs: captive portal", &body, Urgency::Normal);
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
                    let _ = flow::run(&be, &cfg, &profile);
                    last_flow_at = Some(Instant::now());
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
                    let outcome = flow::run(&be, &cfg, &profile);
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
