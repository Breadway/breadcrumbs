//! CLI argument parsing and command handlers. This is the only module
//! `src/main.rs` calls into; everything else (the actual state machine,
//! nmcli/tailscale wrappers, config, …) is exercised directly by library
//! consumers (including the integration tests under `tests/`).

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use clap::{Parser, Subcommand};

use crate::config::{Config, NetworkDef};
use crate::state::{self, State};
use crate::util::{self, command_exists, home_dir};
use crate::{config, flow, nm, watch};

const C_RESET: &str = "\x1b[0m";
const C_BOLD: &str = "\x1b[1m";
const C_GREEN: &str = "\x1b[32m";
const C_RED: &str = "\x1b[31m";
const C_YELLOW: &str = "\x1b[33m";
const C_DIM: &str = "\x1b[2m";

#[derive(Parser)]
#[command(
    name = "breadcrumbs",
    version,
    about = "Profile-aware Wi-Fi state machine with Tailscale handling",
    disable_help_subcommand = true
)]
struct Cli {
    /// Override the active profile for this run only (does not persist)
    #[arg(long, short, global = true)]
    profile: Option<String>,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

/// Optional flags for `add`. Flattened into the `Add` subcommand so the
/// CLI surface is unchanged while keeping `cmd_add`'s signature small.
#[derive(clap::Args)]
struct AddOpts {
    /// Password (prompted if omitted)
    password: Option<String>,
    /// Network is hidden (does not broadcast its SSID).
    /// `--hidden` sets it; `--hidden=false` clears it on an existing
    /// entry; omitted leaves an existing entry's flag untouched.
    #[arg(long, num_args = 0..=1, default_missing_value = "true")]
    hidden: Option<bool>,
    /// Per-network DNS override (empty string disables DNS pinning
    /// for this network)
    #[arg(long)]
    dns: Option<String>,
    /// 802.1x EAP method for enterprise networks (e.g. "peap", "tls")
    #[arg(long)]
    eap: Option<String>,
    /// 802.1x identity for enterprise networks
    #[arg(long)]
    identity: Option<String>,
    /// Path to a CA certificate for 802.1x
    #[arg(long)]
    ca_cert: Option<String>,
    /// Attach this SSID to a profile's priority list
    #[arg(long)]
    to: Option<String>,
    /// Position in the profile list (0 = highest priority)
    #[arg(long)]
    at: Option<usize>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show current Wi-Fi / profile / Tailscale status (default)
    Status {
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// Run the full connect sequence for the active profile
    #[command(visible_aliases = ["up", "connect", "i"])]
    Init {
        /// Retry until connected or this many seconds have elapsed
        /// (0 = single attempt)
        #[arg(long, default_value_t = 0)]
        wait: u64,
    },
    /// Run as a daemon: watch for drops and auto-recover
    Watch {
        /// Skip the connect attempt on startup
        #[arg(long)]
        no_initial: bool,
    },
    /// Get / set / list location profiles (the state machine)
    Profile {
        #[command(subcommand)]
        action: Option<ProfileCmd>,
    },
    /// Guess the profile from visible networks
    Detect {
        /// Set + apply the detected profile
        #[arg(long)]
        apply: bool,
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// Add or update a saved network
    Add {
        ssid: String,
        #[command(flatten)]
        opts: AddOpts,
    },
    /// Connect to an already-saved network by SSID, outside of any profile.
    /// Takes no password — the secret was already persisted by `add`, so
    /// this command never puts a secret on its own argv. For callers (e.g.
    /// breadbar's "join network" dialog) that just ran `add` and now need
    /// NetworkManager to actually activate the network.
    Join { ssid: String },
    /// Remove a saved network (config + NetworkManager)
    Forget { ssid: String },
    /// Remove NetworkManager wireless profiles whose SSID is no longer in
    /// the breadcrumbs config
    Prune {
        /// Only list what would be removed
        #[arg(long)]
        dry_run: bool,
    },
    /// Scan, pick, connect and save a network interactively
    Scan {
        /// Attach the saved network to this profile
        #[arg(long)]
        to: Option<String>,
    },
    /// List configured networks and profiles
    List {
        #[arg(long)]
        show_passwords: bool,
    },
    /// Open the config file in $EDITOR
    Edit,
    /// Quick connectivity / Tailscale diagnostics
    Doctor {
        /// Run the full diag.sh report from the config directory
        #[arg(long)]
        full: bool,
    },
    /// Print the breadcrumbs config directory
    Cd {
        #[arg(long)]
        shell: bool,
    },
    /// Install + enable the systemd user watcher service
    InstallService {
        /// Install the unit but do not enable/start it
        #[arg(long)]
        no_enable: bool,
    },
}

#[derive(Subcommand)]
enum ProfileCmd {
    /// Print the active profile
    Get,
    /// Set the active profile (and apply it unless --no-apply)
    Set {
        name: String,
        #[arg(long)]
        no_apply: bool,
    },
    /// List available profiles
    List,
}

/// Parse `argv` and run the requested command. Returns the process exit code.
pub fn run() -> i32 {
    let cli = Cli::parse();
    match real_main(cli) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{C_RED}error:{C_RESET} {e}");
            1
        }
    }
}

fn active_profile(cfg: &Config, override_p: &Option<String>) -> String {
    if let Some(p) = override_p {
        return p.clone();
    }
    State::load(&cfg.settings.default_profile).profile
}

fn real_main(cli: Cli) -> Result<i32, String> {
    let cmd = cli.cmd.unwrap_or(Cmd::Status { json: false });

    // `cd` and `install-service` don't need a parsed config first.
    if let Cmd::Cd { shell } = &cmd {
        return cmd_cd(*shell);
    }

    let mut cfg = Config::load()?;

    match cmd {
        Cmd::Status { json } => cmd_status(&cfg, &cli.profile, json),
        Cmd::Init { wait } => cmd_init(&mut cfg, &cli.profile, wait),
        Cmd::Watch { no_initial } => Ok(watch::run(cfg, !no_initial)),
        Cmd::Profile { action } => cmd_profile(&mut cfg, action),
        Cmd::Detect { apply, json } => cmd_detect(&mut cfg, apply, json),
        Cmd::Add { ssid, opts } => cmd_add(&mut cfg, ssid, opts),
        Cmd::Join { ssid } => cmd_join(&mut cfg, &ssid),
        Cmd::Forget { ssid } => cmd_forget(&mut cfg, &ssid),
        Cmd::Prune { dry_run } => cmd_prune(&cfg, dry_run),
        Cmd::Scan { to } => cmd_scan(&mut cfg, to),
        Cmd::List { show_passwords } => cmd_list(&cfg, show_passwords),
        Cmd::Edit => cmd_edit(),
        Cmd::Doctor { full } => cmd_doctor(&cfg, &cli.profile, full),
        Cmd::InstallService { no_enable } => cmd_install_service(!no_enable),
        Cmd::Cd { .. } => unreachable!(),
    }
}

fn cmd_init(cfg: &mut Config, override_p: &Option<String>, wait: u64) -> Result<i32, String> {
    let p = active_profile(cfg, override_p);
    let deadline = std::time::Instant::now() + Duration::from_secs(wait);
    let mut attempt = 0;
    loop {
        // First attempt notifies normally (user-initiated); retries are
        // quiet so a long --wait run doesn't spam notifications.
        let outcome = if attempt == 0 {
            flow::run(cfg, &p)
        } else {
            flow::run_quiet(cfg, &p)
        };
        if outcome.ok() {
            print_outcome(&p, &outcome);
            return Ok(0);
        }
        if wait == 0 || std::time::Instant::now() >= deadline {
            print_outcome(&p, &outcome);
            return Ok(1);
        }
        attempt += 1;
        println!("{C_DIM}not connected yet — retrying in 3s…{C_RESET}");
        std::thread::sleep(Duration::from_secs(3));
    }
}

fn print_outcome(profile: &str, o: &flow::Outcome) {
    match o {
        flow::Outcome::Connected { ssid, note } => {
            print!("{C_GREEN}connected{C_RESET} {C_BOLD}{ssid}{C_RESET} ({profile})");
            match note {
                Some(n) => println!(" {C_YELLOW}— {n}{C_RESET}"),
                None => println!(),
            }
        }
        flow::Outcome::TailscaleError { ssid, health } => {
            println!(
                "{C_RED}tailscale error{C_RESET}: {} {C_DIM}(on {}){C_RESET}",
                health.describe(),
                ssid.clone().unwrap_or_else(|| "—".into())
            );
        }
        flow::Outcome::NoInterface => {
            println!("{C_RED}no Wi-Fi adapter{C_RESET} — hardware issue")
        }
        flow::Outcome::NoNetworks => {
            println!("{C_RED}no known networks in range{C_RESET} (profile {profile})")
        }
        flow::Outcome::UnknownProfile(p) => {
            println!("{C_RED}unknown profile{C_RESET}: {p}")
        }
    }
}

fn cmd_status(cfg: &Config, override_p: &Option<String>, json: bool) -> Result<i32, String> {
    let p = active_profile(cfg, override_p);
    let s = crate::status::gather(cfg, &p);

    let healthy = s.internet
        && s.iface.is_some()
        && (!s.tailscale_required || s.tailscale.as_ref().map(|h| h.is_ok()).unwrap_or(false));

    if json {
        let tailscale = s.tailscale.as_ref().map(|h| h.state_str());
        println!(
            "{}",
            serde_json::json!({
                "profile": p,
                "iface": s.iface,
                "ssid": s.ssid,
                "ip": s.ip,
                "internet": s.internet,
                "portal": s.portal,
                "tailscale_required": s.tailscale_required,
                "tailscale": tailscale,
                "exit_node": s.exit_node,
                "healthy": healthy,
            })
        );
        return Ok(if healthy { 0 } else { 1 });
    }

    let dot = |ok: bool| {
        if ok {
            format!("{C_GREEN}●{C_RESET}")
        } else {
            format!("{C_RED}●{C_RESET}")
        }
    };

    println!("{C_BOLD}breadcrumbs{C_RESET}");
    println!("  profile     {C_BOLD}{p}{C_RESET}");
    println!(
        "  adapter     {}",
        s.iface
            .clone()
            .unwrap_or_else(|| format!("{C_RED}none{C_RESET}"))
    );
    println!(
        "  ssid        {}",
        s.ssid
            .clone()
            .unwrap_or_else(|| format!("{C_DIM}—{C_RESET}"))
    );
    println!(
        "  ip          {}",
        s.ip.clone().unwrap_or_else(|| format!("{C_DIM}—{C_RESET}"))
    );
    println!(
        "  internet    {} {}",
        dot(s.internet),
        if s.internet { "ok" } else { "down" }
    );

    match (&s.tailscale, s.tailscale_required) {
        (Some(h), req) => {
            let ok = h.is_ok();
            println!(
                "  tailscale   {} {} {C_DIM}(exit: {}{}){C_RESET}",
                dot(ok || !req),
                h.describe(),
                s.exit_node,
                if req { "" } else { ", optional" }
            );
        }
        (None, _) => println!("  tailscale   {C_DIM}not installed{C_RESET}"),
    }

    println!(
        "  state       {}",
        if healthy {
            format!("{C_GREEN}healthy{C_RESET}")
        } else {
            format!("{C_YELLOW}needs attention{C_RESET} — run `breadcrumbs init`")
        }
    );
    Ok(if healthy { 0 } else { 1 })
}

fn cmd_profile(cfg: &mut Config, action: Option<ProfileCmd>) -> Result<i32, String> {
    match action.unwrap_or(ProfileCmd::Get) {
        ProfileCmd::Get => {
            println!("{}", State::load(&cfg.settings.default_profile).profile);
            Ok(0)
        }
        ProfileCmd::List => {
            let cur = State::load(&cfg.settings.default_profile).profile;
            for name in cfg.profiles.keys() {
                let mark = if *name == cur { "*" } else { " " };
                println!("{mark} {name}");
            }
            Ok(0)
        }
        ProfileCmd::Set { name, no_apply } => {
            state::set_profile(cfg, &name)?;
            println!("profile = {C_BOLD}{name}{C_RESET}");
            if no_apply {
                return Ok(0);
            }
            let outcome = flow::run(cfg, &name);
            print_outcome(&name, &outcome);
            Ok(if outcome.ok() { 0 } else { 1 })
        }
    }
}

fn detect_profile(cfg: &Config) -> Option<String> {
    let iface = nm::wifi_interface_preferred(cfg.settings.interface.as_deref())?;
    nm::radio_on();
    nm::rescan(&iface, &[]);
    let visible = nm::visible_signals(&iface);

    // Scored detection: the profile with the most matching markers wins, so
    // a 2-marker match beats a 1-marker one. Profiles are stored in a
    // BTreeMap, so ties resolve deterministically (alphabetically first).
    let mut best: Option<(String, usize)> = None;
    for (name, profile) in &cfg.profiles {
        if profile.detect_ssids.is_empty() {
            continue;
        }
        let count = profile
            .detect_ssids
            .iter()
            .filter(|s| visible.contains_key(s.as_str()))
            .count();
        if count > 0 {
            let better = match &best {
                None => true,
                Some((_, c)) => count > *c,
            };
            if better {
                best = Some((name.clone(), count));
            }
        }
    }

    best.map(|(p, _)| p).or_else(|| {
        // Fall back to the default profile if no markers matched — but only
        // if it actually exists: a stale `default_profile` name is a config
        // error, not a detection result, and persisting it would wedge the
        // watcher in UnknownProfile forever.
        if cfg.profiles.contains_key(&cfg.settings.default_profile) {
            Some(cfg.settings.default_profile.clone())
        } else {
            None
        }
    })
}

fn cmd_detect(cfg: &mut Config, apply: bool, json: bool) -> Result<i32, String> {
    match detect_profile(cfg) {
        Some(p) => {
            if json && !apply {
                println!("{}", serde_json::json!({ "profile": p }));
                return Ok(0);
            }
            if apply {
                if json {
                    println!("{}", serde_json::json!({ "profile": p }));
                } else {
                    println!("{p}");
                }
                // Route through state::set_profile (like the CLI and the
                // bread bus do) so an unknown fallback is rejected with a
                // proper error instead of being persisted as active.
                state::set_profile(cfg, &p)?;
                let outcome = flow::run(cfg, &p);
                print_outcome(&p, &outcome);
                return Ok(if outcome.ok() { 0 } else { 1 });
            }
            println!("{p}");
            Ok(0)
        }
        None => Err("could not detect a profile (no Wi-Fi adapter, or no \
                      profile matches and the default is misconfigured)"
            .into()),
    }
}

fn prompt_line(msg: &str) -> String {
    print!("{msg}");
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    let _ = std::io::stdin().lock().read_line(&mut s);
    s.trim_end_matches(['\n', '\r']).to_string()
}

/// An empty string entered for a password (CLI arg or a blank prompt
/// response) means "this network has no password" (open Wi-Fi) — normalize
/// it to `None` right at the point of entry so it flows the same way a
/// genuinely absent/cleared password does. Without this, `Some("")` would
/// make `nm::connect_verbose` send an empty PSK in the settings payload,
/// which NetworkManager treats as "secured with a blank password" rather
/// than "open", and the connect fails against a real open SSID.
fn non_empty(s: String) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn prompt_secret(msg: &str) -> String {
    // `util::run` redirects child stdin to /dev/null, so plain `stty -echo`
    // would target the wrong fd and silently leave echo ON (leaking the
    // password to the screen). `-F /dev/tty` makes stty act on the controlling
    // terminal directly. If there is no tty we fall back to visible input.
    let had_tty = util::run("stty", &["-F", "/dev/tty", "-echo"], Duration::from_secs(2)).success;
    let val = prompt_line(msg);
    if had_tty {
        let _ = util::run("stty", &["-F", "/dev/tty", "echo"], Duration::from_secs(2));
        println!();
    }
    val
}

fn cmd_add(cfg: &mut Config, ssid: String, opts: AddOpts) -> Result<i32, String> {
    let AddOpts {
        password,
        hidden,
        dns,
        eap,
        identity,
        ca_cert,
        to,
        at,
    } = opts;
    // `--dns ""` is the explicit "don't pin DNS" opt-out; normalize an
    // absent flag to None (use the global setting).
    let dns = match dns {
        Some(s) if s.is_empty() => Some(String::new()),
        Some(s) => Some(s),
        None => None,
    };
    // For enterprise networks, the password is the 802.1x password.
    let password = match password {
        Some(p) => p,
        None if eap.is_some() => prompt_secret(&format!("802.1x password for '{ssid}': ")),
        None => prompt_secret(&format!("Password for '{ssid}': ")),
    };
    let password = non_empty(password);
    match cfg.networks.iter_mut().find(|n| n.ssid == ssid) {
        Some(n) => {
            n.password = password;
            // `--hidden` / `--hidden=false` set the flag explicitly; when
            // the flag is omitted, leave an existing entry's hidden state
            // alone (a password-only update must not un-hide a network).
            if let Some(h) = hidden {
                n.hidden = h;
            }
            if dns.is_some() {
                n.dns = dns;
            }
            if eap.is_some() {
                n.eap = eap;
            }
            if identity.is_some() {
                n.identity = identity;
            }
            if ca_cert.is_some() {
                n.ca_cert = ca_cert;
            }
        }
        None => cfg.networks.push(NetworkDef {
            ssid: ssid.clone(),
            password,
            dns,
            eap,
            identity,
            ca_cert,
            hidden: hidden.unwrap_or(false),
        }),
    }
    if let Some(prof_name) = to {
        let prof = cfg
            .profiles
            .get_mut(&prof_name)
            .ok_or_else(|| format!("unknown profile '{prof_name}'"))?;
        prof.networks.retain(|s| s != &ssid);
        let idx = at.unwrap_or(prof.networks.len()).min(prof.networks.len());
        prof.networks.insert(idx, ssid.clone());
    }
    cfg.save()?;
    println!("{C_GREEN}saved{C_RESET} {ssid}");
    Ok(0)
}

/// Activate an already-saved network right now, independent of any profile.
/// Deliberately takes no password argument: `add` is the only command that
/// accepts a secret, and it never appears on this command's argv, so a
/// caller can safely shell this out without any secret-handling of its own.
fn cmd_join(cfg: &mut Config, ssid: &str) -> Result<i32, String> {
    let net = cfg
        .networks
        .iter()
        .find(|n| n.ssid == ssid)
        .cloned()
        .ok_or_else(|| format!("no saved network '{ssid}' — run `add` first"))?;
    let iface = nm::wifi_interface_preferred(cfg.settings.interface.as_deref())
        .ok_or_else(|| "no Wi-Fi adapter found".to_string())?;
    nm::radio_on();
    match flow::connect_and_verify(&iface, &net, cfg) {
        Ok(()) => {
            flow::clear_password_if_used(cfg, ssid);
            println!("{C_GREEN}connected{C_RESET} {ssid}");
            Ok(0)
        }
        Err(e) => {
            println!("{C_RED}failed{C_RESET}: {e}");
            Ok(1)
        }
    }
}

fn cmd_forget(cfg: &mut Config, ssid: &str) -> Result<i32, String> {
    let before = cfg.networks.len();
    cfg.networks.retain(|n| n.ssid != ssid);
    for p in cfg.profiles.values_mut() {
        p.networks.retain(|s| s != ssid);
        if p.bootstrap.as_deref() == Some(ssid) {
            p.bootstrap = None;
        }
    }
    cfg.save()?;
    let removed = nm::delete_connections_for_ssid(ssid);
    println!(
        "{C_GREEN}forgot{C_RESET} {ssid} (config: {}, NetworkManager: {})",
        if cfg.networks.len() < before {
            "removed"
        } else {
            "not present"
        },
        if removed { "removed" } else { "not present" }
    );
    Ok(0)
}

/// Remove NetworkManager wireless profiles whose SSID is no longer known to
/// breadcrumbs (config `networks`, or any profile's priority list or
/// bootstrap). `--dry-run` only lists. Returns the number removed.
fn cmd_prune(cfg: &Config, dry_run: bool) -> Result<i32, String> {
    let known: Vec<&str> = cfg
        .networks
        .iter()
        .map(|n| n.ssid.as_str())
        .chain(
            cfg.profiles
                .values()
                .flat_map(|p| p.networks.iter().map(|s| s.as_str()).chain(p.bootstrap.iter().map(|s| s.as_str()))),
        )
        .collect();
    let stale: Vec<(String, String)> = nm::wireless_profiles()
        .into_iter()
        .filter(|(_name, ssid)| !known.contains(&ssid.as_str()))
        .collect();
    if stale.is_empty() {
        println!("{C_GREEN}nothing to prune{C_RESET}");
        return Ok(0);
    }
    for (name, ssid) in &stale {
        if dry_run {
            println!("{C_DIM}would remove{C_RESET} {name} ({ssid})");
        } else {
            println!("{C_GREEN}removed{C_RESET} {name} ({ssid})");
            let _ = nm::delete_connections_for_ssid(ssid);
        }
    }
    Ok(0)
}

fn cmd_scan(cfg: &mut Config, to: Option<String>) -> Result<i32, String> {
    // Validate `--to` up front, before any side effects (connecting is
    // one): `add --to` errors on an unknown profile, so `scan --to` must
    // too instead of silently saving a network that never gets attached.
    if let Some(prof_name) = &to {
        if !cfg.profiles.contains_key(prof_name) {
            return Err(format!("unknown profile '{prof_name}'"));
        }
    }
    let iface = nm::wifi_interface_preferred(cfg.settings.interface.as_deref())
        .ok_or("no Wi-Fi adapter")?;
    nm::radio_on();
    nm::rescan(&iface, &[]);
    let entries = nm::scan_list(&iface);
    if entries.is_empty() {
        return Err("no networks found".into());
    }
    for (i, e) in entries.iter().enumerate() {
        println!(
            "{:>2}. {C_BOLD}{}{C_RESET} {C_DIM}sig {}  {}{C_RESET}",
            i + 1,
            if e.ssid.is_empty() {
                "<hidden>"
            } else {
                &e.ssid
            },
            e.signal,
            e.security
        );
    }
    let sel = prompt_line("Select number: ");
    let idx: usize = sel
        .parse::<usize>()
        .ok()
        .filter(|n| *n >= 1 && *n <= entries.len())
        .ok_or("invalid selection")?;
    let ssid = entries[idx - 1].ssid.clone();
    if ssid.is_empty() {
        return Err("cannot select a hidden SSID here; use `breadcrumbs add`".into());
    }
    let password = non_empty(prompt_secret(&format!("Password for '{ssid}': ")));
    let mut def = NetworkDef {
        ssid: ssid.clone(),
        password,
        dns: None,
        eap: None,
        identity: None,
        ca_cert: None,
        hidden: false,
    };
    if !nm::connect(&iface, &def, cfg.settings.connect_wait, &cfg.settings.dns) {
        return Err(format!("failed to connect to {ssid}"));
    }
    // A successful connect means NetworkManager now durably holds the PSK
    // (either in a freshly created profile, or one whose PSK we just set) —
    // breadcrumbs no longer needs to keep its own plaintext copy.
    def.password = None;
    match cfg.networks.iter_mut().find(|n| n.ssid == ssid) {
        Some(n) => n.password = None,
        None => cfg.networks.push(def),
    }
    if let Some(prof_name) = to {
        if let Some(prof) = cfg.profiles.get_mut(&prof_name) {
            if !prof.networks.contains(&ssid) {
                prof.networks.push(ssid.clone());
            }
        }
    }
    cfg.save()?;
    println!("{C_GREEN}connected + saved{C_RESET} {ssid}");
    Ok(0)
}

/// Mask a secret for display. Always renders the same fixed-length
/// placeholder so the output reveals neither the secret's length nor any
/// character of it (a fixed placeholder is what password managers show;
/// length-hiding also means multi-byte UTF-8 passwords need no special
/// handling).
fn mask(_p: &str) -> String {
    "•".repeat(8)
}

fn cmd_list(cfg: &Config, show_pw: bool) -> Result<i32, String> {
    println!("{C_BOLD}settings{C_RESET}");
    println!("  dns         {}", cfg.settings.dns);
    println!("  exit_node   {}", cfg.settings.exit_node);
    println!("  default     {}", cfg.settings.default_profile);
    println!("  watch every {}s", cfg.settings.watch_interval);

    println!("\n{C_BOLD}networks{C_RESET}");
    for n in &cfg.networks {
        let pw_display = match &n.password {
            Some(p) if show_pw => p.clone(),
            Some(p) => mask(p),
            // No local secret: NetworkManager already owns the credential for
            // this SSID, so there is nothing to mask — showing dots here
            // would falsely imply breadcrumbs is still hiding a password.
            None => format!("{C_DIM}managed by NetworkManager{C_RESET}"),
        };
        println!(
            "  {C_BOLD}{}{C_RESET}  {C_DIM}{}{}{C_RESET}",
            n.ssid,
            pw_display,
            if n.hidden { "  (hidden)" } else { "" }
        );
    }

    println!("\n{C_BOLD}profiles{C_RESET}");
    let cur = State::load(&cfg.settings.default_profile).profile;
    for (name, p) in &cfg.profiles {
        let mark = if *name == cur {
            format!("{C_GREEN}*{C_RESET}")
        } else {
            " ".into()
        };
        println!("{mark} {C_BOLD}{name}{C_RESET}");
        if let Some(b) = &p.bootstrap {
            println!("    bootstrap   {b}");
        }
        if p.tailscale {
            println!(
                "    tailscale   required (exit: {})",
                p.exit_node
                    .clone()
                    .unwrap_or_else(|| cfg.settings.exit_node.clone())
            );
        }
        let mut order: Vec<String> = p.networks.clone();
        if p.include_all_known {
            order.push("…all other known networks".into());
        }
        println!("    priority    {}", order.join(" > "));
    }
    Ok(0)
}

fn cmd_edit() -> Result<i32, String> {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "nano".into());
    let path = config::config_path();
    // EDITOR values routinely carry arguments ("code -w", "subl -w"), so
    // split on whitespace: the first token is the program, the rest are its
    // arguments. The path stays a separate argument — never interpolated
    // into a shell string — so it can't be used for injection.
    let mut parts = editor.split_whitespace();
    let prog = parts.next().unwrap_or("nano");
    let mut cmd = Command::new(prog);
    cmd.args(parts);
    let status = cmd
        .arg(&path)
        .status()
        .map_err(|e| format!("launching {editor}: {e}"))?;
    if !status.success() {
        return Err("editor exited with error".into());
    }
    match Config::load() {
        Ok(_) => {
            println!("{C_GREEN}config OK{C_RESET}");
            Ok(0)
        }
        Err(e) => Err(format!("config is now invalid: {e}")),
    }
}

