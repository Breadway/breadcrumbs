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
        self.cmd_env(args, &[])
    }

    /// Like [`cmd`], with extra environment variables layered on top of the
    /// isolated base (e.g. `EDITOR` for the `edit` command).
    fn cmd_env(&self, args: &[&str], extra: &[(&str, &str)]) -> std::process::Output {
        let mut c = Command::new(BIN);
        c.args(args)
            .env_clear()
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            // Empty bin dir => no external commands resolve.
            .env("PATH", self.root.join("bin"));
        for (k, v) in extra {
            c.env(k, v);
        }
        c.output().expect("failed to spawn breadcrumbs")
    }

    fn config_file(&self) -> PathBuf {
        self.root.join("config/breadcrumbs/breadcrumbs.toml")
    }

    /// Saved networks live in their own file, split out of `breadcrumbs.toml`
    /// (see `config::networks_path`).
    fn networks_file(&self) -> PathBuf {
        self.root.join("config/breadcrumbs/networks.toml")
    }

    /// Write an executable shell script into the sandbox's PATH dir so a
    /// test can stand in for an external command (e.g. `$EDITOR`).
    fn write_fake_bin(&self, name: &str, script: &str) -> PathBuf {
        let path = self.root.join("bin").join(name);
        fs::write(&path, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
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

fn stderr(o: &std::process::Output) -> String {
    String::from_utf8_lossy(&o.stderr).to_string()
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
fn profile_set_unknown_reports_available_profiles() {
    let sb = Sandbox::new();
    sb.cmd(&["list"]); // bootstrap the config
    let o = sb.cmd(&["profile", "set", "bogus"]);
    assert!(!o.status.success());
    let err = stderr(&o);
    assert!(err.contains("unknown profile"), "stderr: {err}");
    assert!(err.contains("home") && err.contains("work") && err.contains("away"));
}

#[test]
fn profile_list_marks_exactly_the_current_profile() {
    let sb = Sandbox::new();
    sb.cmd(&["profile", "set", "home", "--no-apply"]);
    let o = sb.cmd(&["profile", "list"]);
    assert!(o.status.success());
    let out = stdout(&o);
    assert!(out.contains("* home"), "out: {out}");
    assert_eq!(
        out.lines().filter(|l| l.trim_start().starts_with('*')).count(),
        1,
        "expected exactly one marked profile, got: {out}"
    );
}

#[test]
fn add_with_multibyte_password_does_not_crash_list() {
    // Regression test for a byte-slicing panic in the password-masking code:
    // `list` used to index into the first *byte* of the password, which
    // panicked whenever that byte fell mid-character in a multi-byte UTF-8
    // password (e.g. non-Latin scripts or an emoji as the first character).
    let sb = Sandbox::new();
    let add = sb.cmd(&["add", "CafeWifi", "日本語パスワード🔒"]);
    assert!(add.status.success(), "stderr: {}", stderr(&add));

    let list = sb.cmd(&["list"]);
    assert!(
        list.status.success(),
        "list crashed on multibyte password — stderr: {}",
        stderr(&list)
    );
    assert!(!stdout(&list).contains("日本語パスワード🔒"));
}

#[test]
fn list_hides_password_by_default_and_reveals_with_flag() {
    let sb = Sandbox::new();
    sb.cmd(&["add", "CafeWifi", "hunter2"]);

    let hidden = sb.cmd(&["list"]);
    assert!(!stdout(&hidden).contains("hunter2"));

    let shown = sb.cmd(&["list", "--show-passwords"]);
    assert!(stdout(&shown).contains("hunter2"));
}

#[test]
fn add_to_profile_persists_in_config_priority_list() {
    let sb = Sandbox::new();
    sb.cmd(&["list"]); // bootstrap
    let o = sb.cmd(&["add", "CafeWifi", "pw", "--to", "home"]);
    assert!(o.status.success(), "stderr: {}", stderr(&o));

    let text = fs::read_to_string(sb.config_file()).unwrap();
    let home_section = text.split("[profiles.home]").nth(1).unwrap_or("");
    assert!(
        home_section.contains("CafeWifi"),
        "CafeWifi not attached under [profiles.home]: {text}"
    );
}

#[test]
fn forget_removes_network_from_config() {
    let sb = Sandbox::new();
    sb.cmd(&["add", "CafeWifi", "pw", "--to", "away"]);
    let o = sb.cmd(&["forget", "CafeWifi"]);
    assert!(o.status.success(), "stderr: {}", stderr(&o));

    // Networks live in networks.toml (see the split-secrets test below) —
    // that's the file that actually needs to lose the entry.
    let networks = fs::read_to_string(sb.networks_file()).unwrap();
    assert!(
        !networks.contains("CafeWifi"),
        "network still in networks.toml: {networks}"
    );
    // ...and it should never have been in breadcrumbs.toml to begin with.
    let text = fs::read_to_string(sb.config_file()).unwrap();
    assert!(!text.contains("CafeWifi"), "network leaked into config: {text}");
}

#[test]
fn detect_without_wifi_adapter_errors() {
    let sb = Sandbox::new();
    let o = sb.cmd(&["detect"]);
    assert!(!o.status.success());
    assert!(stderr(&o).contains("could not detect"), "stderr: {}", stderr(&o));
}

#[test]
fn doctor_reports_missing_nmcli_in_sandbox() {
    let sb = Sandbox::new();
    let o = sb.cmd(&["doctor"]);
    assert!(o.status.success(), "stderr: {}", stderr(&o));
    assert!(stdout(&o).contains("MISSING"));
}

#[test]
fn status_runs_without_crashing_when_offline() {
    let sb = Sandbox::new();
    let o = sb.cmd(&["status"]);
    // No adapter/internet in the sandbox, so this reports unhealthy (exit 1)
    // rather than crashing.
    assert_eq!(o.status.code(), Some(1));
    let out = stdout(&o);
    assert!(out.contains("breadcrumbs"));
    assert!(out.contains("needs attention"));
}

#[test]
fn cd_prints_the_config_directory() {
    let sb = Sandbox::new();
    let o = sb.cmd(&["cd"]);
    assert!(o.status.success());
    let printed = PathBuf::from(stdout(&o).trim());
    assert_eq!(printed, sb.root.join("config/breadcrumbs"));
}

#[test]
fn install_service_no_enable_writes_valid_unit_file() {
    let sb = Sandbox::new();
    let o = sb.cmd(&["install-service", "--no-enable"]);
    assert!(o.status.success(), "stderr: {}", stderr(&o));

    let unit_path = sb.root.join(".config/systemd/user/breadcrumbs.service");
    assert!(unit_path.exists());
    let text = fs::read_to_string(unit_path).unwrap();
    assert!(text.contains("ExecStart="));
    assert!(text.contains("breadcrumbs watch"));
    assert!(text.contains("[Install]"));
    assert!(text.contains("WantedBy=default.target"));
}

#[test]
fn edit_invokes_editor_then_validates_config() {
    let sb = Sandbox::new();
    sb.write_fake_bin("fake-editor", "#!/bin/sh\nexit 0\n");

    let o = sb.cmd_env(&["edit"], &[("EDITOR", "fake-editor")]);
    assert!(o.status.success(), "stderr: {}", stderr(&o));
    assert!(stdout(&o).contains("config OK"));
    assert!(sb.config_file().exists());
}

#[test]
fn edit_reports_editor_failure() {
    let sb = Sandbox::new();
    sb.write_fake_bin("fake-editor", "#!/bin/sh\nexit 1\n");

    let o = sb.cmd_env(&["edit"], &[("EDITOR", "fake-editor")]);
    assert!(!o.status.success());
    assert!(stderr(&o).contains("editor exited with error"));
}

// -----------------------------------------------------------------------
// Split secrets file (item 5)
// -----------------------------------------------------------------------

#[test]
fn networks_are_stored_separately_from_settings_and_profiles() {
    let sb = Sandbox::new();
    let o = sb.cmd(&["add", "CafeWifi", "hunter2", "--to", "away"]);
    assert!(o.status.success(), "stderr: {}", stderr(&o));

    let settings = fs::read_to_string(sb.config_file()).unwrap();
    assert!(
        !settings.contains("hunter2") && !settings.contains("[[networks]]"),
        "breadcrumbs.toml should hold settings/profiles only, not network credentials: {settings}"
    );
    // The profile's priority list (an SSID *reference*, not a credential)
    // does still live in breadcrumbs.toml — that's expected and fine.
    assert!(settings.contains("[profiles.away]"));
    assert!(settings.contains("CafeWifi"));

    let networks = fs::read_to_string(sb.networks_file()).unwrap();
    assert!(networks.contains("CafeWifi") && networks.contains("hunter2"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(sb.networks_file()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "networks.toml should be owner-only");
    }
}

#[test]
fn add_with_empty_password_is_stored_as_no_password() {
    // An explicitly empty password (e.g. `add SSID ""`, or a blank response
    // at the interactive prompt) means "this is an open network" — it must
    // round-trip as an absent `password` key, the same as a cleared one,
    // not as `password = ""` (which `nm::connect_verbose` would send as a
    // literal empty PSK and fail against a real open SSID).
    let sb = Sandbox::new();
    let o = sb.cmd(&["add", "OpenCafe", ""]);
    assert!(o.status.success(), "stderr: {}", stderr(&o));

    let networks = fs::read_to_string(sb.networks_file()).unwrap();
    assert!(networks.contains("OpenCafe"));
    assert!(!networks.contains("password"), "networks.toml: {networks}");

    let list = sb.cmd(&["list"]);
    assert!(stdout(&list).contains("managed by NetworkManager"));
}

// -----------------------------------------------------------------------
// NM-owned credentials (item 4): a password is only ever needed once.
// -----------------------------------------------------------------------

/// A fake `nmcli` that behaves statefully enough to exercise the "first
/// connect creates a profile with a password, second connect reuses it
/// without one" path: it records every invocation's argv (one per line) to
/// `$HOME/.nmcli-calls`, and remembers — via a marker file, also under
/// `$HOME` — that "device wifi connect TestNet" has already run, so a
/// following `connection show` reports a saved profile exists.
const FAKE_NMCLI_STATEFUL: &str = r#"#!/bin/sh
record="$HOME/.nmcli-calls"
marker="$HOME/.nmcli-profile-created"
echo "$@" >> "$record"
args="$*"
case "$args" in
  "-t -f DEVICE,TYPE device status")
    echo "wlan0:wifi" ;;
  "radio wifi on") ;;
  "device wifi rescan"*) ;;
  "-t -f SSID device wifi list ifname wlan0")
    echo "TestNet" ;;
  "-t -f NAME,TYPE connection show")
    if [ -f "$marker" ]; then
      echo "TestNet:802-11-wireless"
    fi
    ;;
  *"device wifi connect TestNet"*)
    # `: > file` (truncate-or-create via a shell builtin + redirection) —
    # not `touch`, which is an external binary and the sandbox's PATH
    # deliberately contains nothing but this fake nmcli itself.
    : > "$marker" ;;
  *"connection up TestNet"*) ;;
  *"802-11-wireless-security.psk"*) ;;
  "-g GENERAL.CON-UUID device show wlan0")
    echo "uuid-1" ;;
  *"ipv4.ignore-auto-dns"*) ;;
  "device reapply wlan0") ;;
  "-t -f DEVICE,STATE device status")
    echo "wlan0:connected" ;;
  *) ;;
