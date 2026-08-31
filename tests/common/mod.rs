//! Shared test infrastructure for in-process integration tests (as opposed
//! to `tests/cli.rs`'s black-box `Sandbox`, which spawns the compiled
//! binary). This module is `mod`-included by each test file that needs it —
//! see `tests/flow_watch.rs`.
//!
//! Two pieces:
//!
//! - [`FakeRunner`]: a `breadcrumbs::util::Runner` implementation driven by
//!   rules ("if the program+args match this predicate, return this canned
//!   `Output`"), which also records every invocation so a test can assert
//!   exactly what was — or, just as importantly, was *not* — passed (e.g.
//!   that a password argument never reaches a fake subprocess).
//! - [`EnvSandbox`]: real logic (`flow::run`, `watch::classify`) still does
//!   its own best-effort file logging via `notify::log`, which resolves a
//!   path from `$HOME`/`$XDG_STATE_HOME`. `EnvSandbox` points those at a
//!   throwaway tempdir for the duration of a test so nothing lands in the
//!   developer's real `~/.local/state/breadcrumbs`. Mutating process env is
//!   inherently cross-test-within-this-binary racy, so it's guarded by a
//!   process-wide mutex — tests using it serialize against each other but
//!   not against unrelated tests (each `tests/*.rs` file is its own binary).
//! - [`fake_nm`]: a real fake NetworkManager D-Bus service on a private
//!   `dbus-daemon`. The production `nm` module talks to it over real D-Bus
//!   marshalling (`Connection::system()` honors `DBUS_SYSTEM_BUS_ADDRESS`),
//!   replacing the old fake-`nmcli`-argv rules.

#![allow(dead_code)] // not every test file uses every helper here

pub mod fake_nm;

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use breadcrumbs::util::{Output, Runner};

/// One recorded call to the fake `Runner::run`.
#[derive(Debug, Clone)]
pub struct RecordedCall {
    pub prog: String,
    pub args: Vec<String>,
    pub stdin: Option<String>,
}

impl RecordedCall {
    /// Convenience for glob-style assertions, e.g.
    /// `call.argv().contains(&"connection")`.
    pub fn argv(&self) -> Vec<&str> {
        std::iter::once(self.prog.as_str())
            .chain(self.args.iter().map(String::as_str))
            .collect()
    }
}

type Matcher = Box<dyn Fn(&str, &[&str]) -> bool>;
type DynamicRule = (Matcher, Box<dyn Fn(&str, &[&str]) -> Output>);

/// A canned, rule-based [`Runner`]. Rules are tried in registration order;
/// the first whose matcher returns `true` supplies the response. No rule
/// matching falls back to [`Output::failed`] — the same "closed" default the
/// old empty-`PATH` sandbox relied on, so an un-anticipated call fails loud
/// (a wrong exit code) rather than silently returning success.
pub struct FakeRunner {
    rules: Vec<(Matcher, Output)>,
    dynamic_rules: Vec<DynamicRule>,
    commands: HashSet<String>,
    calls: Rc<RefCell<Vec<RecordedCall>>>,
}

impl FakeRunner {
    pub fn new() -> Self {
        FakeRunner {
            rules: Vec::new(),
            dynamic_rules: Vec::new(),
            commands: HashSet::new(),
            calls: Rc::new(RefCell::new(Vec::new())),
        }
    }

    /// Handle to inspect recorded calls after the runner has been consumed by
    /// [`breadcrumbs::util::with_runner`] (which takes it by value).
    pub fn calls_handle(&self) -> Rc<RefCell<Vec<RecordedCall>>> {
        self.calls.clone()
    }

    /// Make `breadcrumbs::util::command_exists(name)` report present.
    pub fn with_command(mut self, name: &str) -> Self {
        self.commands.insert(name.to_string());
        self
    }

