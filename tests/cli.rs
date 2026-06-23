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

#[test]
fn profile_list_marks_active_profile() {
    let sb = Sandbox::new();

    // Default profile is "away".
    let o = sb.cmd(&["profile", "list"]);
    assert!(o.status.success());
    let out = stdout(&o);
    // The active profile line starts with "* ".
    assert!(
        out.lines().any(|l| l.starts_with("* away")),
        "active profile marker missing: {out}"
    );
    // Inactive profiles start with "  ".
    assert!(
        out.lines().any(|l| l.starts_with("  home")),
        "inactive profile format wrong: {out}"
    );

    // After switching to "work" the marker should move.
    sb.cmd(&["profile", "set", "work", "--no-apply"]);
    let o = sb.cmd(&["profile", "list"]);
    let out = stdout(&o);
    assert!(out.lines().any(|l| l.starts_with("* work")));
    assert!(out.lines().any(|l| l.starts_with("  away")));
}

#[test]
fn add_saves_network_to_config() {
    let sb = Sandbox::new();

    let o = sb.cmd(&["add", "CafeWifi", "mypassword"]);
    assert!(
        o.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(stdout(&o).contains("saved"));

    // The network should now appear in the config file.
    let text = fs::read_to_string(sb.config_file()).unwrap();
    assert!(text.contains("CafeWifi"), "SSID missing from config");
    assert!(text.contains("mypassword"), "password missing from config");
}

#[test]
fn add_attaches_ssid_to_profile_at_position() {
    let sb = Sandbox::new();

    // First add without attaching, then add again with --to.
    sb.cmd(&["add", "HomeWifi", "pw1"]);
    let o = sb.cmd(&["add", "HomeWifi", "pw1", "--to", "home"]);
    assert!(
        o.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&o.stderr)
    );

    let text = fs::read_to_string(sb.config_file()).unwrap();
    // After attaching, the home profile's networks list should contain HomeWifi.
    assert!(
        text.contains("HomeWifi"),
        "SSID not found in config after --to: {text}"
    );
}

#[test]
fn add_to_unknown_profile_fails() {
    let sb = Sandbox::new();
    let o = sb.cmd(&["add", "SomeNet", "pw", "--to", "nonexistent"]);
    assert!(!o.status.success());
}

#[test]
fn add_updates_existing_network_password() {
    let sb = Sandbox::new();

    sb.cmd(&["add", "MyNet", "oldpass"]);
    let o = sb.cmd(&["add", "MyNet", "newpass"]);
    assert!(o.status.success());

    let text = fs::read_to_string(sb.config_file()).unwrap();
    assert!(text.contains("newpass"), "updated password missing");
    // Old password must be gone.
    assert!(!text.contains("oldpass"), "old password still present");
    // Only one entry for MyNet.
    assert_eq!(
        text.matches("MyNet").count(),
        1,
        "duplicate network entries"
    );
}

#[test]
fn forget_removes_network_from_config() {
    let sb = Sandbox::new();

    sb.cmd(&["add", "ToDelete", "pw"]);
    let text = fs::read_to_string(sb.config_file()).unwrap();
    assert!(text.contains("ToDelete"));

    let o = sb.cmd(&["forget", "ToDelete"]);
    assert!(o.status.success());
    assert!(stdout(&o).contains("forgot"));

    let text = fs::read_to_string(sb.config_file()).unwrap();
    assert!(
        !text.contains("ToDelete"),
        "network still in config after forget"
    );
}

#[test]
fn forget_nonexistent_network_is_graceful() {
    let sb = Sandbox::new();
    // Should succeed (idempotent) even if the SSID was never saved.
    let o = sb.cmd(&["forget", "NeverSaved"]);
    assert!(o.status.success());
}

#[test]
fn forget_removes_ssid_from_profile_networks_list() {
    let sb = Sandbox::new();

    // Add "WorkNet" and attach it to the "work" profile.
    sb.cmd(&["add", "WorkNet", "pw", "--to", "work"]);
    let text = fs::read_to_string(sb.config_file()).unwrap();
    assert!(text.contains("WorkNet"));

    sb.cmd(&["forget", "WorkNet"]);

    let text = fs::read_to_string(sb.config_file()).unwrap();
    assert!(
        !text.contains("WorkNet"),
        "SSID still in profile after forget"
    );
}

#[test]
fn list_masks_passwords_by_default() {
    let sb = Sandbox::new();
    sb.cmd(&["add", "SecretNet", "hunter2"]);

    let o = sb.cmd(&["list"]);
    assert!(o.status.success());
    let out = stdout(&o);
    assert!(
        !out.contains("hunter2"),
        "plain-text password exposed in list"
    );
    // A masking bullet should appear.
    assert!(out.contains('•'), "no masking character in list output");
}