fn cmd_doctor(cfg: &Config, override_p: &Option<String>, full: bool) -> Result<i32, String> {
    if full {
        let script = config::config_dir().join("diag.sh");
        if !script.exists() {
            return Err(format!(
                "diag.sh not found (expected at {})",
                script.display()
            ));
        }
        let st = Command::new("bash")
            .arg(&script)
            .status()
            .map_err(|e| format!("running diag: {e}"))?;
        return Ok(st.code().unwrap_or(1));
    }

    let p = active_profile(cfg, override_p);
    let s = crate::status::gather(cfg, &p);
    println!("{C_BOLD}breadcrumbs doctor{C_RESET}  (profile {p})");
    println!(
        "  network-manager {}",
        if nm::available() {
            "present (D-Bus)"
        } else {
            "MISSING"
        }
    );
    println!(
        "  tailscale   {}",
        if command_exists("tailscale") {
            "present"
        } else {
            "absent"
        }
    );
    println!(
        "  adapter     {}",
        s.iface.clone().unwrap_or_else(|| "none".into())
    );
    println!(
        "  ssid        {}",
        s.ssid.clone().unwrap_or_else(|| "—".into())
    );
    println!(
        "  ip          {}",
        s.ip.clone().unwrap_or_else(|| "—".into())
    );
    println!("  internet    {}", if s.internet { "ok" } else { "DOWN" });
    if let Some(h) = &s.tailscale {
        println!("  tailscale   {} (exit {})", h.describe(), s.exit_node);
    }

    if let Some(iface) = &s.iface {
        let visible = nm::visible_ssids(iface);
        let known: Vec<&str> = cfg
            .networks
            .iter()
            .filter(|n| visible.contains(&n.ssid))
            .map(|n| n.ssid.as_str())
            .collect();
        println!(
            "  in range    {}",
            if known.is_empty() {
                "none of your saved networks".into()
            } else {
                known.join(", ")
            }
        );
    }
    println!("\nFull report: {C_DIM}breadcrumbs doctor --full{C_RESET}");
    Ok(0)
}

