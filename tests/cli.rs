//! End-to-end CLI tests. Each run is fully isolated: HOME / XDG dirs point at a
//! throwaway tempdir and PATH is emptied so no real `nmcli`/`tailscale`/`date`
//! is ever invoked and the host system is never touched.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const BIN: &str = env!("CARGO_BIN_EXE_breadcrumbs");

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new() -> Sandbox {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "breadcrumbs-it-{}-{}-{}",
            std::process::id(),
            n,
            nanos
        ));
        fs::create_dir_all(root.join("bin")).unwrap();
        Sandbox { root }
    }

    /// Binary invocation with an isolated, side-effect-free environment.
    fn cmd(&self, args: &[&str]) -> std::process::Output {
        Command::new(BIN)
            .args(args)
            .env_clear()
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            // Empty bin dir => no external commands resolve.
            .env("PATH", self.root.join("bin"))
            .output()
            .expect("failed to spawn breadcrumbs")
    }

    fn config_file(&self) -> PathBuf {
        self.root.join("config/breadcrumbs/breadcrumbs.toml")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn stdout(o: &std::process::Output) -> String {
    String::from_utf8_lossy(&o.stdout).to_string()
}

#[test]
fn help_lists_all_commands() {
    let sb = Sandbox::new();
    let o = sb.cmd(&["--help"]);
    assert!(o.status.success());
    let out = stdout(&o);
    assert!(out.contains("Profile-aware Wi-Fi state machine"));
    for c in [
        "status",
        "init",
        "watch",
        "profile",
        "doctor",
        "install-service",
    ] {
        assert!(out.contains(c), "help missing `{c}`");
    }
}

#[test]
fn version_prints_crate_version() {
    let sb = Sandbox::new();
    let o = sb.cmd(&["--version"]);
    assert!(o.status.success());
    assert!(stdout(&o).contains("breadcrumbs"));
}

#[test]
fn list_bootstraps_config_with_core_profiles() {
    let sb = Sandbox::new();
    let o = sb.cmd(&["list"]);
    assert!(
        o.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    let out = stdout(&o);

    // All three core profiles must be present on a fresh install.
    assert!(out.contains("home") && out.contains("work") && out.contains("away"));

    // Config was materialised on disk and is valid TOML.
    let cfg = sb.config_file();
    assert!(cfg.exists(), "config not created at {}", cfg.display());
    let text = fs::read_to_string(&cfg).unwrap();
    assert!(text.contains("[profiles.home]"));
    assert!(text.contains("[profiles.work]"));
    assert!(text.contains("[profiles.away]"));
}

#[test]
fn profile_defaults_to_away_then_persists_set() {
    let sb = Sandbox::new();

    let o = sb.cmd(&["profile", "get"]);
    assert!(o.status.success());
    assert_eq!(stdout(&o).trim(), "away");

    // `set --no-apply` must not touch the network (no nmcli available anyway).
    let o = sb.cmd(&["profile", "set", "home", "--no-apply"]);
    assert!(
        o.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&o.stderr)
    );

    let o = sb.cmd(&["profile", "get"]);
    assert_eq!(stdout(&o).trim(), "home");

    // Unknown profile is rejected.
    let o = sb.cmd(&["profile", "set", "bogus"]);
    assert!(!o.status.success());
}

#[test]
fn unknown_profile_override_is_reported() {
    let sb = Sandbox::new();
    let o = sb.cmd(&["--profile", "nope", "init"]);
    assert!(!o.status.success());
}
