# Operational scenario audit

This is a behavior audit for the current `wifilogin` daemon. It covers
realistic operating, routing, portal, and failure conditions—not every
theoretical failure of Linux, D-Bus, or the network stack.

## Evidence and policy

The audit is based on the implementation and the following NetworkManager
reference documentation:

- [NetworkManager root D-Bus interface](https://www.networkmanager.dev/docs/api/latest/gdbus-org.freedesktop.NetworkManager.html):
  `Connectivity`, connectivity-check settings, device signals, and
  `ActiveConnections`.
- [NetworkManager Device D-Bus interface](https://www.networkmanager.dev/docs/api/latest/gdbus-org.freedesktop.NetworkManager.Device.html):
  `ActiveConnection` is a base Device property.
- [NetworkManager ActiveConnection D-Bus interface](https://www.networkmanager.dev/docs/api/latest/gdbus-org.freedesktop.NetworkManager.Connection.Active.html):
  `Uuid`, lifecycle `State`, `Default`, and `Default6`.
- [NetworkManager connectivity states](https://www.networkmanager.dev/docs/api/latest/nm-dbus-types.html#enum-nmconnectivitystate):
  the defined `Unknown`, `None`, `Portal`, `Limited`, and `Full` meanings.

The daemon's policy is intentionally narrow:

```text
automatic credential submission
    = configured local profile UUID
    + exact SSID
    + active connection
    + an IPv4 or IPv6 default route
    + NetworkManager reports Portal
    + unpaused daemon
    + readable credentials
```

Anything outside that conjunction is non-destructive: it is observed and
reported, but never causes a Wi-Fi connection, disconnect, profile change, or
portal form submission.

## Scenario matrix

| Condition | Result | Coverage |
| --- | --- | --- |
| First run with no config | `run` exits with an actionable `config init` error; no file is created implicitly. | Safe |
| Empty target list | Daemon remains in `NoTargets`; it cannot send credentials. | Safe |
| Unknown/obsolete or malformed TOML field | Configuration is rejected by `deny_unknown_fields`; the running daemon keeps its last valid config on reload. | Safe |
| Invalid portal or check URL | Validation rejects non-HTTP(S), hostless, and unparsable URLs before the daemon starts. | Safe |
| No NetworkManager/system D-Bus | Startup fails; a systemd unit restarts according to its restart policy. | Safe, unavailable |
| NetworkManager restarts while running | `NameOwnerChanged` wakes the daemon. A transient property-read error gets one 750 ms resync; later ownership/state signals trigger another fresh read. | Safe recovery |
| D-Bus connection stream itself errors | The event task closes its receiver, the daemon exits, and systemd restarts it rather than leaving a dead watcher. | Safe recovery |
| Wi-Fi adapter absent, unmanaged, disabled, airplane mode, or no AP | `Disconnected`; no scan or activation request is made. | Safe |
| Wi-Fi device is hot-plugged/removed | `DeviceAdded`/`DeviceRemoved` and device state signals trigger a fresh device list. | Safe |
| Several Wi-Fi adapters | Every Wi-Fi device is examined; an active one owning a default route wins. Otherwise one active Wi-Fi connection is reported without action. | Safe |
| Target profile is still activating/deactivating | `Connecting`; no credentials are loaded or sent. | Safe |
| Connected to a phone hotspot or any unlisted Wi-Fi | `OtherNetwork`; no portal request. | Safe |
| Same SSID but a different local NetworkManager profile UUID | `OtherNetwork`; no portal request. | Safe |
| Target Wi-Fi is secondary to Ethernet, a full-tunnel VPN, or another default route | `TargetNotDefault`; no portal request. | Safe |
| Target owns only the IPv6 default route | `Default6` is accepted, so it can qualify just like an IPv4 default route. | Safe |
| NetworkManager reports `Full` | `Online`; no HTTP connectivity probe is made. | Safe |
| NetworkManager reports `Portal` | Credentials are submitted using the configured Pronto form, then one HTTP 204 postcondition check runs. | Expected operation |
| NetworkManager reports `Unknown`, `None`, or `Limited` | `WaitingForNetworkManager`; it does not guess that a portal exists. | Safe, manual action may be needed |
| Connectivity checks disabled in NetworkManager | State stays `Unknown`; automatic login is intentionally disabled. | Deliberate limitation |
| Credentials absent | `CredentialsMissing`; no timer retries and no password prompt occurs in the daemon. A `creds set` command wakes it. | Safe |
| Credentials definitively rejected | `BadCredentials`; no automatic retry prevents account lockouts. | Safe |
| Keyring temporarily locked/unavailable | One 750 ms local retry is attempted, then the daemon waits for a control or network event. | Safe recovery |
| DNS, TLS, portal timeout, or inconclusive portal result | Retries begin at 30 seconds and cap at 5 minutes; each retry re-checks the active profile and route first. | Bounded recovery |
| User switches to hotspot, Ethernet, VPN, or disconnects during an HTTP request | A D-Bus/control event cancels the in-flight state step; the next step reads current state before any new request. | Safe recovery |
| Portal response is oversized or chunked indefinitely | Body reading stops above 1 MiB and follows bounded retry behavior. | Safe |
| Clock changes | Timers use Tokio `Instant`, not wall time. Status timestamps may be cosmetically skewed only. | Safe |
| Pause/resume, credential changes, config edit | Persistent state is changed first, then a local Unix datagram asks the daemon to reload. | Safe |
| Manual config-file edit | Not watched by design; use `wifilogin config edit` or restart the user service. | Deliberate feature cut |
| Second daemon launch / stale control socket | A responding socket rejects the second daemon; an unresponsive stale socket is removed before binding. | Safe recovery |
| `wifilogin login` on a hotspot or Ethernet | The explicit command still requires an allowed, active, default-route profile; it refuses otherwise. | Safe |
| `wifilogin online` on any network | Performs the requested one-off HTTP check. This command is intentionally diagnostic and does not submit credentials. | Expected operation |

## Known limits and operator decisions

These are not silent failure modes; they are limits kept outside the daemon's
small interface.

1. **Only Pronto login forms are supported.** The form field names and success
   markers are hard-coded. A different captive-portal vendor requires a
   portal adapter, not a looser generic form configuration.
2. **The profile UUID is local authorization, not access-point
   authentication.** On an open network, a rogue access point with the same
   SSID can still be selected by a local NetworkManager profile. Use WPA2/WPA3
   or Enterprise authentication and, where appropriate, constrain the
   NetworkManager profile to a BSSID.
3. **Plain HTTP portal endpoints expose portal credentials to the network.**
   This is a property of portals that require HTTP. Prefer an HTTPS endpoint
   when the portal supports it.
4. **NetworkManager connectivity detection is global.** Systems with split
   IPv4/IPv6 default routes or policy routing can have an assessment that does
   not perfectly describe one Wi-Fi device. The daemon consequently chooses
   false negatives (wait) over credential submission when its target does not
   own either default route.
5. **A permanently unavailable D-Bus or keyring needs an external recovery.**
   The user service retries after process failure; a persistent unlocked-keyring
   issue requires unlocking the keyring or sending a reload. The daemon does
   not poll either subsystem indefinitely.

## Validation performed

- Unit tests cover profile identity, hotspot exclusion, active/default-route
  gates, unknown connectivity, key credential outcomes, bounded portal retry,
  and one D-Bus resync.
- Live validation on the development host successfully read an active Wi-Fi
  SSID, its active connection UUID, default-route ownership, and
  NetworkManager `Connectivity = Full` through `wifilogin status`.
