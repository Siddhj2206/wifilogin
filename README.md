# wifilogin

`wifilogin` submits captive-portal credentials for explicitly allowed Wi-Fi
networks managed by NetworkManager. It is designed to be safe to leave running:
it never scans for, connects to, disconnects from, or changes the autoconnect
policy of a network.

## Safety model

The daemon will submit credentials only when all of these conditions are true:

1. Wi-Fi is already associated with a `targets` entry's exact SSID and local
   NetworkManager connection UUID.
2. That Wi-Fi connection is NetworkManager's default route.
3. NetworkManager reports its connectivity as `Portal`.
4. Automatic login is not paused and credentials are in the system keyring.

This means a phone hotspot, an unlisted coffee-shop network, Ethernet, or a VPN
that becomes the preferred route is left alone. An empty `targets` list is
valid and is the safe default.

NetworkManager D-Bus signals drive normal operation: association, default-route
and connectivity changes each trigger a fresh state read. There is no periodic
Wi-Fi or HTTP polling. A timer is used only for exponential backoff after a
portal accepted neither a verified login nor a definitive bad-credentials
response.

## Setup

```sh
wifilogin config init
$EDITOR "$(wifilogin config path)"
wifilogin creds set my-portal-user
wifilogin service install
```

Replace the empty `targets` list with one entry for each local NetworkManager
profile where submitting the configured credentials is intended. Get the UUID
with `nmcli -g UUID connection show "Campus WiFi"`:

```toml
[[targets]]
ssid = "Campus WiFi"
connection_uuid = "00000000-0000-0000-0000-000000000000"

[[targets]]
ssid = "Campus WiFi 5G"
connection_uuid = "11111111-1111-1111-1111-111111111111"

portal_url = "http://portal.example.invalid/login"
connectivity_url = "http://clients3.google.com/generate_204"
```

`connectivity_url` is requested once after a submitted login to verify its
result. NetworkManager's connectivity status—not that URL—decides whether the
daemon starts an automatic login.

## Commands

| Command | Purpose |
| --- | --- |
| `wifilogin status` | Show the daemon's last state and live NetworkManager state. |
| `wifilogin pause` / `resume` | Disable or enable automatic login without stopping the daemon. |
| `wifilogin creds set [username]` | Store credentials in the system keyring. |
| `wifilogin config edit` | Edit, validate, and reload configuration. |
| `wifilogin login` | Submit credentials now, but only on an allowed active profile that owns the default route. |
| `wifilogin online` | Run a one-off HTTP connectivity check. |
| `wifilogin service install` | Install and start the systemd user service. |

When configuration or credentials change through a `wifilogin` command, a
local Unix datagram wakes the daemon. For manual edits, run
`wifilogin config edit`, `systemctl --user restart wifilogin`, or restart the
daemon; no filesystem watcher is kept running.

## Requirements

- Linux with NetworkManager on the system D-Bus
- NetworkManager connectivity checking enabled; if it reports `Unknown` or
  `Limited`, wifilogin deliberately waits rather than guessing that a portal is
  present
- A user session keyring supported by the `keyring` crate
- Optional: systemd user services for `wifilogin service ...`
