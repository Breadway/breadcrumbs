//! breadcrumbs library crate.
//!
//! All actual logic lives here; `src/main.rs` is a thin binary shim that
//! parses no arguments itself — it just calls [`app::run`]. Splitting things
//! this way means integration tests can link against `breadcrumbs` as an
//! ordinary library crate and drive the real state machine (`flow::run`,
//! `watch::classify`, …) in-process, instead of only being able to spawn the
//! compiled binary.

pub mod app;
pub mod config;
pub mod flow;
pub mod nm;
pub mod notify;
pub mod state;
pub mod status;
pub mod tailscale;
pub mod util;
pub mod watch;
