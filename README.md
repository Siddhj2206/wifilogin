# wifilogin

`wifilogin` submits VIT captive-portal credentials for the `R-VIT` Wi-Fi
network managed by NetworkManager. It is designed to be safe to leave running:
it never scans for, connects to, disconnects from, or changes the autoconnect
policy of a network.

## Safety model

The daemon will submit credentials only when all of these conditions are true:

1. Wi-Fi is already associated with the configured `R-VIT` target SSID.
2. That Wi-Fi connection is NetworkManager's default route.
3. NetworkManager reports its connectivity as `Portal`.
4. Automatic login is not paused, a username is configured, and the password is
   in the system keyring.

This means a phone hotspot, an unlisted coffee-shop network, Ethernet, or a VPN
that becomes the preferred route is left alone. A target list is an allowlist;
it does not tell NetworkManager to join those networks.

NetworkManager D-Bus signals drive normal operation: association, default-route
and connectivity changes each trigger a fresh state read. There is no periodic
Wi-Fi or HTTP polling. A timer is used only for exponential backoff after a
portal accepted neither a verified login nor a definitive bad-credentials
response.

## Setup

```sh
cargo install --path .
wifilogin setup
```

Install the executable first so the systemd unit has a stable path. The setup
command prompts for your VIT username and password, writes the VIT target list
and username to its config, stores only the password in the system keyring, and
installs/starts the systemd user service. To configure credentials without
installing the service, run `wifilogin setup --no-service`.

No editor is required for normal use. `wifilogin creds set <username>` updates
the configured username and keyring password later. `wifilogin config show`
and `wifilogin config edit` remain available only for diagnostics and unusual
configuration changes.

`connectivity_url` is requested once after a submitted login to verify its
result. NetworkManager's connectivity status—not that URL—decides whether the
daemon starts an automatic login.

## Commands

| Command | Purpose |
| --- | --- |
| `wifilogin status` | Show the daemon's last state and live NetworkManager state. |
| `wifilogin pause` / `resume` | Disable or enable automatic login without stopping the daemon. |
| `wifilogin setup [username]` | Configure the VIT target list and username, securely store the password, and install/start the user service. |
| `wifilogin creds set [username]` | Update the config username and the password in the system keyring. |
| `wifilogin config edit` | Edit, validate, and reload configuration. |
| `wifilogin login` | Submit credentials now, but only on an allowed target Wi-Fi network that owns the default route. |
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