esac
exit 0
"#;

#[test]
fn password_is_cleared_after_first_connect_and_never_sent_again() {
    let sb = Sandbox::new();
    sb.write_fake_bin("nmcli", FAKE_NMCLI_STATEFUL);

    let add = sb.cmd(&["add", "TestNet", "hunter2"]);
    assert!(add.status.success(), "stderr: {}", stderr(&add));
    // "away" is the default profile and defaults to include_all_known, so
    // TestNet is already a connect candidate with no `--to` needed.

    let record = sb.root.join(".nmcli-calls");

    // First connect: no saved NM profile yet, so breadcrumbs creates one via
    // `device wifi connect ... password hunter2 ...`.
    let first = sb.cmd(&["init"]);
    assert!(first.status.success(), "stderr: {}", stderr(&first));
    let first_calls = fs::read_to_string(&record).unwrap_or_default();
    assert!(
        first_calls.contains("device wifi connect TestNet") && first_calls.contains("hunter2"),
        "first connect should create a new NM profile with the password: {first_calls}"
    );

    // The local copy is gone from disk immediately after.
    let networks = fs::read_to_string(sb.networks_file()).unwrap();
    assert!(
        !networks.contains("hunter2"),
        "password should have been cleared from networks.toml: {networks}"
    );
    assert!(networks.contains("TestNet"), "network entry itself should remain");

    // Reset the recording so the second run's argv can be checked in isolation.
    fs::write(&record, "").unwrap();

    // Second connect: a saved profile now exists (per the fake nmcli's own
    // bookkeeping) and breadcrumbs has no local password anymore, so it must
    // reuse the profile via `connection up` and never send a PSK argument.
    let second = sb.cmd(&["init"]);
    assert!(second.status.success(), "stderr: {}", stderr(&second));
    let second_calls = fs::read_to_string(&record).unwrap_or_default();
    assert!(
        second_calls.contains("connection up TestNet"),
        "second connect should reuse the existing NM profile: {second_calls}"
    );
    assert!(
        !second_calls.to_lowercase().contains("hunter2")
            && !second_calls.contains("psk")
            && !second_calls.contains("password"),
        "second connect must never send a password argument: {second_calls}"
    );
}