fn cmd_cd(shell: bool) -> Result<i32, String> {
    let dir = config::config_dir();
    if shell {
        let sh = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let err = exec_replace(&sh, &dir);
        return Err(err);
    }
    println!("{}", dir.display());
    Ok(0)
}

/// Re-exec into an interactive login shell inside `dir`, replacing the
/// current process. `dir` is passed as `$1` to the shell script rather than
/// interpolated into the script text — the config dir can come from
/// `$XDG_CONFIG_HOME`/`$HOME`, and string-formatting an arbitrary path
/// straight into a `sh -c` command would let shell metacharacters (`$(...)`,
/// backticks, etc.) in that path execute as commands.
fn exec_replace(prog: &str, dir: &std::path::Path) -> String {
    use std::os::unix::process::CommandExt;
    let e = Command::new(prog)
        .arg("-lc")
        .arg("cd \"$1\" && exec \"$0\"")
        .arg(prog)
        .arg(dir)
        .exec();
    format!("exec {prog} failed: {e}")
}

fn cmd_install_service(enable: bool) -> Result<i32, String> {
    // Honor XDG_CONFIG_HOME like the rest of the app: systemd --user units
    // live in $XDG_CONFIG_HOME/systemd/user (default ~/.config/systemd/user).
    let unit_dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".config"))
        .join("systemd")
        .join("user");
    std::fs::create_dir_all(&unit_dir)
        .map_err(|e| format!("creating {}: {e}", unit_dir.display()))?;
    let bin = std::env::current_exe().map_err(|e| format!("resolving current executable: {e}"))?;
    // Ordering against graphical-session.target lets the watcher inherit the
    // session's DISPLAY/WAYLAND_DISPLAY/DBUS so notify-send and the Tailscale
    // login browser-open actually work. PATH is pinned because systemd --user
    // units do not get the login shell's PATH, and the watcher shells out to
    // tailscale/sudo/xdg-open by name (NetworkManager is reached over D-Bus,
    // so no nmcli is needed).
    let unit = format!(
        "[Unit]\n\
         Description=breadcrumbs Wi-Fi state machine watcher\n\
         After=network.target NetworkManager.service graphical-session.target\n\
         Wants=network.target graphical-session.target\n\n\
         [Service]\n\
         Type=simple\n\
         Environment=PATH=/usr/local/bin:/usr/bin:/bin\n\
         ExecStart={bin} watch\n\
         Restart=always\n\
         RestartSec=5\n\
         Nice=5\n\n\
         [Install]\n\
         WantedBy=default.target\n",
        bin = bin.display()
    );
    let unit_path = unit_dir.join("breadcrumbs.service");
    std::fs::write(&unit_path, unit)
        .map_err(|e| format!("writing {}: {e}", unit_path.display()))?;
    println!("{C_GREEN}wrote{C_RESET} {}", unit_path.display());

    let _ = util::run(
        "systemctl",
        &["--user", "daemon-reload"],
        Duration::from_secs(10),
    );
    if enable {
        let o = util::run(
            "systemctl",
            &["--user", "enable", "--now", "breadcrumbs.service"],
            Duration::from_secs(15),
        );
        if o.success {
            println!("{C_GREEN}enabled + started{C_RESET} breadcrumbs.service");
        } else {
            println!(
                "{C_YELLOW}unit installed{C_RESET}; enable failed: {}",
                o.stderr.trim()
            );
            return Ok(1);
        }
    } else {
        println!("Run: systemctl --user enable --now breadcrumbs.service");
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_is_fixed_length_regardless_of_secret() {
        // Fixed-length masking: the output must reveal neither the secret's
        // length nor any character of it — for empty, short, and long
        // secrets alike.
        assert_eq!(mask(""), "•".repeat(8));
        assert_eq!(mask("a"), "•".repeat(8));
        assert_eq!(mask("ab"), "•".repeat(8));
        assert_eq!(mask("hunter2"), "•".repeat(8));
        assert!(!mask("hunter2").contains('h'));
    }

    #[test]
    fn mask_multibyte_password_does_not_panic() {
        // Regression test: the old byte-slicing `&p[..1]` panicked whenever
        // the first character of the password was multi-byte UTF-8 (e.g. an
        // emoji or accented character), since byte index 1 can land mid-char.
        let pw = "日本語パスワード";
        let masked = mask(pw);
        assert_eq!(masked, "•".repeat(8));
        assert!(masked.chars().all(|c| c == '•'));
    }

    #[test]
    fn mask_emoji_first_character_does_not_panic() {
        let pw = "🔒password123";
        assert_eq!(mask(pw), "•".repeat(8));
    }
}
