# breadcrumbs

A profile-aware Wi-Fi state machine for Linux with Tailscale exit-node management and a self-healing watch daemon.

breadcrumbs sits on top of NetworkManager (`nmcli`) and manages your Wi-Fi based on **location profiles**. Switch between home, work, school, or any other context with a single command — it handles scanning, connecting, DNS pinning, and Tailscale setup automatically.

## Features

- **Profile-based connection management** — define ordered network priority lists per location
- **Bootstrap + Tailscale gating** — connect to an interim network first, bring up Tailscale, then move to the target network
- **Self-healing watch daemon** — monitors for drops, auto-recovers, reacts within seconds via `nmcli monitor`
- **Auto-detection** — scans visible SSIDs and guesses your location from config-defined markers (picks the profile with the most markers in range)
- **Captive-portal detection** — distinguishes a real connection from a sign-in page and surfaces the portal URL instead of falsely reporting "online"
- **Secure credential handling** — passwords fed to `nmcli` out-of-band (via stdin with `--ask`, or a 0600 `passwd-file`), never in argv/`ps`; config stored at 0600
- **Machine-readable status** — `breadcrumbs status --json` for bars/scripts
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
git clone https://git.breadway.dev/Breadway/breadcrumbs
cd breadcrumbs
cargo build --release
# Copy to somewhere on your PATH:
cp target/release/breadcrumbs ~/.local/bin/
```

## Configuration

On first run, breadcrumbs creates `~/.config/breadcrumbs/breadcrumbs.toml` with default profiles. Copy `breadcrumbs.example.toml` as a starting point and fill in your real network credentials:

```bash
cp breadcrumbs.example.toml ~/.config/breadcrumbs/breadcrumbs.toml
breadcrumbs edit   # opens in $EDITOR
```

Config paths respect `$XDG_CONFIG_HOME` and `$XDG_STATE_HOME`.

### Config structure

```toml
[settings]
dns = "1.1.1.1"          # DNS server pinned on every connection
nmcli_wait = 8           # seconds to wait for nmcli connect
exit_node = "myhostname" # default Tailscale exit node
default_profile = "away"
watch_interval = 12      # seconds between health checks (minimum 4)
connectivity_url = "http://connectivitycheck.gstatic.com/generate_204"
ping_host = "1.1.1.1"

[[networks]]
ssid = "MyHomeNetwork"
password = "hunter2"
hidden = false

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

### Profiles

Each profile defines:

| Key | Description |
|-----|-------------|
| `networks` | Ordered list of SSIDs to try. First available wins. |
| `tailscale` | If `true`, Tailscale must be healthy before moving to a target network. |
| `bootstrap` | SSID to connect to first (e.g. guest Wi-Fi that allows Tailscale traffic). |
| `exit_node` | Tailscale exit node for this profile (overrides `settings.exit_node`). |
| `include_all_known` | After the priority list, also try every other known network. |
| `detect_ssids` | Any visible SSID in this list marks this profile as a candidate for `breadcrumbs detect`. |

## Usage

```
breadcrumbs [--profile <name>] <command>
```

| Command | Description |
|---------|-------------|
| `status [--json]` | Show current Wi-Fi / Tailscale health (default); `--json` for scripts |
| `init` | Run the full connect sequence for the active profile |
| `watch [--no-initial]` | Self-healing daemon: monitors and auto-recovers drops |
| `profile get` | Print the active profile |
| `profile set <name>` | Switch profile (and apply it, unless `--no-apply`) |
| `profile list` | List all profiles |
| `profile add <name> [--detect <ssid>]…` | Create a new (empty) profile, optionally with detection markers |
| `profile remove <name>` | Delete a profile (core `home`/`work`/`away` are protected) |
| `detect [--apply]` | Guess profile from visible networks; optionally apply it |
| `add <ssid> [password]` | Add or update a saved network |
| `forget <ssid>` | Remove a network from config and NetworkManager |
| `join <ssid>` | Connect to a specific saved network by SSID, bypassing profile routing |
| `networks [--json]` | List saved network SSIDs |
| `scan [--to <profile>]` | Interactive scan, pick, connect and save |
| `scan-list [--json]` | Scan for visible networks and list them with signal strength |
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
