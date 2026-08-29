# wifilogin

Event-driven captive-portal auto-login for Linux/NetworkManager. Designed for
college campuses behind a Pronto Networks portal (e.g. R-VIT), but the portal
endpoint and target SSIDs are configurable.

The daemon sleeps until something actually happens:

- a NetworkManager D-Bus event (SSID or device state change) — no polling
- a config/settings/credential change (via filesystem watch)
- a periodic connectivity re-verify **only while online on a target SSID**

On any other network, or while disconnected, it does nothing at all.

## Setup

```sh
cargo install --path .

wifilogin config init        # writes a commented config, tells you where
$EDITOR $(wifilogin config path)
wifilogin creds set myuser   # prompts for the password (hidden, confirmed)
wifilogin status             # sanity check
```

Run it as a systemd user service (recommended):

```sh
mkdir -p ~/.config/systemd/user
cp systemd/wifilogin.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now wifilogin
```

Or just run `wifilogin` (equivalent to `wifilogin run`).

> **Keyring note:** credentials live in the secret service (gnome-keyring/KWallet).
> Under systemd this needs an unlocked session keyring — standard on desktop
> distros with PAM keyring unlock.

## Commands

| Command | What it does |
|---|---|
| `wifilogin run` | Start the daemon (default when no subcommand given) |
| `wifilogin status` | Is the daemon alive? What did it last do? Wifi/creds/config overview |
| `wifilogin online` | Connectivity check. Exit `0` online, `1` captive/offline, `2` error |
| `wifilogin login` | Force a portal login now, then verify connectivity |
| `wifilogin ensure [SSID]` | Activate the saved NM profile for a target |
| `wifilogin pause` / `resume` | Disable/enable auto-login without stopping the daemon |
| `wifilogin creds set [user]` | Store credentials (password prompted, or `--stdin`) |
| `wifilogin creds get` / `delete` | Check / remove stored credentials |
| `wifilogin config init` / `show` / `path` / `edit` | Manage the config file |

`config edit` opens `$EDITOR` and validates the file afterwards.

## How the daemon decides what to do

A state machine runs one `step` per wake event:

```
Paused ── if disabled
Error ─── can't read wifi state            → backoff retry
WifiDisconnected ── tries saved NM profiles for each target in order
NeedsProvision ── no saved profile; connect once via OS settings (no retry)
IdleOtherNetwork ── on a non-target SSID   → sleep until next event
Online ── on target + HTTP 204             → re-verify every verify_interval
Captive ── on target, portal intercepts    → login → verify → backoff retry
BadCredentials / CredentialsMissing ──     → backoff (1 min → 30 min cap)
```

Retries use exponential backoff (base `retry_interval`, capped at 5 minutes;
credential failures capped at 30 minutes) so a broken portal or a wrong
password never gets hammered — that's how accounts get locked out.

State transitions surface as desktop notifications (`notify-send`, best
effort): failed logins, missing credentials, and recovery back to online.

## Configuration

`~/.config/wifilogin/config.toml`:

```toml
targets = ["R-VIT", "R-VIT-5G"]
portal_url = "http://phc.prontonetworks.com/cgi-bin/authlogin?URI="
connectivity_url = "http://clients3.google.com/generate_204"
verify_interval = "60s"
retry_interval = "10s"
```

- `targets`: SSIDs that trigger auto-login. The daemon only ever connects,
  logs in, or polls while on one of these.
- `connectivity_url`: must return HTTP 204 when truly online.
- `verify_interval`: how often to re-check while online on a target.
- `retry_interval`: base for the exponential retry backoff.

Config, settings, and credentials are all hot-reloaded — no daemon restart
needed. Overrides: `--config <path>` flag, `WIFILOGIN_CONFIG`,
`WIFILOGIN_STATE_PATH` env vars.

## Development

```sh
cargo test        # unit tests (state machine, backoff, config, portal parsing)
RUST_LOG=debug wifilogin run   # verbose logging (logs go to stderr)
```

The session controller is testable without D-Bus or network: it depends on
`Wifi`, `Portal`, and `Creds` traits, and the tests drive it with fakes.
