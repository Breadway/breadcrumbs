use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use bread_utils::bread_client::BreadClient;
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

use crate::bread_events;
use crate::config::Config;
use crate::flow;
use crate::nm;
use crate::notify::{log, notify, Urgency};
use crate::state::{self, State};
use crate::status::{self};
use crate::tailscale::TsHealth;
use crate::util::boot_secs;

const NM_DEST: &str = "org.freedesktop.NetworkManager";
const NM_PATH: &str = "/org/freedesktop/NetworkManager";
const NM_IFACE: &str = "org.freedesktop.NetworkManager";
const DEV_IFACE: &str = "org.freedesktop.NetworkManager.Device";
const PROPS_IFACE: &str = "org.freedesktop.DBus.Properties";

/// Coarse health classification the watch loop reacts to each tick. `pub`
/// (and so is [`classify`]) purely so integration tests can drive the real
/// classification logic in-process against a faked [`crate::util::Runner`],
/// instead of only being able to observe it indirectly through the watch
/// loop's side effects.
#[derive(PartialEq, Eq, Clone, Debug)]
pub enum Health {
    Up,
    DownNoNet,
    /// Traffic is being intercepted: a captive/guest portal answered the
    /// connectivity check with 200/301/302 instead of 204. Not something
    /// reconnecting fixes; the user must sign in.
    CaptivePortal,
    DownTailscaleManual,
    DownTailscaleOther,
    NoAdapter,
    /// `profile` isn't defined in the config (e.g. state still points at a
    /// custom profile the user deleted from breadcrumbs.toml).
    UnknownProfile,
}

impl Health {
    /// Wire name used in `bread.crumbs.health.changed`: the Rust variant
    /// as a string, not a prettier label.
    pub fn as_str(&self) -> &'static str {
        match self {
            Health::Up => "Up",
            Health::DownNoNet => "DownNoNet",
            Health::CaptivePortal => "CaptivePortal",
            Health::DownTailscaleManual => "DownTailscaleManual",
            Health::DownTailscaleOther => "DownTailscaleOther",
            Health::NoAdapter => "NoAdapter",
            Health::UnknownProfile => "UnknownProfile",
        }
    }
}

/// Everything the watch loop needs to know about one health observation:
/// the classification plus the context used for events and notifications.
#[derive(Debug, Clone)]
pub struct Classification {
    pub health: Health,
    pub ssid: Option<String>,
    pub iface: Option<String>,
    pub ip: Option<String>,
    pub tailscale: Option<TsHealth>,
    pub exit_node: String,
}

