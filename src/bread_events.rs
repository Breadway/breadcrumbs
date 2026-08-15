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

pub fn emit_health_changed(client: &BreadClient, profile: &str, health: &str, ssid: Option<&str>) {
    client.emit(
        "bread.crumbs.health.changed",
        serde_json::json!({
            "profile": profile,
            "health": health,
            "ssid": ssid,
        }),
    );
}

/// Reacts to `bread.command.crumbs.*` verbs. Only `set_profile` maps to
/// real, existing breadcrumbs functionality today — there is no pin/select
/// (or other) verb because breadcrumbs has no such concept. Unrecognized
/// verbs are ignored, not stubbed as no-ops that pretend to succeed.
///
/// Returns `true` when a profile was actually persisted, so the watch loop
/// can wake immediately and re-evaluate instead of waiting out the current
/// poll interval.
///
/// Emits `bread.crumbs.set_profile.done`/`.failed` per the confirmation
/// convention in bread's Documentation.md.
pub fn handle_command(event: &BreadEvent) -> bool {
    let Some(verb) = event.event.strip_prefix("bread.command.crumbs.") else {
        return false;
    };
    match verb {
        "set_profile" => handle_set_profile(event),
        other => {
            crate::notify::log(&format!(
                "watch: ignoring unrecognized bread.command.crumbs.{other}"
            ));
            false
        }
    }
}

fn handle_set_profile(event: &BreadEvent) -> bool {
    let Some(name) = event.data.get("profile").and_then(|v| v.as_str()) else {
        emit_set_profile_failed("missing string \"profile\" in command data");
        return false;
    };
    match Config::load().and_then(|cfg| state::set_profile(&cfg, name)) {
        Ok(()) => {
            crate::notify::log(&format!(
                "watch: profile set via bread.command.crumbs.set_profile -> {name}"
            ));
            client().emit(
                "bread.crumbs.set_profile.done",
                serde_json::json!({ "profile": name }),
            );
            true
        }
        Err(e) => {
            emit_set_profile_failed(&e);
            false
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