// -----------------------------------------------------------------------
// CLI-level coverage through fake nmcli/tailscale (item 3)
// -----------------------------------------------------------------------

const FAKE_NMCLI_HEALTHY: &str = r#"#!/bin/sh
args="$*"
case "$args" in
  "-t -f DEVICE,TYPE device status")
    echo "wlan0:wifi" ;;
  "-t -f ACTIVE,SSID device wifi list ifname wlan0")
    echo "yes:HomeWifi" ;;
  "-g IP4.ADDRESS device show wlan0")
    echo "192.168.1.50/24" ;;
  *) ;;
esac
exit 0
"#;

#[test]
fn status_reports_healthy_through_fake_nmcli_and_curl() {
    let sb = Sandbox::new();
    sb.write_fake_bin("nmcli", FAKE_NMCLI_HEALTHY);
    sb.write_fake_bin("curl", "#!/bin/sh\necho -n 204\nexit 0\n");

    let o = sb.cmd(&["status"]);
    assert!(o.status.success(), "stderr: {}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains("HomeWifi"), "out: {out}");
    assert!(out.contains("healthy"), "out: {out}");
}

#[test]
fn doctor_reports_present_when_nmcli_and_tailscale_are_on_path() {
    let sb = Sandbox::new();
    sb.write_fake_bin("nmcli", FAKE_NMCLI_HEALTHY);
    sb.write_fake_bin("tailscale", "#!/bin/sh\nexit 0\n");

    let o = sb.cmd(&["doctor"]);
    assert!(o.status.success(), "stderr: {}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains("nmcli") && out.contains("present"), "out: {out}");
    assert!(!out.contains("MISSING"), "out: {out}");
}

const FAKE_NMCLI_DETECT: &str = r#"#!/bin/sh
args="$*"
case "$args" in
  "-t -f DEVICE,TYPE device status")
    echo "wlan0:wifi" ;;
  "radio wifi on") ;;
  "device wifi rescan"*) ;;
  "-t -f SSID device wifi list ifname wlan0")
    echo "CorpWifi" ;;
  *) ;;
esac
exit 0
"#;

#[test]
fn detect_picks_profile_whose_detect_ssids_are_visible() {
    let sb = Sandbox::new();
    sb.write_fake_bin("nmcli", FAKE_NMCLI_DETECT);
    sb.cmd(&["list"]); // bootstrap the default config (home/work/away)

    // Attach a marker SSID to "work" so detection has something to match —
    // the skeleton config ships with empty detect_ssids everywhere.
    let text = fs::read_to_string(sb.config_file()).unwrap();
    let patched = text.replace(
        "[profiles.work]",
        "[profiles.work]\ndetect_ssids = [\"CorpWifi\"]",
    );
    fs::write(sb.config_file(), patched).unwrap();

    let o = sb.cmd(&["detect"]);
    assert!(o.status.success(), "stderr: {}", stderr(&o));
    assert_eq!(stdout(&o).trim(), "work");
}