    /// Register a canned response: the first registered matcher that returns
    /// `true` for a given `(prog, args)` supplies the `Output`.
    pub fn on(mut self, matcher: impl Fn(&str, &[&str]) -> bool + 'static, output: Output) -> Self {
        self.rules.push((Box::new(matcher), output));
        self
    }

    /// Shorthand for matching on `prog` plus a whitespace-joined view of
    /// `args` containing `substr` (handy for `tailscale`/`curl` calls, whose
    /// interesting bit is usually a subcommand somewhere in the middle).
    pub fn on_contains(self, prog: &'static str, substr: &'static str, output: Output) -> Self {
        self.on(
            move |p, args| p == prog && args.join(" ").contains(substr),
            output,
        )
    }

    /// Register a rule whose *response* is computed at call time (rather
    /// than canned), enabling stateful fakes — e.g. answering "which SSID is
    /// active?" with the SSID of the most recently dialed connection.
    /// Dynamic rules are tried after the static ones.
    pub fn on_dynamic(
        mut self,
        matcher: impl Fn(&str, &[&str]) -> bool + 'static,
        out: impl Fn(&str, &[&str]) -> Output + 'static,
    ) -> Self {
        self.dynamic_rules
            .push((Box::new(matcher), Box::new(out)));
        self
    }
}

impl Default for FakeRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl Runner for FakeRunner {
    fn run(&self, prog: &str, args: &[&str], stdin: Option<&str>, _timeout: Duration) -> Output {
        self.calls.borrow_mut().push(RecordedCall {
            prog: prog.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            stdin: stdin.map(|s| s.to_string()),
        });
        for (matcher, out) in &self.rules {
            if matcher(prog, args) {
                return out.clone();
            }
        }
        for (matcher, out) in &self.dynamic_rules {
            if matcher(prog, args) {
                return out(prog, args);
            }
        }
        Output::failed()
    }

    fn command_exists(&self, name: &str) -> bool {
        self.commands.contains(name)
    }
}

pub fn ok(stdout: &str) -> Output {
    Output {
        success: true,
        stdout: stdout.to_string(),
        stderr: String::new(),
    }
}

pub fn ok_empty() -> Output {
    ok("")
}

pub fn fail(stderr: &str) -> Output {
    Output {
        success: false,
        stdout: String::new(),
        stderr: stderr.to_string(),
    }
}

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

static SANDBOX_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Points `HOME` / `XDG_CONFIG_HOME` / `XDG_STATE_HOME` at a throwaway
/// tempdir for its lifetime, so any real filesystem side effect
/// (`notify::log`'s best-effort log file, `Config::save`, …) that in-process
/// logic performs during a test lands there instead of the developer's real
/// home directory. Holds a process-wide lock for its lifetime — construct
/// one per test, drop it (or let it go out of scope) before the test ends.
pub struct EnvSandbox {
    _guard: MutexGuard<'static, ()>,
    root: PathBuf,
    prev: Vec<(&'static str, Option<String>)>,
}

const ENV_VARS: [&str; 3] = ["HOME", "XDG_CONFIG_HOME", "XDG_STATE_HOME"];

impl EnvSandbox {
    pub fn new() -> Self {
        let guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());

        let n = SANDBOX_COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "breadcrumbs-inproc-{}-{}-{}",
            std::process::id(),
            n,
            nanos
        ));
        std::fs::create_dir_all(&root).expect("create EnvSandbox root");

        let prev: Vec<(&'static str, Option<String>)> = ENV_VARS
            .iter()
            .map(|v| (*v, std::env::var(v).ok()))
            .collect();
        std::env::set_var("HOME", &root);
        std::env::set_var("XDG_CONFIG_HOME", root.join("config"));
        std::env::set_var("XDG_STATE_HOME", root.join("state"));

        EnvSandbox {
            _guard: guard,
            root,
            prev,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl Default for EnvSandbox {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for EnvSandbox {
    fn drop(&mut self) {
        for (k, v) in &self.prev {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