pub fn classify(cfg: &Config, profile: &str) -> Classification {
    // Checked before gather(): a profile missing from config would otherwise
    // silently fall back to "tailscale not required" and read as healthy off
    // of nothing but a bare internet check, never surfacing the misconfig.
    if cfg.profile(profile).is_none() {
        return Classification {
            health: Health::UnknownProfile,
            ssid: None,
            iface: None,
            ip: None,
            tailscale: None,
            exit_node: String::new(),
        };
    }
    let s = status::gather(cfg, profile);
    if s.iface.is_none() {
        return Classification {
            health: Health::NoAdapter,
            ssid: None,
            iface: None,
            ip: None,
            tailscale: None,
            exit_node: s.exit_node,
        };
    }
    let ssid = s.ssid.clone();
    let health = if !s.internet {
        if s.portal {
            Health::CaptivePortal
        } else {
            Health::DownNoNet
        }
    } else if s.tailscale_required {
        match s.tailscale {
            Some(TsHealth::Ok) => Health::Up,
            // NeedsLogin / NotInstalled / NoExitNode all need human action:
            // a missing exit-node config can't be auto-fixed either.
            Some(TsHealth::NeedsLogin)
            | Some(TsHealth::NotInstalled)
            | Some(TsHealth::NoExitNode) => Health::DownTailscaleManual,
            Some(_) => Health::DownTailscaleOther,
            None => Health::DownTailscaleManual,
        }
    } else {
        Health::Up
    };
    Classification {
        health,
        ssid,
        iface: s.iface,
        ip: s.ip,
        tailscale: s.tailscale,
        exit_node: s.exit_node,
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

/// Whether the flow-recovery cooldown has elapsed since the last `flow::run`
/// (or one never ran). Times are [`boot_secs`] readings, so time spent
/// suspended counts toward the cooldown. Pure so the recovery pacing is
/// unit-testable.
fn recovery_due(last_flow_at: Option<f64>, now: f64, cooldown_secs: f64) -> bool {
    last_flow_at.is_none_or(|t| now - t >= cooldown_secs)
}

/// Whether a no-internet observation should trigger recovery now. A single
/// failed probe while the link is associated is not enough: probes fail
/// transiently (a slow DNS answer, a dropped ICMP), and recovering an
/// associated link can mean reconnecting it. Require `DOWN_CONFIRMATIONS`
/// consecutive failures first. With no association at all there is
/// nothing to lose, so recover at once.
fn down_confirmed(associated: bool, consecutive_down: u32) -> bool {
    !associated || consecutive_down >= DOWN_CONFIRMATIONS
}

const DOWN_CONFIRMATIONS: u32 = 2;
/// How soon to re-probe after a first unconfirmed failure.
const CONFIRM_RECHECK: Duration = Duration::from_secs(4);

/// A wake signal for the watch loop. `SetProfile` is an *action* (applied on
/// the loop thread), `LinkChurn` is just "go look"; the distinction keeps
/// every config/state file access on the single loop thread, so the bread
/// subscription thread can never race the loop's own `Config::load`/`save`.
enum Wake {
    LinkChurn,
    SetProfile(String),
}

/// Whether a `PropertiesChanged` message on the NM root changes the
/// `Connectivity` property of the NetworkManager interface: the signal
/// that catches "still connected but lost the internet" (portal, DHCP
/// failure) without waiting out the poll interval.
fn props_changed_connectivity(msg: &zbus::Message) -> bool {
    let Ok(body) = msg
        .body()
        .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
    else {
        return false;
    };
    body.0 == NM_IFACE && body.1.contains_key("Connectivity")
}

/// Subscribe to a D-Bus signal from NetworkManager and ping the channel for
/// each matching message, reconnecting on bus/NM restarts. One thread per
/// subscription (a handful at most); each owns its own connection so a dead
/// bus can't wedge the others.
fn spawn_signal_watcher<F>(
    tx: mpsc::Sender<Wake>,
    path: String,
    iface: &'static str,
    signal: &'static str,
    mut on_msg: F,
) where
    F: FnMut(&zbus::Message) -> bool + Send + 'static,
{
    thread::spawn(move || loop {
        let Ok(conn) = Connection::system() else {
            thread::sleep(Duration::from_secs(10));
            continue;
        };
        let Ok(proxy) = Proxy::new(&conn, NM_DEST, path.as_str(), iface) else {
            thread::sleep(Duration::from_secs(10));
            continue;
        };
        let Ok(mut iter) = proxy.receive_signal(signal) else {
            thread::sleep(Duration::from_secs(10));
            continue;
        };
        // `None` means "haven't fired yet, so fire on the first interesting
        // signal". Storing an `Option` instead of seeding with
        // `Instant::now() - 10s` avoids a panic: `Instant - Duration`
        // underflows (and panics) when the monotonic clock is younger than
        // the offset, which happens if `watch` starts within ~10s of boot,
        // exactly when the systemd unit (ordered after graphical-session)
        // tends to launch.
        let mut last: Option<Instant> = None;
        for msg in iter.by_ref() {
            if on_msg(&msg) && debounce_ready(last, Duration::from_millis(1500)) {
                last = Some(Instant::now());
                let _ = tx.send(Wake::LinkChurn);
            }
        }
        // Subscription died (NM or bus restart): back off and resubscribe.
        thread::sleep(Duration::from_secs(5));
    });
}

/// Subscribe to NetworkManager D-Bus signals and ping the channel on
/// link-state churn so we react to drops within a second instead of waiting
/// out the poll interval. Replaces the old `nmcli monitor` subprocess: the
/// same events are observed, but as structured D-Bus signals.
///
/// Watched signals:
/// - `PropertiesChanged` on the NM root object, filtered to the
///   `Connectivity` property: catches captive portals / DHCP failures that
///   keep the device "connected" while losing the internet;
/// - `DeviceAdded` / `DeviceRemoved`: hotplug;
/// - `Device.StateChanged` on every Wi-Fi device: drops and reconnects.
fn spawn_nm_monitor(tx: mpsc::Sender<Wake>) {
    spawn_signal_watcher(
        tx.clone(),
        NM_PATH.to_string(),
        PROPS_IFACE,
        "PropertiesChanged",
        props_changed_connectivity,
    );
    spawn_signal_watcher(
        tx.clone(),
        NM_PATH.to_string(),
        NM_IFACE,
        "DeviceRemoved",
        |_| true,
    );
    // StateChanged on each Wi-Fi device, including ones that appear later
    // (USB dongle hotplug, or a driver reload that re-creates the device
    // under a new object path): the DeviceAdded handler subscribes those.
    let watched: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    for path in nm::wifi_device_paths() {
        watch_device(&tx, &watched, path);
    }
    let (tx2, watched2) = (tx.clone(), watched);
    spawn_signal_watcher(
        tx,
        NM_PATH.to_string(),
        NM_IFACE,
        "DeviceAdded",
        move |msg| {
            if let Ok(path) = msg.body().deserialize::<OwnedObjectPath>() {
                if nm::is_wifi_device(path.as_str()) {
                    watch_device(&tx2, &watched2, path.as_str().to_string());
                }
            }
            true
        },
    );
}

/// Subscribe to `StateChanged` on one Wi-Fi device, once per path.
fn watch_device(tx: &mpsc::Sender<Wake>, watched: &Mutex<HashSet<String>>, path: String) {
    let fresh = watched
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(path.clone());
    if fresh {
        spawn_signal_watcher(tx.clone(), path, DEV_IFACE, "StateChanged", |_| true);
    }
}

/// Sleep up to `dur`, but wake early if the D-Bus signal monitor signals
/// link churn or a `set_profile` command arrives. Returns the pending
/// action, if any.
fn wait_for_tick(rx: &Receiver<Wake>, dur: Duration) -> Option<Wake> {
    match rx.recv_timeout(dur) {
        Ok(first) => {
            // Drain any burst of churn signals so we don't re-fire
            // immediately. Preserve the newest queued set_profile action:
            // when commands arrive in one poll burst, the last request is
            // the user's current intent.
            let mut pending = match &first {
                Wake::SetProfile(_) => Some(first),
                Wake::LinkChurn => None,
            };
            while let Ok(w) = rx.try_recv() {
                if matches!(&w, Wake::SetProfile(_)) {
                    pending = Some(w);
                }
            }
            pending
        }
        Err(mpsc::RecvTimeoutError::Timeout) => None,
        // Monitor thread gone (shouldn't happen: we hold the sender); fall
        // back to a plain sleep so we don't busy-spin.
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            thread::sleep(dur);
            None
        }
    }
}

pub fn run(mut cfg: Config, run_initial: bool) -> i32 {
    let base = cfg.settings.poll_interval();
    notify(
        "breadcrumbs watcher started",
        "Monitoring Wi-Fi; will auto-recover drops.",
        Urgency::Low,
    );
    log("watch: started");

    let (tx, rx) = mpsc::channel::<Wake>();
    spawn_nm_monitor(tx.clone());

    // Long-lived, so this uses BreadClient::subscribe (a persistent
    // background thread with its own reconnect/backoff loop). breadd being
    // absent or restarting is transparent: the subscription just quietly
    // stops delivering commands until it reconnects. The callback only
    // *validates* the command and forwards an action through the channel;
    // it never touches config/state files itself (that would race this
    // loop's own Config::load/save), so all file access stays on this one
    // thread.
    let bread = BreadClient::connect(bread_events::APP_ID);
    let wake = tx;
    let _commands =
        bread.subscribe(
            "bread.command.crumbs.**",
            move |event| match bread_events::handle_command(&event) {
                bread_events::CommandAction::SetProfile(name) => {
                    let _ = wake.send(Wake::SetProfile(name));
                }
                bread_events::CommandAction::Ignore => {}
            },
        );

    let mut profile = State::load(&cfg.settings.default_profile).profile;
    if run_initial {
        // Don't churn an already-working connection on (re)start.
        let class = classify(&cfg, &profile);
        if class.health == Health::Up {
            log(&format!(
                "watch: already healthy on start (profile={profile}); skipping initial flow"
            ));
        } else {
            log(&format!("watch: initial flow for profile={profile}"));
            let _ = flow::run_quiet(&mut cfg, &profile);
        }
    }

    let mut prev_health: Option<Health> = None;
    let mut prev_profile = profile.clone();
    let mut prev_ssid: Option<String> = None;
    let mut prev_ts: Option<&'static str> = None;
    let mut fail_streak: u32 = 0;
    let mut consecutive_down: u32 = 0;
    // Whether "connection lost" has been shown for the current outage, so
    // the notification is sent once, and only once the outage is
    // confirmed.
    let mut notified_down = false;
    let mut last_flow_at: Option<f64> = None;
    const FLOW_COOLDOWN: f64 = 20.0;
    const RESUME_SLACK: f64 = 60.0;
    const SCHEDULE_GRACE: f64 = 30.0 * 60.0;
    let mut prev_wait = Duration::from_secs(base);
    // Boot clock, not `Instant`: `Instant` stops while suspended, so a
    // monotonic gap check can never see a suspend/resume.
    let mut last_tick_at = boot_secs();
    let mut warned_schedule: HashSet<String> = HashSet::new();
    // Tracks what the *schedule* last applied, so the loop can tell a
    // manual `profile set` (CLI or bus) apart from its own switch and give
    // manual changes a grace window before the schedule overrides them.
    let mut last_schedule_applied: Option<String> = Some(profile.clone());
    let mut manual_set_at: Option<f64> = None;

    loop {
        // No lock is held across a tick: reads need none (writes are
        // atomic), and the few writes a tick can make (clearing a used
        // password, learning an SSID, a schedule switch) each take the lock
        // for just their own load, modify, save. Holding it across a
        // recovery flow used to stall every other breadcrumbs command,
        // including breadbar's `status` polls, for the whole flow.
        // Reload config + state so edits and `profile set` take effect live.
        // This always runs *before* `flow::run` below (never after, within
        // the same tick), so a password `flow::run` clears-and-saves this
        // iteration is durably on disk by the time the *next* iteration's
        // reload runs. All config/state file access happens on this loop
        // thread: `set_profile` commands from the bread bus are queued as
        // [`Wake::SetProfile`] and applied here (see the bottom of the
        // loop), never on the subscription thread.
        if let Ok(fresh) = Config::load() {
            cfg = fresh;
        }
        profile = State::load(&cfg.settings.default_profile).profile;

        // Suspend/resume: the D-Bus signal monitor sees nothing while the
        // machine sleeps, so a large wall-clock gap means the network state
        // may have changed underneath us; allow an immediate recovery run
        // instead of waiting out any remaining flow cooldown.
        if boot_secs() - last_tick_at > prev_wait.as_secs_f64() + RESUME_SLACK {
            log("watch: large gap since last tick (suspend/resume?), forcing recovery check");
            last_flow_at = None;
        }

        // Time-of-day schedule: switch to the scheduled profile when its
        // window is active, unless the user manually set the profile within
        // the grace window.
        if last_schedule_applied.as_deref() != Some(profile.as_str()) {
            // The persisted profile changed and it wasn't our own schedule
            // switch: a manual set (CLI or bus). Start the grace window.
            if last_schedule_applied.is_some() {
                manual_set_at = Some(boot_secs());
            }
            last_schedule_applied = Some(profile.clone());
        }
        if let Some(sched) = scheduled_profile_now(&cfg) {
            if !cfg.profiles.contains_key(&sched) {
                if warned_schedule.insert(sched.clone()) {
                    log(&format!(
                        "watch: schedule targets unknown profile '{sched}', ignoring it"
                    ));
                }
            } else if sched != profile {
                let grace_ok = manual_set_at
                    .map(|t| boot_secs() - t >= SCHEDULE_GRACE)
                    .unwrap_or(true);
                if grace_ok {
                    if state::set_profile(&cfg, &sched).is_ok() {
                        log(&format!("watch: schedule applied profile {sched}"));
                        last_schedule_applied = Some(sched.clone());
                        manual_set_at = None;
                    }
                } else {
                    log(&format!(
                        "watch: schedule would switch to {sched}, but a manual set is still in grace"
                    ));
                }
            }
        }

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
            prev_ssid = None; // a profile switch is a fresh network context
            prev_ts = None;
            last_flow_at = None; // allow immediate recovery on profile change
        }

        let class = classify(&cfg, &profile);
        let health = class.health.clone();
        let ssid = class.ssid.clone();
        let transition = prev_health.as_ref() != Some(&health);
        if transition {
            bread_events::emit_health_changed(
                &bread,
                bread_events::HealthChanged {
                    profile: &profile,
                    health: health.as_str(),
                    ssid: ssid.as_deref(),
                    iface: class.iface.as_deref(),
                    ip: class.ip.as_deref(),
                    exit_node: &class.exit_node,
                    tailscale: class.tailscale.as_ref().map(|t| t.state_str()),
                },
            );
        }
        if class.ssid != prev_ssid {
            bread_events::emit_network_changed(
                &bread,
                prev_ssid.as_deref(),
                class.ssid.as_deref(),
                &profile,
            );
            prev_ssid = class.ssid.clone();
        }
        let ts_state = class.tailscale.as_ref().map(|t| t.state_str());
        if ts_state != prev_ts {
            bread_events::emit_tailscale_changed(&bread, &profile, ts_state, &class.exit_node);
            prev_ts = ts_state;
        }

        if health == Health::DownNoNet {
            consecutive_down = consecutive_down.saturating_add(1);
        } else {
            consecutive_down = 0;
        }
        let awaiting_confirmation =
            health == Health::DownNoNet && !down_confirmed(ssid.is_some(), consecutive_down);

        match &health {
            Health::Up => {
                // Only after a "connection lost" was actually shown: a
                // single unconfirmed failed probe is not worth two popups.
                if transition && prev_health.is_some() && notified_down {
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
                notified_down = false;
            }
            Health::NoAdapter => {
                if transition {
                    notify(
                        "breadcrumbs: no Wi-Fi adapter",
                        "Hardware issue, manual check needed.",
                        Urgency::Critical,
                    );
                }
                fail_streak = fail_streak.saturating_add(1);
            }
            Health::UnknownProfile => {
                // flow::run() is quiet from here, so surface the misconfig
                // ourselves, once per transition/change, not every tick.
                if transition || profile_changed {
                    notify(
                        "breadcrumbs: unknown profile",
                        &format!("'{profile}' is not defined in breadcrumbs.toml"),
                        Urgency::Critical,
                    );
                }
                fail_streak = fail_streak.saturating_add(1);
            }
            Health::CaptivePortal => {
                if transition {
                    notify(
                        "breadcrumbs: captive portal detected",
                        "Traffic is being intercepted, open a browser and sign in.",
                        Urgency::Normal,
                    );
                }
                // Reconnecting won't fix a portal; keep the poll fast (don't
                // count it as a failure) so a successful sign-in is noticed
                // promptly and the state flips back to Up.
                fail_streak = 0;
            }
            Health::DownTailscaleManual => {
                // Can't be auto-fixed (login / install / exit-node config).
                // Notify once per transition.
                if transition {
                    notify(
                        "Tailscale Error",
                        "Tailscale needs manual attention (login / install / \
                         exit node config). Other Wi-Fi automation paused \
                         until resolved.",
                        Urgency::Critical,
                    );
                }
                // Re-attempt periodically and on the transition into this
                // state: login may have completed since the last attempt, or
                // the user may have missed the browser window. Quiet: a
                // still-broken state must not re-notify on every retry. Connecting is idempotent, so
                // this no longer bounces the Wi-Fi link while waiting.
                if recovery_due(last_flow_at, boot_secs(), FLOW_COOLDOWN) {
                    let outcome = flow::run_quiet(&mut cfg, &profile);
                    last_flow_at = Some(boot_secs());
                    fail_streak = if outcome.ok() {
                        0
                    } else {
                        fail_streak.saturating_add(1)
                    };
                }
            }
            Health::DownNoNet | Health::DownTailscaleOther => {
                if !awaiting_confirmation && !notified_down {
                    notify(
                        "breadcrumbs: connection lost",
                        &format!("Recovering ({profile})…"),
                        Urgency::Normal,
                    );
                    notified_down = true;
                }
                if awaiting_confirmation {
                    log(&format!(
                        "watch: down ({health:?}) on {ssid:?}, re-checking before recovering"
                    ));
                } else if recovery_due(last_flow_at, boot_secs(), FLOW_COOLDOWN) {
                    log(&format!(
                        "watch: down ({:?}) profile={profile} ssid={:?}, running flow",
                        health, ssid
                    ));
                    let outcome = flow::run_quiet(&mut cfg, &profile);
                    log(&format!("watch: recovery outcome = {:?}", outcome));
                    last_flow_at = Some(boot_secs());
                    fail_streak = if outcome.ok() {
                        0
                    } else {
                        fail_streak.saturating_add(1)
                    };
                } else {
                    log(&format!(
                        "watch: down ({:?}), cooldown, skipping flow",
                        health
                    ));
                }
            }
        }

        prev_health = Some(health);

        // Adaptive backoff: healthy -> base; failing -> grow up to ~6x. A
        // first unconfirmed failure re-checks quickly instead.
        let mult = 1 + fail_streak.min(5);
        let dur = if awaiting_confirmation {
            CONFIRM_RECHECK
        } else {
            Duration::from_secs(base * u64::from(mult))
        };
        prev_wait = dur;
        last_tick_at = boot_secs();
        // Apply a queued set_profile on this thread (the single owner of
        // config/state file access) and emit the confirmation. The next
        // iteration's reload sees the new profile and recovers accordingly.
        if let Some(Wake::SetProfile(name)) = wait_for_tick(&rx, dur) {
            bread_events::apply_set_profile(&name);
        }
    }
}

/// The profile a time-of-day schedule wants right now, if any. Returns
/// `None` when no schedule is configured or the local time can't be read.
fn scheduled_profile_now(cfg: &Config) -> Option<String> {
    let hhmm = crate::util::local_hhmm()?;
    let mins = crate::config::hhmm_to_minutes(&hhmm)?;
    cfg.settings.scheduled_profile(mins)
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

    #[test]
    fn wait_for_tick_returns_pending_set_profile_and_drains_churn() {
        // A queued set_profile is an action, not a signal: it must survive
        // the churn-burst drain and be returned to the loop.
        let (tx, rx) = mpsc::channel::<Wake>();
        let _ = tx.send(Wake::LinkChurn);
        let _ = tx.send(Wake::SetProfile("home".into()));
        let _ = tx.send(Wake::LinkChurn);

        let wake = wait_for_tick(&rx, Duration::from_millis(10));
        assert!(matches!(wake, Some(Wake::SetProfile(n)) if n == "home"));
        // The burst was fully drained.
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn wait_for_tick_keeps_latest_set_profile_from_burst() {
        let (tx, rx) = mpsc::channel::<Wake>();
        let _ = tx.send(Wake::SetProfile("home".into()));
        let _ = tx.send(Wake::LinkChurn);
        let _ = tx.send(Wake::SetProfile("work".into()));
        let _ = tx.send(Wake::LinkChurn);

        let wake = wait_for_tick(&rx, Duration::from_millis(10));
        assert!(matches!(wake, Some(Wake::SetProfile(name)) if name == "work"));
        assert!(rx.try_recv().is_err(), "the entire burst should be drained");
    }

    #[test]
    fn wait_for_tick_drains_churn_burst_without_action() {
        // A burst of monitor signals collapses to one wake with no action.
        let (tx, rx) = mpsc::channel::<Wake>();
        let _ = tx.send(Wake::LinkChurn);
        let _ = tx.send(Wake::LinkChurn);
        let _ = tx.send(Wake::LinkChurn);

        assert!(wait_for_tick(&rx, Duration::from_millis(10)).is_none());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn wait_for_tick_times_out_with_no_signal() {
        let (_tx, rx) = mpsc::channel::<Wake>();
        assert!(wait_for_tick(&rx, Duration::from_millis(10)).is_none());
    }

    #[test]
    fn recovery_due_fires_when_never_run_and_after_cooldown() {
        // Never run → due immediately.
        assert!(recovery_due(None, 100.0, 20.0));
        // Just ran → not due again yet.
        assert!(!recovery_due(Some(100.0), 100.0, 20.0));
        assert!(!recovery_due(Some(100.0), 119.9, 20.0));
        assert!(recovery_due(Some(100.0), 120.0, 20.0));
        // A zero cooldown is always already-elapsed.
        assert!(recovery_due(Some(100.0), 100.0, 0.0));
    }

    #[test]
    fn down_confirmed_needs_two_strikes_only_when_associated() {
        // Associated: one failed probe is not enough to reconnect.
        assert!(!down_confirmed(true, 1));
        assert!(down_confirmed(true, 2));
        // Not associated: nothing to lose, recover at once.
        assert!(down_confirmed(false, 1));
    }
}
