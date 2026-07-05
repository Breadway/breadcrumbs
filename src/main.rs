mod backend;
mod config;
mod flow;
mod nm;
mod notify;
mod state;
mod status;
mod tailscale;
mod util;
mod watch;

use std::io::{BufRead, Write};
use std::process::Command;
use std::time::Duration;

use clap::{Parser, Subcommand};

use backend::Backend;
use config::{Config, NetworkDef};
use state::State;
use util::{command_exists, home_dir, run};

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

#[derive(Subcommand)]
enum Cmd {
    /// Show current Wi-Fi / profile / Tailscale status (default)
    Status {
        /// Emit machine-readable JSON instead of the human summary
        #[arg(long)]
        json: bool,
    },
    /// Run the full connect sequence for the active profile
    #[command(visible_aliases = ["up", "connect", "i"])]
    Init,
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
    },
    /// Add or update a saved network
    Add {
        ssid: String,
        /// Password (prompted if omitted)
        password: Option<String>,
        /// Network is hidden (does not broadcast its SSID)
        #[arg(long)]
        hidden: bool,
        /// Attach this SSID to a profile's priority list
        #[arg(long)]
        to: Option<String>,
        /// Position in the profile list (0 = highest priority)
        #[arg(long)]
        at: Option<usize>,
    },
    /// Remove a saved network (config + NetworkManager)
    Forget { ssid: String },
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
    /// Connect to a specific saved network by SSID, bypassing profile routing
    Join { ssid: String },
    /// List saved network SSIDs
    Networks {
        /// Emit a JSON array instead of one-per-line
        #[arg(long)]
        json: bool,
    },
    /// Scan for visible networks and list them with signal strength
    ScanList {
        /// Emit JSON instead of human-readable output
        #[arg(long)]
        json: bool,
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
    /// Create a new (empty) profile
    Add {
        name: String,
        /// SSID whose presence marks this location (repeatable, for `detect`)
        #[arg(long = "detect")]
        detect: Vec<String>,
    },
    /// Delete a profile (core profiles home/work/away cannot be removed)
    Remove { name: String },
}

