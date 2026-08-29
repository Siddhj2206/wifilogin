use crate::config::Config;
use crate::keyring::{self, Creds};
use crate::portal::{LoginResult, Outcome, Portal};
use crate::settings;
use crate::wifi::{self, Wifi};
use std::time::Duration;

/// Base delay for "credentials are wrong/missing" retries. Grows to
/// [`CREDS_MAX`] — retrying a wrong password fast is how accounts get locked.
const CREDS_BASE: Duration = Duration::from_secs(60);
const CREDS_MAX: Duration = Duration::from_secs(30 * 60);
/// Cap for transient error/captive retries (base comes from config).
const ERR_MAX: Duration = Duration::from_secs(5 * 60);

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
    pub last_login: Option<LoginResult>,
    pub last_error: Option<String>,
}

#[allow(clippy::too_many_arguments)]
fn snap(
    state: State,
    message: impl Into<String>,
    ssid: Option<String>,
    on_target: bool,
    online: bool,
    last_login: Option<LoginResult>,
    last_error: Option<String>,
) -> Snapshot {
    Snapshot {
        state,
        message: message.into(),
        current_ssid: ssid,
        on_target,
        online,
        last_login,
        last_error,
    }
}

/// Exponential backoff: base, base*2, base*4, ... capped at max.
pub struct Backoff {
    base: Duration,
    max: Duration,
    attempts: u32,
}

impl Backoff {
    pub fn new(base: Duration, max: Duration) -> Self {
        Self {
            base,
            max,
            attempts: 0,
        }
    }

    pub fn next(&mut self) -> Duration {
        let mult = 1u32.checked_shl(self.attempts.min(30)).unwrap_or(u32::MAX);
        let d = self.base.saturating_mul(mult);
        self.attempts = self.attempts.saturating_add(1);
        d.min(self.max)
    }

    pub fn reset(&mut self) {
        self.attempts = 0;
    }
}

/// Core state machine — no internal polling timer, the caller drives `step`.
/// Returns `Some(duration)` only when a future re-check is needed (online
/// verify or a backed-off retry). Returns `None` when idle — the caller should
/// sleep until a D-Bus event or wake signal.
pub struct Controller<W: Wifi, P: Portal, C: Creds> {
    wifi: W,
    portal: P,
    creds: C,
    settings: settings::Manager,
    err_backoff: Backoff,
    creds_backoff: Backoff,
}

impl<W: Wifi, P: Portal, C: Creds> Controller<W, P, C> {
    pub fn new(
        wifi: W,
        portal: P,
        creds: C,
        settings: settings::Manager,
        retry_base: Duration,
    ) -> Self {
        Self {
            wifi,
            portal,
            creds,
            settings,
            err_backoff: Backoff::new(retry_base, ERR_MAX),
            creds_backoff: Backoff::new(CREDS_BASE, CREDS_MAX),
        }
    }

    pub async fn step(&mut self, cfg: &Config) -> (Snapshot, Option<Duration>) {
        let (snap, retry) = self.step_once(cfg).await;
        if snap.state == State::Online {
            self.err_backoff.reset();
            self.creds_backoff.reset();
        }
        (snap, retry)
    }

