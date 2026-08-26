//! `bread.crumbs.*` event integration — optional, non-blocking. See
//! `EVENTS.md` at the repo root for the full contract. breadcrumbs works
//! identically with or without breadd running; every call here is
//! fire-and-forget (`BreadClient::emit` never blocks or errors this
//! process) so a missing or restarting breadd never affects Wi-Fi
//! automation itself.

use bread_utils::bread_client::{BreadClient, BreadEvent};

use crate::config::Config;
use crate::state;

/// This app's id in bread's sibling-app namespace registry
/// (`bread_shared::apps::KNOWN_APPS`) — events publish as `bread.crumbs.*`,
/// commands arrive on `bread.command.crumbs.*`.
pub const APP_ID: &str = "crumbs";

pub fn client() -> BreadClient {
    BreadClient::connect(APP_ID)
}

pub fn emit_profile_changed(client: &BreadClient, from: &str, to: &str) {
    client.emit(
        "bread.crumbs.profile.changed",
        serde_json::json!({ "from": from, "to": to }),
    );
}

/// Payload for `bread.crumbs.health.changed`. Constructed by the watch
/// loop from a classification and passed as a unit so the emit functions
/// stay small.
pub struct HealthChanged<'a> {
    pub profile: &'a str,
    pub health: &'a str,
    pub ssid: Option<&'a str>,
    pub iface: Option<&'a str>,
    pub ip: Option<&'a str>,
    pub exit_node: &'a str,
    pub tailscale: Option<&'a str>,
}

pub fn emit_health_changed(client: &BreadClient, ev: HealthChanged<'_>) {
    client.emit(
        "bread.crumbs.health.changed",
        serde_json::json!({
            "profile": ev.profile,
            "health": ev.health,
            "ssid": ev.ssid,
            "iface": ev.iface,
            "ip": ev.ip,
            "exit_node": ev.exit_node,
            "tailscale": ev.tailscale,
        }),
    );
}

/// `bread.crumbs.network.changed` — the watch loop observed the active SSID
/// transition. `from` is `null` when there was no previous association.
pub fn emit_network_changed(client: &BreadClient, from: Option<&str>, to: Option<&str>, profile: &str) {
    client.emit(
        "bread.crumbs.network.changed",
        serde_json::json!({ "from": from, "to": to, "profile": profile }),
    );
}

/// `bread.crumbs.tailscale.changed` — the Tailscale health state (or its
/// mere presence) changed between watch-loop ticks.
pub fn emit_tailscale_changed(
    client: &BreadClient,
    profile: &str,
    state: Option<&str>,
    exit_node: &str,
) {
    client.emit(
        "bread.crumbs.tailscale.changed",
        serde_json::json!({
            "profile": profile,
            "state": state,
            "exit_node": exit_node,
        }),
    );
}

/// What a `bread.command.crumbs.*` event asks the watch loop to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandAction {
    /// Persist this profile via [`state::set_profile`]. Applied on the
    /// watch loop thread — the single owner of config/state file access —
    /// never on the subscription thread, which would race the loop's own
    /// `Config::load`/`save`.
    SetProfile(String),
    /// Nothing to do: unknown verb, or a validation failure already
    /// reported via `bread.crumbs.set_profile.failed`.
    Ignore,
}

/// Reacts to `bread.command.crumbs.*` verbs. Only `set_profile` maps to
/// real, existing breadcrumbs functionality today — there is no pin/select
/// (or other) verb because breadcrumbs has no such concept. Unrecognized
/// verbs are ignored, not stubbed as no-ops that pretend to succeed.
///
/// This only *parses and validates* the command — it performs no file I/O
/// (that would race the watch loop's own config access from a second
/// thread). The returned [`CommandAction`] is forwarded to the loop, which
/// applies it via [`apply_set_profile`].
pub fn handle_command(event: &BreadEvent) -> CommandAction {
    let Some(verb) = event.event.strip_prefix("bread.command.crumbs.") else {
        return CommandAction::Ignore;
    };
    match verb {
        "set_profile" => match event.data.get("profile").and_then(|v| v.as_str()) {
            Some(name) if !name.trim().is_empty() => CommandAction::SetProfile(name.to_string()),
            _ => {
                emit_set_profile_failed("missing string \"profile\" in command data");
                CommandAction::Ignore
            }
        },
        other => {
            crate::notify::log(&format!(
                "watch: ignoring unrecognized bread.command.crumbs.{other}"
            ));
            CommandAction::Ignore
        }
    }
}

/// Apply a `set_profile` command on the watch loop thread and emit the
/// `done`/`failed` confirmation. Kept separate from [`handle_command`] so
/// the bread subscription thread never touches config/state files
/// concurrently with the loop.
pub fn apply_set_profile(name: &str) {
    match Config::load().and_then(|cfg| state::set_profile(&cfg, name)) {
        Ok(()) => {
            crate::notify::log(&format!(
                "watch: profile set via bread.command.crumbs.set_profile -> {name}"
            ));
            client().emit(
                "bread.crumbs.set_profile.done",
                serde_json::json!({ "profile": name }),
            );
        }
        Err(e) => {
            emit_set_profile_failed(&e);
        }
    }
}

fn emit_set_profile_failed(error: &str) {
    crate::notify::log(&format!(
        "watch: bread.command.crumbs.set_profile failed: {error}"
    ));
    client().emit(
        "bread.crumbs.set_profile.failed",
        serde_json::json!({ "error": error }),
    );
}