#[test]
fn list_masks_multibyte_password_without_panicking() {
    let sb = Sandbox::new();
    // A password whose first character is multi-byte UTF-8 used to panic the
    // byte-slicing mask(); list must mask it cleanly instead.
    sb.cmd(&["add", "UnicodeNet", "ñoño-café-🔐"]);

    let o = sb.cmd(&["list"]);
    assert!(
        o.status.success(),
        "list crashed on multibyte password: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    let out = stdout(&o);
    assert!(
        !out.contains("ñoño-café-🔐"),
        "password leaked in masked list"
    );
    assert!(out.contains('•'), "no masking character in list output");
}

#[test]
fn list_show_passwords_reveals_password() {
    let sb = Sandbox::new();
    sb.cmd(&["add", "SecretNet", "hunter2"]);

    let o = sb.cmd(&["list", "--show-passwords"]);
    assert!(o.status.success());
    assert!(
        stdout(&o).contains("hunter2"),
        "password not shown with --show-passwords"
    );
}

#[test]
fn cd_prints_config_directory() {
    let sb = Sandbox::new();
    // Trigger config creation first.
    sb.cmd(&["list"]);

    let o = sb.cmd(&["cd"]);
    assert!(o.status.success());
    let out = stdout(&o).trim().to_string();
    assert!(!out.is_empty(), "cd produced no output");
    // The printed path must end with "breadcrumbs" (the config subdirectory).
    assert!(out.ends_with("breadcrumbs"), "unexpected config dir: {out}");
}

#[test]
fn doctor_runs_without_crashing() {
    let sb = Sandbox::new();
    // With an empty PATH, nmcli/tailscale are absent. Doctor should report
    // "MISSING"/"absent" but still exit successfully (it's a diag tool).
    let o = sb.cmd(&["doctor"]);
    assert!(
        o.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    let out = stdout(&o);
    assert!(out.contains("nmcli"), "doctor missing nmcli line");
    assert!(out.contains("tailscale"), "doctor missing tailscale line");
}

#[test]
fn status_exits_nonzero_when_unhealthy() {
    let sb = Sandbox::new();
    // No nmcli/tailscale → internet check fails → unhealthy → exit code 1.
    let o = sb.cmd(&["status"]);
    assert!(
        !o.status.success(),
        "expected non-zero exit for unhealthy status"
    );
    let out = stdout(&o);
    assert!(
        out.contains("breadcrumbs"),
        "missing header in status output"
    );
    assert!(
        out.contains("profile"),
        "missing profile line in status output"
    );
}

#[test]
fn profile_override_flag_does_not_persist() {
    let sb = Sandbox::new();

    // Set profile to "home" persistently.
    sb.cmd(&["profile", "set", "home", "--no-apply"]);

    // Use --profile flag to override for a single run (status).
    let o = sb.cmd(&["--profile", "work", "status"]);
    // Status exits non-zero (no network), but it should show the overridden profile.
    let out = stdout(&o);
    assert!(out.contains("work"), "override profile not shown in status");

    // The persistent profile must still be "home".
    let o = sb.cmd(&["profile", "get"]);
    assert_eq!(stdout(&o).trim(), "home");
}

#[test]
fn add_hidden_flag_is_persisted() {
    let sb = Sandbox::new();
    let o = sb.cmd(&["add", "HiddenNet", "pw", "--hidden"]);
    assert!(o.status.success());

    let text = fs::read_to_string(sb.config_file()).unwrap();
    assert!(text.contains("hidden = true"), "hidden flag not persisted");
}

#[test]
fn status_json_is_valid_and_machine_readable() {
    let sb = Sandbox::new();
    // No nmcli/tailscale → unhealthy, exit 1, but JSON must still be valid.
    let o = sb.cmd(&["status", "--json"]);
    assert!(
        !o.status.success(),
        "expected non-zero exit for unhealthy status"
    );
    let v: serde_json::Value =
        serde_json::from_str(&stdout(&o)).expect("status --json did not emit valid JSON");
    assert_eq!(v["profile"], "away");
    assert_eq!(v["internet"], false);
    assert_eq!(v["healthy"], false);
    assert!(v["tailscale"].is_object());
}

#[test]
fn profile_add_persists_detect_ssids_and_remove_deletes() {
    let sb = Sandbox::new();

    let o = sb.cmd(&[
        "profile", "add", "lab", "--detect", "LabWifi", "--detect", "LabGuest",
    ]);
    assert!(
        o.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&o.stderr)
    );

    let text = fs::read_to_string(sb.config_file()).unwrap();
    assert!(
        text.contains("[profiles.lab]"),
        "profile not written: {text}"
    );
    assert!(
        text.contains("LabWifi") && text.contains("LabGuest"),
        "detect ssids missing"
    );

    // Adding the same profile again is rejected.
    assert!(!sb.cmd(&["profile", "add", "lab"]).status.success());

    // Remove it.
    let o = sb.cmd(&["profile", "remove", "lab"]);
    assert!(o.status.success());
    let text = fs::read_to_string(sb.config_file()).unwrap();
    assert!(
        !text.contains("[profiles.lab]"),
        "profile still present after remove"
    );
}

#[test]
fn profile_remove_core_is_rejected() {
    let sb = Sandbox::new();
    let o = sb.cmd(&["profile", "remove", "home"]);
    assert!(!o.status.success(), "removing a core profile should fail");
}

#[test]
fn install_service_writes_unit_file() {
    let sb = Sandbox::new();
    // Install but don't enable (systemctl is absent in the empty PATH, but
    // writing the unit file should succeed regardless).
    let o = sb.cmd(&["install-service", "--no-enable"]);
    assert!(
        o.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    let unit = sb.root.join(".config/systemd/user/breadcrumbs.service");
    assert!(unit.exists(), "unit file not written at {}", unit.display());
    let content = fs::read_to_string(&unit).unwrap();
    assert!(content.contains("ExecStart="), "unit missing ExecStart");
    assert!(
        content.contains("breadcrumbs watch"),
        "unit missing watch subcommand"
    );
}