    async fn step_once(&mut self, cfg: &Config) -> (Snapshot, Option<Duration>) {
        // 0. If disabled, stay idle and wake only on signal/settings change
        if !self.settings.get().enabled {
            return (
                snap(
                    State::Paused,
                    "paused — run `wifilogin resume` to enable",
                    None,
                    false,
                    false,
                    None,
                    None,
                ),
                None,
            );
        }

        // 1. Read current SSID (one D-Bus roundtrip)
        let current = match self.wifi.current_ssid().await {
            Ok(v) => v,
            Err(e) => {
                let msg = e.to_string();
                return (
                    snap(
                        State::Error,
                        format!("failed to read WiFi state: {msg}"),
                        None,
                        false,
                        false,
                        None,
                        Some(msg),
                    ),
                    Some(self.err_backoff.next()),
                );
            }
        };

        let on_target = current
            .as_deref()
            .map(|s| cfg.is_target(s))
            .unwrap_or(false);

        // 2. Not connected at all — try each target's saved profile in order.
        let Some(cur_ssid) = current.clone() else {
            return self.step_disconnected(cfg).await;
        };

        // 3. Connected, but not to a target — idle, wait for SSID change.
        if !on_target {
            return (
                snap(
                    State::IdleOtherNetwork,
                    format!("on {cur_ssid} — idle (targets: {})", cfg.targets.join(", ")),
                    Some(cur_ssid),
                    false,
                    false,
                    None,
                    None,
                ),
                None,
            );
        }

        // 4. On target — check connectivity
        let online = match self.portal.online(&cfg.connectivity_url).await {
            Ok(v) => v,
            Err(e) => {
                let msg = e.to_string();
                return (
                    snap(
                        State::Error,
                        format!("connectivity check failed: {msg}"),
                        Some(cur_ssid),
                        true,
                        false,
                        None,
                        Some(msg),
                    ),
                    Some(self.err_backoff.next()),
                );
            }
        };

        if online {
            return (
                snap(
                    State::Online,
                    "connected and authenticated",
                    Some(cur_ssid),
                    true,
                    true,
                    None,
                    None,
                ),
                Some(cfg.verify_interval),
            );
        }

        // 5. Captive — need login
        let (username, password) = match self.creds.load().await {
            Ok(v) => v,
            Err(e) if keyring::is_not_found(&e) => {
                return (
                    snap(
                        State::CredentialsMissing,
                        "no credentials stored — run `wifilogin creds set <username>`",
                        Some(cur_ssid),
                        true,
                        false,
                        None,
                        Some("credentials missing".into()),
                    ),
                    Some(self.creds_backoff.next()),
                );
            }
            Err(e) => {
                let msg = e.to_string();
                return (
                    snap(
                        State::Error,
                        format!("failed to load credentials: {msg}"),
                        Some(cur_ssid),
                        true,
                        false,
                        None,
                        Some(msg),
                    ),
                    Some(self.err_backoff.next()),
                );
            }
        };

        let login_res = match self
            .portal
            .login(&cfg.portal_url, &username, &password)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                let msg = e.to_string();
                return (
                    snap(
                        State::Error,
                        format!("portal login failed: {msg}"),
                        Some(cur_ssid),
                        true,
                        false,
                        None,
                        Some(msg),
                    ),
                    Some(self.err_backoff.next()),
                );
            }
        };

        if login_res.outcome == Outcome::BadCredentials {
            return (
                snap(
                    State::BadCredentials,
                    "invalid username or password — update with `wifilogin creds set`",
                    Some(cur_ssid),
                    true,
                    false,
                    Some(login_res),
                    Some("invalid credentials".into()),
                ),
                Some(self.creds_backoff.next()),
            );
        }

        // 6. Verify online after login, up to 3 tries
        for _ in 0..3 {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if let Ok(true) = self.portal.online(&cfg.connectivity_url).await {
                return (
                    snap(
                        State::Online,
                        "connected and authenticated",
                        Some(cur_ssid),
                        true,
                        true,
                        Some(login_res),
                        None,
                    ),
                    Some(cfg.verify_interval),
                );
            }
        }

        (
            snap(
                State::Captive,
                "still captive after login — retrying with backoff",
                Some(cur_ssid),
                true,
                false,
                Some(login_res),
                None,
            ),
            Some(self.err_backoff.next()),
        )
    }

    /// Disconnected: try to activate each target's saved profile. No scanning,
    /// no hammering — one pass, then wait for the next wake.
    async fn step_disconnected(&mut self, cfg: &Config) -> (Snapshot, Option<Duration>) {
        let mut last_err: Option<anyhow::Error> = None;
        let mut provisioning_missing = 0;

        for target in &cfg.targets {
            match self.wifi.ensure_connected(target).await {
                // Race: wifi came back between our SSID read and here. Not an
                // error — just re-check shortly.
                Ok(_) => {
                    return (
                        snap(
                            State::WifiDisconnected,
                            format!("activating saved profile for {target}"),
                            None,
                            false,
                            false,
                            None,
                            None,
                        ),
                        Some(Duration::from_secs(3)),
                    );
                }
                Err(e) if wifi::is_provisioning(&e) => {
                    provisioning_missing += 1;
                    last_err = Some(e);
                }
                Err(e) => {
                    let msg = e.to_string();
                    return (
                        snap(
                            State::Error,
                            format!("failed to connect to {target}: {msg}"),
                            None,
                            false,
                            false,
                            None,
                            Some(msg),
                        ),
                        Some(self.err_backoff.next()),
                    );
                }
            }
        }

        if provisioning_missing == cfg.targets.len() {
            // Nothing we can do — the user must connect once via OS settings
            // so NM has a saved profile with passwords.
            return (
                snap(
                    State::NeedsProvision,
                    format!(
                        "connect to {} once via OS WiFi settings so a saved profile exists",
                        cfg.targets.join(", ")
                    ),
                    None,
                    false,
                    false,
                    None,
                    last_err.map(|e| e.to_string()),
                ),
                None,
            );
        }

        (
            snap(
                State::WifiDisconnected,
                "wifi disconnected — waiting for target",
                None,
                false,
                false,
                None,
                last_err.map(|e| e.to_string()),
            ),
            None,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    struct FakeCreds {
        present: Arc<AtomicBool>,
    }

    impl Creds for FakeCreds {
        async fn load(&self) -> Result<(String, String)> {
            if self.present.load(Ordering::SeqCst) {
                Ok(("user".into(), "pass".into()))
            } else {
                Err(anyhow::Error::new(keyring::NotFound))
            }
        }
    }

    struct FakePortal {
        online: Arc<AtomicBool>,
        login_outcome: Mutex<Outcome>,
    }

    impl Portal for FakePortal {
        async fn online(&self, _url: &str) -> Result<bool> {
            Ok(self.online.load(Ordering::SeqCst))
        }
        async fn login(
            &self,
            _url: &str,
            _u: &str,
            _p: &str,
        ) -> Result<LoginResult> {
            Ok(LoginResult {
                outcome: *self.login_outcome.lock().unwrap(),
                http_status: 200,
                body_snippet: String::new(),
            })
        }
    }

    struct FakeWifi {
        ssid: Option<String>,
    }

    impl Wifi for FakeWifi {
        async fn current_ssid(&self) -> Result<Option<String>> {
            Ok(self.ssid.clone())
        }
        async fn ensure_connected(&self, target: &str) -> Result<wifi::EnsureResult> {
            Err(anyhow::Error::new(wifi::NeedsProvisioning(target.into())))
        }
    }

    fn test_settings() -> settings::Manager {
        let dir = std::env::temp_dir().join(format!(
            "wifilogin-session-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        settings::Manager::from_path(dir.join("settings.json")).unwrap()
    }

    fn controller(
        ssid: Option<&str>,
        online: bool,
        outcome: Outcome,
        creds_present: bool,
    ) -> Controller<FakeWifi, FakePortal, FakeCreds> {
        let creds = Arc::new(AtomicBool::new(creds_present));
        let portal = FakePortal {
            online: Arc::new(AtomicBool::new(online)),
            login_outcome: Mutex::new(outcome),
        };
        // Re-wrap so tests can mutate later via the same Arcs if needed.
        let _ = &creds;
        Controller::new(
            FakeWifi {
                ssid: ssid.map(|s| s.to_string()),
            },
            portal,
            FakeCreds { present: creds },
            test_settings(),
            Duration::from_secs(10),
        )
    }

    #[tokio::test]
    async fn paused_returns_no_retry() {
        let mut c = controller(Some("R-VIT"), false, Outcome::Granted, true);
        c.settings.set_enabled(false).unwrap();
        let (snap, retry) = c.step(&Config::default()).await;
        assert_eq!(snap.state, State::Paused);
        assert_eq!(retry, None);
    }

    #[tokio::test]
    async fn idle_on_other_network() {
        let mut c = controller(Some("CoffeeShop"), false, Outcome::Granted, true);
        let (snap, retry) = c.step(&Config::default()).await;
        assert_eq!(snap.state, State::IdleOtherNetwork);
        assert_eq!(retry, None);
    }

    #[tokio::test]
    async fn online_verifies_on_interval() {
        let mut c = controller(Some("R-VIT"), true, Outcome::Granted, true);
        let (snap, retry) = c.step(&Config::default()).await;
        assert_eq!(snap.state, State::Online);
        assert_eq!(retry, Some(Duration::from_secs(60)));
    }

    #[tokio::test]
    async fn bad_credentials_backs_off_and_resets() {
        let mut c = controller(Some("R-VIT"), false, Outcome::BadCredentials, false);
        let (_, r1) = c.step(&Config::default()).await;
        let (_, r2) = c.step(&Config::default()).await;
        assert_eq!(r1, Some(Duration::from_secs(60)));
        assert_eq!(r2, Some(Duration::from_secs(120)));

        // Success resets both backoffs
        c.portal.online.store(true, Ordering::SeqCst);
        let (snap, _) = c.step(&Config::default()).await;
        assert_eq!(snap.state, State::Online);

        // Back to bad creds: backoff restarts at base, not 240s
        c.portal.online.store(false, Ordering::SeqCst);
        c.creds.present.store(false, Ordering::SeqCst);
        let (_, r3) = c.step(&Config::default()).await;
        assert_eq!(r3, Some(Duration::from_secs(60)));
    }

    #[tokio::test]
    async fn disconnected_without_profile_needs_provision() {
        let mut c = controller(None, false, Outcome::Granted, true);
        let (snap, retry) = c.step(&Config::default()).await;
        assert_eq!(snap.state, State::NeedsProvision);
        assert_eq!(retry, None);
    }

    #[test]
    fn backoff_growth_capped() {
        let mut b = Backoff::new(Duration::from_secs(10), Duration::from_secs(60));
        assert_eq!(b.next(), Duration::from_secs(10));
        assert_eq!(b.next(), Duration::from_secs(20));
        assert_eq!(b.next(), Duration::from_secs(40));
        assert_eq!(b.next(), Duration::from_secs(60));
        assert_eq!(b.next(), Duration::from_secs(60));
        b.reset();
        assert_eq!(b.next(), Duration::from_secs(10));
    }
}
