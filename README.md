# breadcrumbs

A profile-aware Wi-Fi state machine for Linux with Tailscale exit-node management and a self-healing watch daemon.

breadcrumbs sits on top of NetworkManager (`nmcli`) and manages your Wi-Fi based on **location profiles**. Switch between home, work, school, or any other context with a single command — it handles scanning, connecting, DNS pinning, and Tailscale setup automatically.

## Features

- **Profile-based connection management** — define ordered network priority lists per location
- **Bootstrap + Tailscale gating** — connect to an interim network first, bring up Tailscale, then move to the target network
- **Self-healing watch daemon** — monitors for drops, auto-recovers, reacts within seconds via `nmcli monitor`
- **Auto-detection** — scans visible SSIDs and guesses your location from config-defined markers
- **Credential handling** — a saved network's password is only needed the *first* time breadcrumbs connects to it. Once that connect succeeds, NetworkManager durably owns the credential (a new connection profile, or an updated PSK on an existing one), so breadcrumbs clears its own local copy and stops writing it to disk. Both config files are `0600` (owner-only); saved networks live in a separate `networks.toml` from settings/profiles (see [Configuration](#configuration)). On that first connect the PSK is fed to `nmcli --ask` on stdin, never as a command argument, so it does not appear in `/proc/<pid>/cmdline`.
- **Desktop notifications** via `notify-send` (optional)
- **systemd user service** generation via `breadcrumbs install-service`

## Requirements

- Linux with NetworkManager (`nmcli` in `$PATH`)
- Rust toolchain (to build from source)
- `tailscale` (optional — only needed if any profile sets `tailscale = true`)
- `notify-send` (optional — for desktop notifications)
- `curl` (optional — used for connectivity checks, falls back to `ping`)

## Installation

```bash
git clone https://github.com/breadway/breadcrumbs
cd breadcrumbs
cargo build --release
# Copy to somewhere on your PATH:
cp target/release/breadcrumbs ~/.local/bin/
```

## Configuration

On first run, breadcrumbs creates `~/.config/breadcrumbs/breadcrumbs.toml` (settings + profiles) and `~/.config/breadcrumbs/networks.toml` (saved networks) with default profiles. Copy `breadcrumbs.example.toml` as a starting point for the former:

```bash
cp breadcrumbs.example.toml ~/.config/breadcrumbs/breadcrumbs.toml
breadcrumbs edit   # opens breadcrumbs.toml in $EDITOR
```

...then add your real networks with `breadcrumbs add`/`scan` rather than hand-editing `networks.toml` (see `networks.example.toml` if you want to see its shape or write it by hand anyway).

Config paths respect `$XDG_CONFIG_HOME` and `$XDG_STATE_HOME`.

### Config structure

Settings and location profiles live in `breadcrumbs.toml` — the file people actually hand-edit or dotfile:

```toml
[settings]
dns = "1.1.1.1"          # DNS server pinned on every connection
nmcli_wait = 8           # seconds to wait for nmcli connect
exit_node = "myhostname" # default Tailscale exit node
exit_nodes = ["a", "b"] # optional priority list; tried in order (fallback nodes)
interface = "wlan0"      # optional preferred Wi-Fi interface
schedule = []            # optional time-of-day profile switches, e.g.
                         #   [[settings.schedule]]
                         #   profile = "home"
                         #   from = "18:00"
                         #   to = "08:00"   # from >= to = overnight window
default_profile = "away"
watch_interval = 12      # seconds between health checks (minimum 4)
connectivity_url = "http://connectivitycheck.gstatic.com/generate_204"
ping_host = "1.1.1.1"

[profiles.home]
networks = ["MyHomeNetwork"]  # priority-ordered SSIDs
tailscale = false
include_all_known = false
detect_ssids = ["MyHomeNetwork"]  # used by `breadcrumbs detect`

[profiles.work]
bootstrap = "GuestWifi"   # connect here first before requiring Tailscale
networks = ["CorpWifi"]
tailscale = true
exit_node = "jump-host"   # per-profile override
detect_ssids = ["CorpWifi", "Corp-5G"]
```

Saved networks (SSID + optional local password) live separately, in `networks.toml`, managed via `add`/`scan`/`forget`:

```toml
[[networks]]
ssid = "MyHomeNetwork"
password = "hunter2"  # optional — see "Credential handling" below
hidden = false
dns = "1.1.1.1"       # optional per-network DNS override; "" disables pinning

# WPA-Enterprise (802.1x) networks use these instead of a PSK:
# [[networks]]
# ssid = "CorpEAP"
# eap = "peap"          # or "tls"
# identity = "user@corp"
# password = "..."       # 802.1x password
# ca_cert = "/etc/ssl/certs/corp-ca.pem"   # optional
```

