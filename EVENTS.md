# breadcrumbs — bread event integration

breadcrumbs is a standalone Wi-Fi state machine: it works exactly the same
with or without `breadd` running. When breadd *is* present **and** the
`breadcrumbs watch` daemon (or the systemd user service it installs) is up,
breadcrumbs publishes events into the shared bread automation fabric and
listens for a small set of commands. See the parent `bread` repo's
`Documentation.md` — specifically its "Namespaces" and "Integrating a
bread\* app" sections — for the general convention this follows.

App id: **`crumbs`**. Transport: `bread-utils`'s `bread_client` module
(feature `bread-client`) — the watch process links it directly, since it's
the long-running piece that both emits on real transitions and holds the
command subscription open.

One-shot CLI invocations (`breadcrumbs status`, `profile set`, `init`, …)
do **not** emit or subscribe on their own. A `breadcrumbs profile set home`
while the watcher is running is picked up on the watcher's next tick
(state is re-read every loop) and *then* published as
`bread.crumbs.profile.changed`. If the watcher is not running, the CLI
still switches the profile on disk — there is just nobody listening for
`bread.command.crumbs.*`, and nobody emitting `bread.crumbs.*`.

## Events published (`bread.crumbs.*`)

| Event | Data | When |
|-------|------|------|
| `bread.crumbs.profile.changed` | `{ "from": "<name>", "to": "<name>" }` | The watch loop observes that the persisted active profile is no longer the one it last acted on (CLI `profile set`, `detect --apply`, `bread.command.crumbs.set_profile`, or a time-of-day schedule switch). Not emitted on watcher start just because a profile is already selected. |
| `bread.crumbs.health.changed` | `{ "profile": "<name>", "health": "<variant>", "ssid": <string or null>, "iface": <string or null>, "ip": <string or null>, "exit_node": "<string>", "tailscale": <variant or null> }` | The watch loop's health classification changes — including the first observation after start, and the forced re-evaluation after a profile change. **Not** emitted on every poll tick while the classification stays the same. |
| `bread.crumbs.network.changed` | `{ "from": <ssid or null>, "to": <ssid or null>, "profile": "<name>" }` | The active SSID changed between watch-loop ticks. `from` is `null` on the first association observed after start (or after a profile switch). |
| `bread.crumbs.tailscale.changed` | `{ "profile": "<name>", "state": <variant or null>, "exit_node": "<string>" }` | The Tailscale health state (or its mere presence) changed between ticks. `state` is the `TsHealth` variant name or `null` when Tailscale isn't installed. |
| `bread.crumbs.set_profile.done` | `{ "profile": "<name>" }` | `bread.command.crumbs.set_profile` persisted the new profile. |
| `bread.crumbs.set_profile.failed` | `{ "error": "<message>" }` | `bread.command.crumbs.set_profile` was received but rejected (unknown profile, missing `profile` field, config unreadable). |

`health` is the Rust enum variant name, not a prettier label:

| Variant | Meaning |
|---------|---------|
| `Up` | Adapter present, internet reachable, Tailscale healthy if the profile requires it. |
| `DownNoNet` | No internet. |
| `CaptivePortal` | No internet and an HTTP response arrived that wasn't the 204 generate_204 returns — traffic is being intercepted (captive/guest portal). Needs a browser sign-in, not a reconnect. |
| `DownTailscaleManual` | Tailscale required but needs login / isn't installed — cannot auto-fix. |
| `DownTailscaleOther` | Tailscale required and unhealthy for some other (usually auto-recoverable) reason. |
| `NoAdapter` | No Wi-Fi interface. |
| `UnknownProfile` | Persisted profile name is not in the config. |

`ssid` is the currently-associated SSID, or `null` when there isn't one
(no adapter, not associated, unknown profile).

## Commands honored (`bread.command.crumbs.*`)

These are only received while `breadcrumbs watch` / `breadcrumbs.service`
is running. Publishing a command with no subscriber is a silent no-op —
that is the documented bread convention, not a breadcrumbs bug.

| Verb | Data | Effect |
|------|------|--------|
| `set_profile` | `{ "profile": "<name>" }` | Persist `<name>` via the same `state::set_profile` path the CLI uses. Wakes the watch loop immediately so the new profile is classified (and recovered, if down) on the next tick rather than waiting out the current poll interval. Does **not** run `flow::run` on the command thread — that would race the watch loop. Emits `bread.crumbs.set_profile.done`/`.failed`. |

### Not implemented: extra verbs

There is no `pin`, `select`, `scan`, `init`, or other command verb. The
CLI already covers those as synchronous one-shots (`breadcrumbs init`,
`breadcrumbs scan`, …), and breadcrumbs has no "pinned network" concept
to hang a bus verb on. If/when that changes, the corresponding
`bread.command.crumbs.*` verb should be added at the same time, not
stubbed out ahead of it.

## Fail-safe behavior

- If breadd isn't installed or isn't running, `emit` is a silent no-op
  (`BreadClient::emit` never blocks or errors the caller) and the
  command subscription simply never receives anything — breadcrumbs'
  actual Wi-Fi / Tailscale / watch functionality is entirely unaffected
  either way.
- If breadd restarts, the command subscription reconnects automatically
  (`BreadClient::subscribe`'s background thread has its own backoff loop);
  no restart of the breadcrumbs watcher is needed.
- If the breadcrumbs watcher is not running, commands are a graceful
  no-op at the bus (no subscriber) and no `bread.crumbs.*` events fire.
  The CLI still works.