fn main() {
    let cli = Cli::parse();
    let code = match real_main(cli) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{C_RED}error:{C_RESET} {e}");
            1
        }
    };
    std::process::exit(code);
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
    let be = backend::System;

    match cmd {
        Cmd::Status { json } => cmd_status(&be, &cfg, &cli.profile, json),
        Cmd::Init => {
            let p = active_profile(&cfg, &cli.profile);
            let outcome = flow::run(&be, &cfg, &p);
            print_outcome(&p, &outcome);
            Ok(if outcome.ok() { 0 } else { 1 })
        }
        Cmd::Watch { no_initial } => Ok(watch::run(cfg, !no_initial)),
        Cmd::Profile { action } => cmd_profile(&be, &mut cfg, action),
        Cmd::Detect { apply } => cmd_detect(&be, &cfg, apply),
        Cmd::Add {
            ssid,
            password,
            hidden,
            to,
            at,
        } => cmd_add(&mut cfg, ssid, password, hidden, to, at),
        Cmd::Forget { ssid } => cmd_forget(&mut cfg, &ssid),
        Cmd::Join { ssid } => cmd_join(&be, &cfg, &ssid),
        Cmd::Networks { json } => cmd_networks(&cfg, json),
        Cmd::ScanList { json } => cmd_scan_list(&cfg, json),
        Cmd::Scan { to } => cmd_scan(&mut cfg, to),
        Cmd::List { show_passwords } => cmd_list(&cfg, show_passwords),
        Cmd::Edit => cmd_edit(),
        Cmd::Doctor { full } => cmd_doctor(&be, &cfg, &cli.profile, full),
        Cmd::InstallService { no_enable } => cmd_install_service(!no_enable),
        Cmd::Cd { .. } => unreachable!(),
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

fn status_healthy(s: &status::Status) -> bool {
    s.internet
        && s.iface.is_some()
        && (!s.tailscale_required || s.tailscale.as_ref().map(|h| h.is_ok()).unwrap_or(false))
}

fn cmd_status(
    be: &dyn Backend,
    cfg: &Config,
    override_p: &Option<String>,
    json: bool,
) -> Result<i32, String> {
    let p = active_profile(cfg, override_p);
    let s = status::gather(be, cfg, &p);
    let healthy = status_healthy(&s);

    if json {
        let v = serde_json::json!({
            "profile": p,
            "adapter": s.iface,
            "ssid": s.ssid,
            "ip": s.ip,
            "internet": s.internet,
            "captive_portal": s.portal,
            "tailscale": {
                "required": s.tailscale_required,
                "installed": s.tailscale.is_some(),
                "ok": s.tailscale.as_ref().map(|h| h.is_ok()),
                "health": s.tailscale.as_ref().map(|h| h.describe()),
                "exit_node": s.exit_node,
            },
            "healthy": healthy,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&v).unwrap_or_else(|_| "{}".into())
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
    if let Some(portal) = &s.portal {
        let detail = if portal.is_empty() {
            "sign-in required".to_string()
        } else {
            portal.clone()
        };
        println!("  portal      {C_YELLOW}captive portal{C_RESET} {C_DIM}{detail}{C_RESET}");
    }

    match (&s.tailscale, s.tailscale_required) {
        (Some(h), req) => {
            let ok = h.is_ok();
            println!(
                "  tailscale   {} {} {C_DIM}(exit node: {}{}){C_RESET}",
                dot(ok || !req),
                h.describe(),
                if s.exit_node.is_empty() { "none" } else { &s.exit_node },
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

const CORE_PROFILES: [&str; 3] = ["home", "work", "away"];

fn cmd_profile(
    be: &dyn Backend,
    cfg: &mut Config,
    action: Option<ProfileCmd>,
) -> Result<i32, String> {
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
        ProfileCmd::Add { name, detect } => {
            if cfg.profiles.contains_key(&name) {
                return Err(format!("profile '{name}' already exists"));
            }
            cfg.profiles.insert(
                name.clone(),
                config::Profile {
                    detect_ssids: detect,
                    ..Default::default()
                },
            );
            cfg.save()?;
            println!("{C_GREEN}added{C_RESET} profile {name}");
            Ok(0)
        }
        ProfileCmd::Remove { name } => {
            if CORE_PROFILES.contains(&name.as_str()) {
                return Err(format!(
                    "'{name}' is a core profile and is always recreated; clear its networks instead"
                ));
            }
            if cfg.profiles.remove(&name).is_none() {
                return Err(format!("unknown profile '{name}'"));
            }
            cfg.save()?;
            println!("{C_GREEN}removed{C_RESET} profile {name}");
            Ok(0)
        }
        ProfileCmd::Set { name, no_apply } => {
            if !cfg.profiles.contains_key(&name) {
                let avail: Vec<&String> = cfg.profiles.keys().collect();
                return Err(format!("unknown profile '{name}'. Available: {avail:?}"));
            }
            let st = State {
                profile: name.clone(),
                updated: util::timestamp(),
            };
            st.save()?;
            notify::log(&format!("profile set -> {name}"));
            println!("profile = {C_BOLD}{name}{C_RESET}");
            if no_apply {
                return Ok(0);
            }
            let outcome = flow::run(be, cfg, &name);
            print_outcome(&name, &outcome);
            Ok(if outcome.ok() { 0 } else { 1 })
        }
    }
}

fn detect_profile(be: &dyn Backend, cfg: &Config) -> Option<String> {
    let iface = be.wifi_interface()?;
    be.radio_on();
    be.rescan(&iface, &[]);
    let visible = be.visible_ssids(&iface);

    // Pick the profile with the most marker SSIDs in range, so overlapping
    // locations disambiguate by strength of evidence. Profiles iterate in
    // BTreeMap (alphabetical) order, which deterministically breaks ties.
    let mut best: Option<(usize, String)> = None;
    for (name, profile) in &cfg.profiles {
        let score = profile
            .detect_ssids
            .iter()
            .filter(|s| visible.contains(s.as_str()))
            .count();
        if score == 0 {
            continue;
        }
        if best.as_ref().map(|(s, _)| score > *s).unwrap_or(true) {
            best = Some((score, name.clone()));
        }
    }

    // Fall back to the default profile if no markers matched.
    Some(
        best.map(|(_, name)| name)
            .unwrap_or_else(|| cfg.settings.default_profile.clone()),
    )
}

fn cmd_detect(be: &dyn Backend, cfg: &Config, apply: bool) -> Result<i32, String> {
    match detect_profile(be, cfg) {
        Some(p) => {
            println!("{p}");
            if apply {
                State {
                    profile: p.clone(),
                    updated: util::timestamp(),
                }
                .save()?;
                let outcome = flow::run(be, cfg, &p);
                print_outcome(&p, &outcome);
                return Ok(if outcome.ok() { 0 } else { 1 });
            }
            Ok(0)
        }
        None => Err("could not detect a profile (no Wi-Fi adapter?)".into()),
    }
}

fn prompt_line(msg: &str) -> String {
    print!("{msg}");
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    let _ = std::io::stdin().lock().read_line(&mut s);
    s.trim_end_matches(['\n', '\r']).to_string()
}

fn prompt_secret(msg: &str) -> String {
    // `util::run` redirects child stdin to /dev/null, so plain `stty -echo`
    // would target the wrong fd and silently leave echo ON (leaking the
    // password to the screen). `-F /dev/tty` makes stty act on the controlling
    // terminal directly. If there is no tty we fall back to visible input.
    let had_tty = run("stty", &["-F", "/dev/tty", "-echo"], Duration::from_secs(2)).success;
    let val = prompt_line(msg);
    if had_tty {
        let _ = run("stty", &["-F", "/dev/tty", "echo"], Duration::from_secs(2));
        println!();
    }
    val
}

fn cmd_add(
    cfg: &mut Config,
    ssid: String,
    password: Option<String>,
    hidden: bool,
    to: Option<String>,
    at: Option<usize>,
) -> Result<i32, String> {
    let password = match password {
        Some(p) => p,
        None => prompt_secret(&format!("Password for '{ssid}': ")),
    };
    match cfg.networks.iter_mut().find(|n| n.ssid == ssid) {
        Some(n) => {
            n.password = password;
            n.hidden = hidden || n.hidden;
        }
        None => cfg.networks.push(NetworkDef {
            ssid: ssid.clone(),
            password,
            hidden,
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

fn cmd_join(be: &dyn Backend, cfg: &Config, ssid: &str) -> Result<i32, String> {
    let net = cfg
        .network(ssid)
        .ok_or_else(|| format!("no saved network '{ssid}' — add it first with `breadcrumbs add {ssid}`"))?;
    let iface = be
        .wifi_interface()
        .ok_or_else(|| "no Wi-Fi adapter found".to_string())?;
    be.radio_on();
    match nm::connect_verbose(&iface, net, cfg.settings.nmcli_wait, &cfg.settings.dns) {
        Ok(()) => {
            println!("{C_GREEN}connected{C_RESET} {C_BOLD}{ssid}{C_RESET}");
            Ok(0)
        }
        Err(e) => {
            eprintln!("{C_RED}connect failed{C_RESET}: {e}");
            Ok(1)
        }
    }
}

fn cmd_scan_list(cfg: &Config, json: bool) -> Result<i32, String> {
    let iface = nm::wifi_interface().ok_or("no Wi-Fi adapter found")?;
    let entries = nm::scan_list(&iface);
    let saved: std::collections::HashSet<&str> =
        cfg.networks.iter().map(|n| n.ssid.as_str()).collect();
    if json {
        let v: Vec<serde_json::Value> = entries
            .iter()
            .map(|e| {
                serde_json::json!({
                    "ssid":     e.ssid,
                    "signal":   e.signal,
                    "security": e.security,
                    "saved":    saved.contains(e.ssid.as_str()),
                })
            })
            .collect();
        println!("{}", serde_json::to_string(&v).unwrap_or_else(|_| "[]".into()));
    } else {
        for e in &entries {
            let mark = if saved.contains(e.ssid.as_str()) { "*" } else { " " };
            println!("{mark} {:>3}%  {}  {}", e.signal, e.ssid, e.security);
        }
    }
    Ok(0)
}

fn cmd_networks(cfg: &Config, json: bool) -> Result<i32, String> {
    let ssids: Vec<&str> = cfg.networks.iter().map(|n| n.ssid.as_str()).collect();
    if json {
        println!("{}", serde_json::to_string(&ssids).unwrap_or_else(|_| "[]".into()));
    } else {
        for ssid in &ssids {
            println!("{ssid}");
        }
    }
    Ok(0)
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

fn cmd_scan(cfg: &mut Config, to: Option<String>) -> Result<i32, String> {
    let iface = nm::wifi_interface().ok_or("no Wi-Fi adapter")?;
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
    let password = prompt_secret(&format!("Password for '{ssid}': "));
    let def = NetworkDef {
        ssid: ssid.clone(),
        password: password.clone(),
        hidden: false,
    };
    if !nm::connect(&iface, &def, cfg.settings.nmcli_wait, &cfg.settings.dns) {
        return Err(format!("failed to connect to {ssid}"));
    }
    match cfg.networks.iter_mut().find(|n| n.ssid == ssid) {
        Some(n) => n.password = password,
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

fn mask(p: &str) -> String {
    // Count by characters, not bytes: slicing &p[..1] would panic on a
    // multi-byte first character (valid in WPA passphrases).
    let count = p.chars().count();
    if count <= 2 {
        "••".into()
    } else {
        let first: String = p.chars().take(1).collect();
        format!("{}{}", first, "•".repeat(count - 1))
    }
}

fn cmd_list(cfg: &Config, show_pw: bool) -> Result<i32, String> {
    println!("{C_BOLD}settings{C_RESET}");
    println!("  dns         {}", cfg.settings.dns);
    println!("  exit_node   {}", cfg.settings.exit_node);
    println!("  default     {}", cfg.settings.default_profile);
    println!("  watch every {}s", cfg.settings.watch_interval);

    println!("\n{C_BOLD}networks{C_RESET}");
    for n in &cfg.networks {
        println!(
            "  {C_BOLD}{}{C_RESET}  {C_DIM}{}{}{C_RESET}",
            n.ssid,
            if show_pw {
                n.password.clone()
            } else {
                mask(&n.password)
            },
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
            let exit_node = p
                .exit_node
                .clone()
                .unwrap_or_else(|| cfg.settings.exit_node.clone());
            println!(
                "    tailscale   required (exit node: {})",
                if exit_node.is_empty() { "none".to_string() } else { exit_node }
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
    let status = Command::new(&editor)
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

fn cmd_doctor(
    be: &dyn Backend,
    cfg: &Config,
    override_p: &Option<String>,
    full: bool,
) -> Result<i32, String> {
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
    let s = status::gather(be, cfg, &p);
    println!("{C_BOLD}breadcrumbs doctor{C_RESET}  (profile {p})");
    println!(
        "  nmcli       {}",
        if command_exists("nmcli") {
            "present"
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
        println!(
            "  tailscale   {} (exit node: {})",
            h.describe(),
            if s.exit_node.is_empty() { "none" } else { &s.exit_node }
        );
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
        let err = exec_replace(&sh, &["-lc", &format!("cd {:?} && exec {sh}", dir)]);
        return Err(err);
    }
    println!("{}", dir.display());
    Ok(0)
}

fn exec_replace(prog: &str, args: &[&str]) -> String {
    use std::os::unix::process::CommandExt;
    let e = Command::new(prog).args(args).exec();
    format!("exec {prog} failed: {e}")
}

fn cmd_install_service(enable: bool) -> Result<i32, String> {
    let unit_dir = home_dir().join(".config/systemd/user");
    std::fs::create_dir_all(&unit_dir)
        .map_err(|e| format!("creating {}: {e}", unit_dir.display()))?;
    let bin = std::env::current_exe().map_err(|e| format!("resolving current executable: {e}"))?;
    // Ordering against graphical-session.target lets the watcher inherit the
    // session's DISPLAY/WAYLAND_DISPLAY/DBUS so notify-send and the Tailscale
    // login browser-open actually work. PATH is pinned because systemd --user
    // units do not get the login shell's PATH, and the watcher shells out to
    // nmcli/tailscale/sudo/xdg-open by name.
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

    let _ = run(
        "systemctl",
        &["--user", "daemon-reload"],
        Duration::from_secs(10),
    );
    if enable {
        let o = run(
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
