# airman

`airman` is a fast Linux terminal interface for Wi-Fi and Bluetooth. It talks
to the system daemons over their native D-Bus APIs and treats those daemons as
the authority: a request being accepted is never displayed as a successful
connection until the daemon reports the final state.

## Everyday use

- `Enter` connects the selected item; on an already connected item it
  disconnects. During a pending connection, `Enter` reverses the request.
- `j`/`k` or the arrow keys move, `g`/`G` select the first/last row, and `Tab`
  changes radio panes.
- `s` scans Wi-Fi or toggles Bluetooth discovery on/off, `d` cycles the
  Bluetooth discovery mode (auto → always on → always off), `/` starts an
  inline fuzzy search for Wi-Fi networks or Bluetooth devices without case
  sensitivity, `Ctrl-P` opens the capability-aware command palette, `l` opens
  the activity journal, and `e` explains the current error and its recovery
  steps.
- While discovery is active the footer shows the current mode and the header
  displays a compact `⚠ BT discovery may slow 2.4 GHz Wi-Fi` indicator because
  Bluetooth discovery shares the 2.4 GHz band and can add latency to 2.4 GHz
  Wi-Fi. The indicator lives in the header chrome so it never hides transient
  notifications.
- Out-of-range Wi-Fi networks and Bluetooth devices are hidden by default; `o`
  reveals or hides them in either pane.
- Successful connection order is retained across launches in
  `$XDG_STATE_HOME/airman/connection-history.json` (falling back to
  `~/.local/state/airman/connection-history.json`).
- The command palette can toggle auto-join, forget saved Wi-Fi profiles or
  Bluetooth pairings, reveal a retrievable saved password, and render a local
  Wi-Fi QR code. Forget actions require confirmation.
- On wide terminals, the right-hand inspector exposes the actions available for
  the selected entry; click one or use its displayed shortcut (`a`, `p`, `r`,
  or `f`). Wi-Fi details include the active IP addresses, prefix lengths,
  subnet masks, interface, backend, and BSSID. Bluetooth details include its
  adapter and address, RSSI when BlueZ supplies it, pairing/trust/block state,
  service readiness, and battery level when available.
- Bluetooth actions include pair, connect/disconnect, trust/untrust,
  block/unblock, and confirmed forget. The right pane shows only actions the
  selected adapter and device can currently perform.
- `F2` or `Ctrl-R` reveals/hides a password while it is being entered. Closing
  a password or QR overlay immediately clears its credential material.
- Focus follows the stable network/device identity even when connection state
  or signal changes move its row. Mouse clicks use the actual scrolled offset.
- No patched font is required.

By default airman requests an immediate Wi-Fi scan, refreshes every 15 seconds
while the Wi-Fi pane is visible and every 60 seconds in the background, and
refreshes on returning to a stale Wi-Fi pane. Scans pause during association,
authentication, address configuration, disconnection, or an existing scan.
Transient daemon refusals use exponential backoff and stay in the activity
journal instead of interrupting the user with repeated error overlays.

airman also acquires its own BlueZ discovery session as soon as an adapter is
ready. Session ownership is tracked separately from BlueZ's global shared
`Discovering` property, so `s` pauses or resumes airman's session even when
another application is scanning. Discovery is reacquired after BlueZ restarts
and adapter power cycles, with exponential retry after transient failures. A
broad BlueZ filter improves RSSI updates while suppressing duplicate
advertisement payloads. The session is released automatically when airman
exits. Use `--no-auto-scan` or `--no-auto-discover` when power usage matters more
than immediate nearby-device visibility.

The `d` shortcut selects one of three discovery modes:

| Mode | Behavior |
| --- | --- |
| auto (default) | Discovers only while the airman terminal is focused, and releases the session when it loses focus. Because Bluetooth discovery shares the 2.4 GHz band, this keeps background 2.4 GHz Wi-Fi latency low when you are not actively looking at the device list. |
| always on | Keeps discovery active regardless of terminal focus. |
| always off | Never keeps a discovery session. |

`--no-auto-discover` starts in "always off"; otherwise airman starts in "auto".
Focus tracking uses terminal focus reporting, so it requires a terminal that
emits focus events.

The list separates observed state (`connected`, `getting IP`, and so on) from a
pending request (`waiting→connected`). Entries the daemon stops reporting stay
visible briefly as `out of range` so a single snapshot gap cannot flicker the
list, then drop once the grace period expires. Saved Wi-Fi profiles and paired
Bluetooth devices that the daemon still reports as known but not currently
reachable remain as `out of range` below present entries. When BlueZ or a Wi-Fi
daemon re-keys the same name under a new identity, the predecessor is dropped
immediately instead of being listed twice.