`password` is only needed the first time breadcrumbs connects to a network. Once NetworkManager durably saves the credential, breadcrumbs clears its local copy and omits the key on the next save — an existing config with `password = "..."` still loads fine either way, no migration step needed. A config with `[[networks]]` still written inline in `breadcrumbs.toml` (from before this split) also still loads: it's read once, then migrated into `networks.toml` automatically on the next save.

### Profiles

Each profile defines:

| Key | Description |
|-----|-------------|
| `networks` | Ordered list of SSIDs to try. First available wins. |
| `tailscale` | If `true`, Tailscale must be healthy before moving to a target network. |
| `bootstrap` | SSID to connect to first (e.g. guest Wi-Fi that allows Tailscale traffic). |
| `exit_node` | Tailscale exit node for this profile (overrides `settings.exit_node`). |
| `include_all_known` | After the priority list, also try every other known network. |
| `detect_ssids` | Any visible SSID in this list marks this profile as a candidate for `breadcrumbs detect`. Profiles with more matching markers win. |
| `learn` | If `true`, SSIDs this profile successfully connects to are appended to `detect_ssids` (bounded), so `detect` improves without hand-editing. Off by default. |

## Usage

```
breadcrumbs [--profile <name>] <command>
```

| Command | Description |
|---------|-------------|
| `status [--json]` | Show current Wi-Fi / Tailscale health (default) |
| `init [--wait <s>]` | Run the full connect sequence; `--wait` retries until connected or the timeout elapses |
| `watch [--no-initial]` | Self-healing daemon: monitors and auto-recovers drops |
| `profile get` | Print the active profile |
| `profile set <name>` | Switch profile (and apply it, unless `--no-apply`) |
| `profile list` | List all profiles |
| `detect [--apply] [--json]` | Guess profile from visible networks; optionally apply it |
| `add <ssid> [password]` | Add or update a saved network (`--dns`, `--eap`, `--identity`, `--ca-cert`, `--hidden`, `--to`, `--at`) |
| `forget <ssid>` | Remove a network from config and NetworkManager |
| `prune [--dry-run]` | Remove NetworkManager wireless profiles whose SSID is no longer in the config |
| `scan [--to <profile>]` | Interactive scan, pick, connect and save |
| `list [--show-passwords]` | Show config: settings, networks, profiles |
| `edit` | Open config in `$EDITOR`, validate on exit |
| `doctor [--full]` | Quick connectivity and Tailscale diagnostics |
| `cd [--shell]` | Print (or `cd` into) the config directory |
| `install-service [--no-enable]` | Install and optionally enable systemd user unit |

### Examples

```bash
# Check current state
breadcrumbs

# Switch to the "work" profile and connect
breadcrumbs profile set work

# Run as a daemon in the foreground (use install-service for persistent use)
breadcrumbs watch

# Override profile for one run without persisting
breadcrumbs --profile home init

# Add a new network and attach it to a profile
breadcrumbs add "CoffeeShop5G" --to away

# Detect and switch profile based on visible networks
breadcrumbs detect --apply

# Install and start the systemd watcher service
breadcrumbs install-service
```

## Watch daemon

`breadcrumbs watch` is the recommended way to run breadcrumbs for daily use. It:

1. Polls health every `watch_interval` seconds (adaptive backoff on repeated failures)
2. Reacts immediately to link-state changes via `nmcli monitor`
3. Runs `flow::run` (the connect state machine) on any detected drop
4. Handles profile changes live — re-reads config and state on every tick
5. Distinguishes captive portals from plain no-internet (a 200/301/302 instead
   of the 204 generate_204 returns) and tells you to sign in instead of
   pointlessly reconnecting
6. Applies a `[settings.schedule]` time-of-day profile switch, respecting a
   30-minute grace window after a manual `profile set`
7. Detects suspend/resume (a large gap between ticks) and forces an immediate
   recovery check instead of waiting out the poll interval

When a Tailscale profile is connected through a bootstrap network and the
connectivity check is intercepted, the watcher stays put and notifies once —
it does not churn reconnects against a portal.

Install as a systemd user service:

```bash
breadcrumbs install-service
# or manually:
systemctl --user enable --now breadcrumbs.service
```

## Tailscale integration

For profiles with `tailscale = true`:

1. Connects to the `bootstrap` SSID (if configured)
2. Ensures the Tailscale daemon is running; opens a browser login if needed
3. Sets the configured exit node with `tailscale set --exit-node=<node>`
4. Only moves to the target network once Tailscale is healthy

If Tailscale needs interactive login, the auth URL is opened automatically and the watch daemon stays on the bootstrap network until authentication completes.

## License

MIT
