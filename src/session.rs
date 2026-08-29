use crate::config::Config;
use crate::portal;
use crate::settings;
use crate::wifi;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Online,
    Captive,
    WifiDisconnected,
    NeedsProvision,
    BadCredentials,
    CredentialsMissing,
    Error,
    IdleOtherNetwork,
    Paused,
}

impl std::fmt::Display for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub state: State,
    pub message: String,
    pub current_ssid: Option<String>,
    pub on_target: bool,
    pub online: bool,
    pub last_login: Option<portal::LoginResult>,
    pub last_error: Option<String>,
}

/// Core state machine — no internal polling timer, caller drives `step`.
/// Returns `Some(duration)` only when on target and needs re-check (online verify or captive retry).
/// Returns `None` when idle (other network / disconnected not on target) — caller should sleep until D-Bus event / wake.
pub struct Controller<'a> {
    cfg: &'a Config,
    wifi: &'a wifi::Manager,
    settings: &'a settings::Manager,
}

impl<'a> Controller<'a> {
    pub fn new(cfg: &'a Config, wifi: &'a wifi::Manager, settings: &'a settings::Manager) -> Self {
        Self {
            cfg,
            wifi,
            settings,
        }
    }

    /// One step; performs wifi/portal actions synchronously. Efficient: only talks to network when on target.
    pub async fn step(&self) -> (Snapshot, Option<Duration>) {
        // 0. If disabled, stay idle and wake only on signal/settings change
        if !self.settings.get().enabled {
            let snap = Snapshot {
                state: State::Paused,
                message: "paused — run `wifilogin resume` to enable".into(),
                current_ssid: None,
                on_target: false,
                online: false,
                last_login: None,
                last_error: None,
            };
            return (snap, None);
        }

        // 1. Read current SSID (one D-Bus roundtrip)
        let current = match self.wifi.current_ssid().await {
            Ok(v) => v,
            Err(e) => {
                let snap = Snapshot {
                    state: State::Error,
                    message: format!("failed to read WiFi state: {e}"),
                    current_ssid: None,
                    on_target: false,
                    online: false,
                    last_login: None,
                    last_error: Some(e.to_string()),
                };
                return (snap, Some(Duration::from_secs(10)));
            }
        };

        let on_target = current
            .as_deref()
            .map(|s| self.cfg.is_target(s))
            .unwrap_or(false);

        // If not connected at all
        let Some(cur_ssid) = current.clone() else {
            // Disconnected — try each target in order until one can be activated.
            // Efficient: only tries saved profiles, no scanning.
            let mut last_err: Option<anyhow::Error> = None;
            let mut provisioned_missing = 0;
            for target in &self.cfg.targets {
                match self.wifi.ensure_connected(target).await {
                    Ok(r) if r.already_connected => unreachable!(),
                    Ok(_) => {
                        let snap = Snapshot {
                            state: State::WifiDisconnected,
                            message: format!("connecting to {target}"),
                            current_ssid: None,
                            on_target: false,
                            online: false,
                            last_login: None,
                            last_error: None,
                        };
                        return (snap, Some(Duration::from_secs(5)));
                    }
                    Err(e) if wifi::is_provisioning(&e) => {
                        provisioned_missing += 1;
                        last_err = Some(e);
                        continue;
                    }
                    Err(e) => {
                        last_err = Some(e);
                        // transient error — retry soon
                        let msg = last_err.as_ref().unwrap().to_string();
                        let snap = Snapshot {
                            state: State::Error,
                            message: format!("failed to connect to {target}: {msg}"),
                            current_ssid: None,
                            on_target: false,
                            online: false,
                            last_login: None,
                            last_error: Some(msg),
                        };
                        return (snap, Some(Duration::from_secs(15)));
                    }
                }
            }
            // All targets missing saved profile
            if provisioned_missing == self.cfg.targets.len() {
                let snap = Snapshot {
                    state: State::NeedsProvision,
                    message: format!(
                        "connect to {} once via OS WiFi settings",
                        self.cfg.targets.join(", ")
                    ),
                    current_ssid: None,
                    on_target: false,
                    online: false,
                    last_login: None,
                    last_error: last_err.map(|e| e.to_string()),
                };
                return (snap, None);
            }
            // No targets tried? fallback idle — wait for event
            let snap = Snapshot {
                state: State::WifiDisconnected,
                message: "wifi disconnected — waiting for target".into(),
                current_ssid: None,
                on_target: false,
                online: false,
                last_login: None,
                last_error: last_err.map(|e| e.to_string()),
            };
            return (snap, None);
        };

        // We are connected to some SSID
        if !on_target {
            let snap = Snapshot {
                state: State::IdleOtherNetwork,
                message: format!(
                    "on {cur_ssid} — idle (targets: {})",
                    self.cfg.targets.join(", ")
                ),
                current_ssid: Some(cur_ssid),
                on_target: false,
                online: false,
                last_login: None,
                last_error: None,
            };
            // Efficient: no timer at all until SSID changes (D-Bus event)
            return (snap, None);
        }

        // On target — check connectivity
        let online = match portal::online().await {
            Ok(v) => v,
            Err(e) => {
                let snap = Snapshot {
                    state: State::Error,
                    message: format!("connectivity check failed: {e}"),
                    current_ssid: Some(cur_ssid),
                    on_target: true,
                    online: false,
                    last_login: None,
                    last_error: Some(e.to_string()),
                };
                return (snap, Some(Duration::from_secs(10)));
            }
        };

        if online {
            let snap = Snapshot {
                state: State::Online,
                message: "connected and authenticated".into(),
                current_ssid: Some(cur_ssid),
                on_target: true,
                online: true,
                last_login: None,
                last_error: None,
            };
            // Only re-verify periodically while online on target
            return (snap, Some(self.cfg.verify_interval));
        }

        // Captive — need login
        let (username, password) = match crate::keyring::load().await {
            Ok(v) => v,
            Err(e) if crate::keyring::is_not_found(&e) => {
                let snap = Snapshot {
                    state: State::CredentialsMissing,
                    message: "credentials missing — run `wifilogin creds set <user> <pass>`".into(),
                    current_ssid: Some(cur_ssid),
                    on_target: true,
                    online: false,
                    last_login: None,
                    last_error: Some("set username and password".into()),
                };
                // Retry after 60s as fallback, but daemon also wakes immediately on `creds set` signal
                return (snap, Some(Duration::from_secs(60)));
            }
            Err(e) => {
                let snap = Snapshot {
                    state: State::Error,
                    message: format!("failed to load credentials: {e}"),
                    current_ssid: Some(cur_ssid),
                    on_target: true,
                    online: false,
                    last_login: None,
                    last_error: Some(e.to_string()),
                };
                return (snap, Some(Duration::from_secs(20)));
            }
        };

        // Perform login
        let login_res = match portal::login(&self.cfg.portal_url, &username, &password).await {
            Ok(r) => r,
            Err(e) => {
                let snap = Snapshot {
                    state: State::Error,
                    message: format!("portal login failed: {e}"),
                    current_ssid: Some(cur_ssid),
                    on_target: true,
                    online: false,
                    last_login: None,
                    last_error: Some(e.to_string()),
                };
                return (snap, Some(Duration::from_secs(15)));
            }
        };

        if login_res.outcome == portal::Outcome::BadCredentials {
            let snap = Snapshot {
                state: State::BadCredentials,
                message: "invalid username or password".into(),
                current_ssid: Some(cur_ssid),
                on_target: true,
                online: false,
                last_login: Some(login_res),
                last_error: Some("invalid credentials".into()),
            };
            // Don't retry quickly on bad creds — wait for creds change or long backoff
            return (snap, Some(Duration::from_secs(90)));
        }

        // Verify online after login, up to 3 tries
        for _ in 0..3 {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if let Ok(true) = portal::online().await {
                let snap = Snapshot {
                    state: State::Online,
                    message: "connected and authenticated".into(),
                    current_ssid: Some(cur_ssid.clone()),
                    on_target: true,
                    online: true,
                    last_login: Some(login_res.clone()),
                    last_error: None,
                };
                return (snap, Some(self.cfg.verify_interval));
            }
        }

        let snap = Snapshot {
            state: State::Captive,
            message: "still captive after login".into(),
            current_ssid: Some(cur_ssid),
            on_target: true,
            online: false,
            last_login: Some(login_res),
            last_error: None,
        };
        (snap, Some(self.cfg.retry_interval))
    }
}
