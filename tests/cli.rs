//! End-to-end CLI tests. Each run is fully isolated: HOME / XDG dirs point at a
//! throwaway tempdir, PATH is emptied so no real `tailscale`/`curl`/`date` is
//! ever invoked, and a private `dbus-daemon` (optionally hosting a fake
//! NetworkManager service — see `tests/common::fake_nm`) stands in for the
//! system bus so the binary's D-Bus calls never touch the host.

mod common;

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use common::fake_nm::{self, Security};

const BIN: &str = env!("CARGO_BIN_EXE_breadcrumbs");

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Sandbox {
    root: PathBuf,
    _bus: fake_nm::Daemon,
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
        Sandbox {
            root,
            _bus: fake_nm::launch_daemon(),
        }
    }

    /// Attach the fake NetworkManager service to this sandbox's bus and
    /// return a handle for driving its state.
    fn nm(&self) -> fake_nm::FakeNmBus {
        fake_nm::serve_on(&self._bus.addr)
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
            .env("PATH", self.root.join("bin"))
            // Point the binary's `Connection::system()` at this test's
            // private bus so it never touches a real system bus.
            .env("DBUS_SYSTEM_BUS_ADDRESS", &self._bus.addr);
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
    /// test can stand in for an external command (e.g. `$EDITOR`, `curl`).
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

    // `set --no-apply` must not touch the network (no NM on the bus anyway).
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
fn doctor_reports_missing_network_manager_on_private_bus() {
    // The sandbox bus has no NetworkManager service on it, so doctor must
    // report it missing rather than assuming presence.
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

    // The sandbox sets XDG_CONFIG_HOME=$root/config, so the unit lands
    // under $XDG_CONFIG_HOME/systemd/user — the whole point of the fix is
    // honoring XDG rather than hardcoding ~/.config.
    let unit_path = sb.root.join("config/systemd/user/breadcrumbs.service");
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

#[test]
fn password_is_cleared_after_first_connect_and_never_sent_again() {
    // The first connect creates an NM profile carrying the PSK (over D-Bus,
    // never in argv); the second connect reuses that profile and must not
    // create a duplicate or resend the password.
    let sb = Sandbox::new();
    let nm = sb.nm();
    let dev = nm.add_wifi_device("wlan0", 100);
    nm.add_ap(&dev, "TestNet", 80, Security::Wpa2);

    let add = sb.cmd(&["add", "TestNet", "hunter2"]);
    assert!(add.status.success(), "stderr: {}", stderr(&add));
    // "away" is the default profile and defaults to include_all_known, so
    // TestNet is already a connect candidate with no `--to` needed.

    // First connect: no saved NM profile yet, so breadcrumbs creates one
    // with the password.
    let first = sb.cmd(&["init"]);
    assert!(first.status.success(), "stderr: {}", stderr(&first));

    // The local copy is gone from disk immediately after.
    let networks = fs::read_to_string(sb.networks_file()).unwrap();
    assert!(
        !networks.contains("hunter2"),
        "password should have been cleared from networks.toml: {networks}"
    );
    assert!(networks.contains("TestNet"), "network entry itself should remain");

    // The NM profile durably holds the secret.
    let psk = {
        let st = nm.state.lock().unwrap();
        st.connections
            .values()
            .filter_map(|s| {
                let sec = s.get("802-11-wireless-security")?;
                sec.get("psk").and_then(|v| v.downcast_ref::<String>().ok())
            })
            .next()
    };
    assert_eq!(psk.as_deref(), Some("hunter2"), "NM profile must hold the PSK");

    // Second connect: breadcrumbs has no local password anymore, so it must
    // reuse the existing profile without creating a duplicate.
    let second = sb.cmd(&["init"]);
    assert!(second.status.success(), "stderr: {}", stderr(&second));
    assert_eq!(
        nm.connection_count(),
        1,
        "reuse must not accumulate duplicate NM profiles"
    );
}

#[test]
fn join_connects_a_saved_network_without_a_password_argument() {
    // `join` must work from config alone — no --password/positional arg
    // exists on it at all, so there's nothing to check off argv here; this
    // just proves the command actually activates the network.
    let sb = Sandbox::new();
    let nm = sb.nm();
    let dev = nm.add_wifi_device("wlan0", 100);
    nm.add_ap(&dev, "TestNet", 80, Security::Wpa2);

    let add = sb.cmd(&["add", "TestNet", "hunter2"]);
    assert!(add.status.success(), "stderr: {}", stderr(&add));

    let join = sb.cmd(&["join", "TestNet"]);
    assert!(join.status.success(), "stderr: {}", stderr(&join));
    assert!(stdout(&join).contains("connected"));

    let psk = {
        let st = nm.state.lock().unwrap();
        st.connections
            .values()
            .filter_map(|s| {
                let sec = s.get("802-11-wireless-security")?;
                sec.get("psk").and_then(|v| v.downcast_ref::<String>().ok())
            })
            .next()
    };
    assert_eq!(psk.as_deref(), Some("hunter2"), "NM profile must hold the PSK");

    let networks = fs::read_to_string(sb.networks_file()).unwrap();
    assert!(
        !networks.contains("hunter2"),
        "password should have been cleared from networks.toml: {networks}"
    );
}

#[test]
fn join_unknown_ssid_errors() {
    let sb = Sandbox::new();
    let join = sb.cmd(&["join", "NeverSaved"]);
    assert!(!join.status.success());
    assert!(stderr(&join).contains("no saved network"), "stderr: {}", stderr(&join));
}

// -----------------------------------------------------------------------
// CLI-level coverage through fake NM + tailscale (item 3)
// -----------------------------------------------------------------------

#[test]
fn status_reports_healthy_through_fake_nm_and_curl() {
    let sb = Sandbox::new();
    let nm = sb.nm();
    let dev = nm.add_wifi_device("wlan0", 100);
    let ap = nm.add_ap(&dev, "HomeWifi", 80, Security::Wpa2);
    nm.set_active_ap(&dev, &ap);
    sb.write_fake_bin("curl", "#!/bin/sh\necho -n 204\nexit 0\n");

    let o = sb.cmd(&["status"]);
    assert!(o.status.success(), "stderr: {}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains("HomeWifi"), "out: {out}");
    assert!(out.contains("healthy"), "out: {out}");
}

#[test]
fn doctor_reports_present_when_nm_and_tailscale_are_available() {
    let sb = Sandbox::new();
    let _nm = sb.nm(); // attach the fake NM; keep the handle alive for the run
    sb.write_fake_bin("tailscale", "#!/bin/sh\nexit 0\n");

    let o = sb.cmd(&["doctor"]);
    assert!(o.status.success(), "stderr: {}", stderr(&o));
    let out = stdout(&o);
    assert!(
        out.contains("network-manager") && out.contains("present"),
        "out: {out}"
    );
    assert!(!out.contains("MISSING"), "out: {out}");
}

#[test]
fn detect_picks_profile_whose_detect_ssids_are_visible() {
    let sb = Sandbox::new();
    let nm = sb.nm();
    let dev = nm.add_wifi_device("wlan0", 100);
    nm.add_ap(&dev, "CorpWifi", 80, Security::Wpa2);
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

// -----------------------------------------------------------------------
// Regression tests for the audit fixes (XDG paths, EDITOR args, config
// merging/clamping, core-profile ownership, scan validation).
// -----------------------------------------------------------------------

#[test]
fn edit_splits_editor_arguments() {
    // EDITOR="code -w" style values must be split into program + args
    // instead of being treated as one (nonexistent) binary path.
    let sb = Sandbox::new();
    sb.write_fake_bin(
        "fake-editor",
        "#!/bin/sh\necho \"$@\" > \"$HOME/editor-args\"\nexit 0\n",
    );

    let o = sb.cmd_env(&["edit"], &[("EDITOR", "fake-editor --wait")]);
    assert!(o.status.success(), "stderr: {}", stderr(&o));
    assert!(stdout(&o).contains("config OK"));
    let args = fs::read_to_string(sb.root.join("editor-args")).unwrap();
    assert!(args.contains("--wait"), "editor args must be split off: {args}");
    assert!(
        args.contains("breadcrumbs.toml"),
        "config path must be appended as its own argument: {args}"
    );
}

#[test]
fn scan_to_unknown_profile_errors_like_add() {
    // `scan --to bogus` must fail up front, matching `add --to`, instead of
    // silently saving a network that never gets attached.
    let sb = Sandbox::new();
    sb.cmd(&["list"]); // bootstrap the config
    let o = sb.cmd(&["scan", "--to", "bogus"]);
    assert!(!o.status.success());
    assert!(
        stderr(&o).contains("unknown profile 'bogus'"),
        "stderr: {}",
        stderr(&o)
    );
}

#[test]
fn watch_interval_below_minimum_is_clamped_to_four() {
    // `list` and the watch loop must agree on the poll interval: a value
    // below the documented minimum of 4 is clamped at load, not just
    // silently clamped inside the watch loop.
    let sb = Sandbox::new();
    fs::create_dir_all(sb.root.join("config/breadcrumbs")).unwrap();
    fs::write(sb.config_file(), "[settings]\nwatch_interval = 1\n").unwrap();

    let o = sb.cmd(&["list"]);
    assert!(o.status.success(), "stderr: {}", stderr(&o));
    assert!(
        stdout(&o).contains("watch every 4s"),
        "watch interval must clamp to the minimum: {}",
        stdout(&o)
    );
}

#[test]
fn legacy_inline_networks_merge_with_networks_toml_instead_of_dropping() {
    // A breadcrumbs.toml still carrying a legacy inline `[[networks]]` block
    // must keep those networks even when networks.toml already exists — the
    // merge is completed (and the inline block dropped) on the next save.
    let sb = Sandbox::new();
    fs::create_dir_all(sb.root.join("config/breadcrumbs")).unwrap();
    fs::write(
        sb.config_file(),
        "[settings]\ndefault_profile = \"away\"\n\n[[networks]]\nssid = \"InlineNet\"\npassword = \"pw-inline\"\n",
    )
    .unwrap();
    fs::write(
        sb.networks_file(),
        "[[networks]]\nssid = \"FileNet\"\npassword = \"pw-file\"\n",
    )
    .unwrap();

    let o = sb.cmd(&["list"]);
    assert!(o.status.success(), "stderr: {}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains("InlineNet"), "inline network must survive the merge: {out}");
    assert!(out.contains("FileNet"), "networks.toml network must be present: {out}");

    // A later save migrates the merged set into networks.toml and drops the
    // inline block from breadcrumbs.toml.
    let o2 = sb.cmd(&["add", "OtherNet", "pw3"]);
    assert!(o2.status.success(), "stderr: {}", stderr(&o2));
    let networks = fs::read_to_string(sb.networks_file()).unwrap();
    assert!(
        networks.contains("InlineNet")
            && networks.contains("FileNet")
            && networks.contains("OtherNet"),
        "save must persist the merged set: {networks}"
    );
    let config_text = fs::read_to_string(sb.config_file()).unwrap();
    assert!(
        !config_text.contains("[[networks]]"),
        "inline block should be gone after migration: {config_text}"
    );
}

#[test]
fn core_profiles_are_not_resurrected_once_config_is_user_owned() {
    // After the first save the config is user-owned: a deliberately deleted
    // core profile must stay deleted.
    let sb = Sandbox::new();
    fs::create_dir_all(sb.root.join("config/breadcrumbs")).unwrap();
    fs::write(
        sb.config_file(),
        "[settings]\ncore_profiles_initialized = true\n",
    )
    .unwrap();

    let o = sb.cmd(&["profile", "list"]);
    assert!(o.status.success(), "stderr: {}", stderr(&o));
    let out = stdout(&o);
    assert!(
        !out.contains("home") && !out.contains("work") && !out.contains("away"),
        "deleted core profiles must stay deleted: {out}"
    );
}

#[test]
fn legacy_config_without_profiles_gets_core_profiles_backfilled() {
    // Pre-ownership configs (no flag yet) still get the core profiles
    // backfilled once — the self-heal that makes bare `[settings]` configs
    // usable.
    let sb = Sandbox::new();
    fs::create_dir_all(sb.root.join("config/breadcrumbs")).unwrap();
    fs::write(sb.config_file(), "[settings]\n").unwrap();

    let o = sb.cmd(&["profile", "list"]);
    assert!(o.status.success(), "stderr: {}", stderr(&o));
    let out = stdout(&o);
    assert!(
        out.contains("home") && out.contains("work") && out.contains("away"),
        "legacy configs get the core profiles backfilled once: {out}"
    );
}

// -----------------------------------------------------------------------
// New features: per-network DNS, enterprise (802.1x) networks, --json
// output, prune, scored detection, and init --wait retry.
// -----------------------------------------------------------------------

#[test]
fn add_with_dns_persists_per_network_override() {
    let sb = Sandbox::new();
    let o = sb.cmd(&["add", "CafeWifi", "pw", "--dns", "9.9.9.9"]);
    assert!(o.status.success(), "stderr: {}", stderr(&o));
    let networks = fs::read_to_string(sb.networks_file()).unwrap();
    assert!(
        networks.contains("dns = \"9.9.9.9\""),
        "per-network DNS override must be persisted: {networks}"
    );
}

#[test]
fn add_enterprise_fields_persist() {
    let sb = Sandbox::new();
    let o = sb.cmd(&[
        "add",
        "CorpWifi",
        "pw",
        "--eap",
        "peap",
        "--identity",
        "user@corp",
        "--ca-cert",
        "/etc/ca.pem",
    ]);
    assert!(o.status.success(), "stderr: {}", stderr(&o));
    let networks = fs::read_to_string(sb.networks_file()).unwrap();
    assert!(networks.contains("eap = \"peap\""), "networks: {networks}");
    assert!(networks.contains("identity = \"user@corp\""));
    assert!(networks.contains("ca_cert = \"/etc/ca.pem\""));
}

#[test]
fn status_json_emits_machine_readable_output() {
    let sb = Sandbox::new();
    let o = sb.cmd(&["status", "--json"]);
    // No adapter in the sandbox → unhealthy (exit 1), but still valid JSON.
    assert_eq!(o.status.code(), Some(1));
    let v: serde_json::Value =
        serde_json::from_str(&stdout(&o)).expect("status --json must emit valid JSON");
    assert_eq!(v["profile"].as_str(), Some("away"));
    assert_eq!(v["healthy"].as_bool(), Some(false));
    assert_eq!(v["internet"].as_bool(), Some(false));
}

#[test]
fn detect_json_emits_machine_readable_output() {
    let sb = Sandbox::new();
    let nm = sb.nm();
    let dev = nm.add_wifi_device("wlan0", 100);
    nm.add_ap(&dev, "CorpWifi", 80, Security::Wpa2);
    sb.cmd(&["list"]); // bootstrap the default config

    let text = fs::read_to_string(sb.config_file()).unwrap();
    let patched = text.replace(
        "[profiles.work]",
        "[profiles.work]\ndetect_ssids = [\"CorpWifi\"]",
    );
    fs::write(sb.config_file(), patched).unwrap();

    let o = sb.cmd(&["detect", "--json"]);
    assert!(o.status.success(), "stderr: {}", stderr(&o));
    let v: serde_json::Value =
        serde_json::from_str(&stdout(&o)).expect("detect --json must emit valid JSON");
    assert_eq!(v["profile"].as_str(), Some("work"));
}

#[test]
fn detect_prefers_profile_with_more_matching_markers() {
    let sb = Sandbox::new();
    let nm = sb.nm();
    let dev = nm.add_wifi_device("wlan0", 100);
    nm.add_ap(&dev, "CorpWifi", 80, Security::Wpa2);
    nm.add_ap(&dev, "CafeWifi", 70, Security::Wpa2);
    sb.cmd(&["list"]); // bootstrap

    // home matches 1 marker (CorpWifi); work matches 2 (CorpWifi + CafeWifi).
    let text = fs::read_to_string(sb.config_file()).unwrap();
    let patched = text
        .replace(
            "[profiles.home]",
            "[profiles.home]\ndetect_ssids = [\"CorpWifi\"]",
        )
        .replace(
            "[profiles.work]",
            "[profiles.work]\ndetect_ssids = [\"CorpWifi\", \"CafeWifi\"]",
        );
    fs::write(sb.config_file(), patched).unwrap();

    let o = sb.cmd(&["detect"]);
    assert!(o.status.success(), "stderr: {}", stderr(&o));
    assert_eq!(
        stdout(&o).trim(),
        "work",
        "the profile with more matching markers must win"
    );
}

#[test]
fn prune_dry_run_lists_stale_nm_profiles() {
    let sb = Sandbox::new();
    let nm = sb.nm();
    nm.save_connection("OldCafe", None);
    sb.cmd(&["list"]); // bootstrap (no saved networks → everything is stale)

    let o = sb.cmd(&["prune", "--dry-run"]);
    assert!(o.status.success(), "stderr: {}", stderr(&o));
    let out = stdout(&o);
    assert!(
        out.contains("would remove") && out.contains("OldCafe"),
        "out: {out}"
    );
}

#[test]
fn prune_removes_stale_nm_profiles() {
    let sb = Sandbox::new();
    let nm = sb.nm();
    nm.save_connection("OldCafe", None);
    sb.cmd(&["list"]);

    let o = sb.cmd(&["prune"]);
    assert!(o.status.success(), "stderr: {}", stderr(&o));
    let out = stdout(&o);
    assert!(
        out.contains("removed") && out.contains("OldCafe"),
        "out: {out}"
    );
    assert_eq!(
        nm.connection_count(),
        0,
        "prune must actually delete the stale profile"
    );
}

#[test]
fn init_wait_retries_until_connect_succeeds() {
    let sb = Sandbox::new();
    let nm = sb.nm();
    let dev = nm.add_wifi_device("wlan0", 100);
    nm.add_ap(&dev, "HomeWifi", 80, Security::Wpa2);
    // "away" defaults to include_all_known, so HomeWifi is a candidate.
    let add = sb.cmd(&["add", "HomeWifi", "hunter2"]);
    assert!(add.status.success(), "stderr: {}", stderr(&add));

    // The fake's first activation fails; the retry succeeds. `--wait` must
    // keep going past the first failure rather than bailing.
    nm.fail_next_activations(1);
    let o = sb.cmd(&["init", "--wait", "5"]);
    assert!(o.status.success(), "stderr: {}", stderr(&o));
    assert!(stdout(&o).contains("connected"), "out: {}", stdout(&o));
}
