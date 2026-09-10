# wifilogin

`wifilogin` submits VIT captive-portal credentials only on Wi-Fi names you
explicitly configure. It is designed to be safe to leave running: it never
scans for, connects to, disconnects from, or changes the autoconnect policy of
a network.

## Safety model

The daemon will submit credentials only when all of these conditions are true:

1. Wi-Fi is already associated with a configured target SSID.
2. That Wi-Fi connection is NetworkManager's default route.
3. NetworkManager reports its connectivity as `Portal`.
4. A username is configured and the password is in the system keyring.

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
wifilogin setup --target R-VIT
```

Install the executable first so the systemd unit has a stable path. The setup
command prompts for your VIT username and password, writes the target list and
username to its config, stores only the password in the system keyring, and
installs/starts the systemd user service. Repeat `--target` for each VIT Wi-Fi
name. To configure credentials without installing the service, run
`wifilogin setup --no-service`.

No editor is required for normal use. Manage the allowlist through:

```sh
wifilogin target list
wifilogin target add "VIT Guest"
wifilogin target remove "VIT Guest"
```

For non-interactive setup, provide the username and every initial target as
arguments, then pass the password on standard input:

```sh
printf '%s\n' "$PASSWORD" | wifilogin setup my-vtop-user --target R-VIT --stdin
```

After a submitted login, wifilogin makes one HTTP 204 request to verify the
result. NetworkManager's connectivity status—not that request—decides whether
the daemon starts an automatic login.

## Commands

| Command | Purpose |
| --- | --- |
| `wifilogin status` | Show live NetworkManager state plus configured targets and credential availability. |
| `wifilogin setup [username] --target <SSID>` | Configure targets and username, securely store the password, and install/start the user service. |
| `wifilogin target list/add/remove` | Manage target Wi-Fi names without editing a config file. |
| `wifilogin login` | Submit credentials now, but only on an allowed target Wi-Fi network that owns the default route. |
| `wifilogin uninstall` | Disable, stop, and remove the systemd user service. |

Target changes restart an installed user service. Otherwise, stop and restart
the foreground daemon to apply them. No filesystem watcher or custom reload
socket runs in the background.

## Requirements

- Linux with NetworkManager on the system D-Bus
- NetworkManager connectivity checking enabled; if it reports `Unknown` or
  `Limited`, wifilogin deliberately waits rather than guessing that a portal is
  present
- A user session keyring supported by the `keyring` crate
- Optional: systemd user services for automatic background login