BlueZ's RSSI property is optional. A remembered device without RSSI is shown as
`range unknown` rather than incorrectly labelled out of range; starting
discovery gives BlueZ an opportunity to update it.

## Backends

`--backend auto` selects the highest-level daemon that exposes a usable Wi-Fi
interface and fails over when its owner disappears:

| Backend | D-Bus service | Notes |
| --- | --- | --- |
| NetworkManager | `org.freedesktop.NetworkManager` | Preferred; connectivity checks, saved profiles, radio control |
| ConnMan | `net.connman` | Native Technology/Service APIs and temporary credential agent |
| iwd | `net.connman.iwd` | Native Station/Network APIs and non-displacing credential agent |
| wpa_supplicant + networkd | `fi.w1.wpa_supplicant1`, `org.freedesktop.network1` | Association and IP configuration are reported separately |
| BlueZ | `org.bluez` | Bluetooth adapters/devices through ObjectManager |

Use `--backend network-manager`, `iwd`, `wpa-networkd`, or `conn-man` to require
one Wi-Fi implementation. `--wifi-interface wlan0` and
`--bluetooth-adapter hci0` disambiguate multi-radio machines.

The implementation follows the upstream [iwd Station and Network
APIs](https://kernel.googlesource.com/pub/scm/network/wireless/iwd/+/master/doc/station-api.txt),
[wpa_supplicant D-Bus API](https://w1.fi/wpa_supplicant/devel/dbus.html), and
[ConnMan Service API](https://kernel.googlesource.com/pub/scm/network/connman/connman/+/master/doc/service-api.txt).

Known constraints are surfaced instead of guessed:

- Creating a new enterprise/802.1X profile needs identity, EAP, and certificate
  choices. Provision it with the owning daemon first; airman can then activate
  it.
- iwd exposes SSIDs as D-Bus strings, so iwd cannot preserve invalid UTF-8 SSID
  bytes. NetworkManager, wpa_supplicant, and ConnMan identities preserve raw
  bytes.
- networkd route state is not an Internet probe. The combined backend reports
  it as local/limited, never as verified Internet.
- wpa_supplicant has no radio-power D-Bus method; that command is omitted from
  the palette for that backend.
- Saved-password retrieval currently uses NetworkManager's `GetSecrets` API.
  It can return only secrets available from persistent storage or a secret
  agent in the current login session. iwd, ConnMan, and wpa_supplicant do not
  expose a comparably safe supported retrieval path, so those actions are
  omitted rather than reading daemon-private files.

## Diagnostics and logs

Run:

```console
airman diagnose
airman diagnose --json
```

Diagnostics show D-Bus owners, versions when exposed, backend epochs, interface
filters, and actionable warnings. The TUI exposes the same report through
`Ctrl-P` → `Open diagnostics`.

Every invocation writes a unique per-session log below
`$XDG_STATE_HOME/airman/logs` (normally
`~/.local/state/airman/logs`). Ten sessions are retained. `--log-file PATH`
overrides the destination. Credentials use zeroizing storage, redact `Debug`,
and low-level D-Bus trace bodies are forcibly disabled even with a broad tracing
filter.

Optional configuration lives at `$XDG_CONFIG_HOME/airman/config.toml`:

```toml
backend = "auto"
wifi_interface = "wlan0"
bluetooth_adapter = "hci0"
auto_scan = true
auto_discover = true
log_level = "airman=info"
```

Unknown configuration keys are rejected rather than silently ignored.

## Installation

### One-line installer (Linux)

```bash
curl -fsSL https://raw.githubusercontent.com/nkitsaini/airman/main/install.sh | sh
```

### With Cargo

```bash
cargo install airman
```

### With Nix

```bash
# Run directly
nix run github:nkitsaini/airman

# Install to profile
nix profile install github:nkitsaini/airman
```

## Build and verification


```console
nix develop
cargo test --all-features --all-targets
cargo clippy --all-features --all-targets -- -D warnings
cargo run --release
```

`nix build` produces the release binary. A focused local NixOS/QEMU test boots
real NetworkManager and BlueZ services and validates the machine-readable
diagnostics path:

```console
nix build .#checks.x86_64-linux.daemon-integration
```

The deterministic simulator and reducer tests cover stale/reordered events,
daemon epochs, superseded operations, confirmation timeouts, stable focus,
credential redaction, tiny terminals, and backend selection failover.

## Architecture

The UI emits intents. A runtime routes them to one owning backend. Backends emit
versioned snapshots and health events. A pure reducer rejects stale revisions,
reconciles operations against facts, and owns the state rendered by the TUI.
Signal bursts are coalesced before bounded-concurrency snapshots; a lagged event
consumer forces a fresh authoritative snapshot. Service owner changes increment
an epoch so replies from a previous daemon process cannot win a race.

## License

[MIT](LICENSE)

