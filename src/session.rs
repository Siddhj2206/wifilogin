use crate::config::{Config, PortalPermission};
use crate::keyring::{self, Creds};
use crate::portal::{Outcome, Portal};
use crate::settings;
use crate::wifi::{Connectivity, Wifi};
use std::time::Duration;
use tokio::time::Instant;

const RETRY_BASE: Duration = Duration::from_secs(30);
const RETRY_MAX: Duration = Duration::from_secs(5 * 60);
const SNAPSHOT_RESYNC_DELAY: Duration = Duration::from_millis(750);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Paused,
    NoTargets,
    Disconnected,
    OtherNetwork,
    Connecting,
    TargetNotDefault,
    WaitingForNetworkManager,
    Online,
    Captive,
    CredentialsMissing,
    BadCredentials,
    Error,
}

impl std::fmt::Display for State {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{self:?}")
    }
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub state: State,
    pub message: String,
    pub current_ssid: Option<String>,
    pub last_error: Option<String>,
}

impl Snapshot {
    fn idle(state: State, message: impl Into<String>) -> Self {
        Self {
            state,
            message: message.into(),
            current_ssid: None,
            last_error: None,
        }
    }

    fn on_network(state: State, ssid: String, message: impl Into<String>) -> Self {
        Self {
            state,
            message: message.into(),
            current_ssid: Some(ssid),
            last_error: None,
        }
    }

    fn with_error(mut self, error: impl Into<String>) -> Self {
        self.last_error = Some(error.into());
        self
    }

    pub fn is_online(&self) -> bool {
        self.state == State::Online
    }
}

/// Exponential portal retry delay. It is deliberately separate from the
/// one-off D-Bus resync used to recover a property-read race.
pub struct Backoff {
    attempts: u32,
}

impl Backoff {
    fn next(&mut self) -> Duration {
        let multiplier = 1u32.checked_shl(self.attempts.min(30)).unwrap_or(u32::MAX);
        self.attempts = self.attempts.saturating_add(1);
        RETRY_BASE.saturating_mul(multiplier).min(RETRY_MAX)
    }

    fn reset(&mut self) {
        self.attempts = 0;
    }
}

struct PortalRetry {
    ssid: String,
    connection_uuid: String,
    deadline: Instant,
    message: String,
}

impl PortalRetry {
    fn matches(&self, ssid: &str, connection_uuid: &str) -> bool {
        self.ssid == ssid && self.connection_uuid == connection_uuid
    }
}

/// Captive-portal policy. It never asks NetworkManager to activate a profile.
/// Normal work is driven by D-Bus events; a retry is returned only after an
/// inconclusive portal request or one failed local state/keyring read.
pub struct Controller<W: Wifi, P: Portal, C: Creds> {
    wifi: W,
    portal: P,
    creds: C,
    settings: settings::Manager,
    portal_backoff: Backoff,
    portal_retry: Option<PortalRetry>,
    resync_pending: bool,
    keyring_resync_pending: bool,
}

impl<W: Wifi, P: Portal, C: Creds> Controller<W, P, C> {
    pub fn new(wifi: W, portal: P, creds: C, settings: settings::Manager) -> Self {
        Self {
            wifi,
            portal,
            creds,
            settings,
            portal_backoff: Backoff { attempts: 0 },
            portal_retry: None,
            resync_pending: false,
            keyring_resync_pending: false,
        }
    }

    pub async fn step(&mut self, config: &Config) -> (Snapshot, Option<Duration>) {
        if !self.settings.get().enabled {
            return (
                Snapshot::idle(State::Paused, "paused — run `wifilogin resume`"),
                None,
            );
        }
        if config.targets.is_empty() {
            return (
                Snapshot::idle(
                    State::NoTargets,
                    "no target connections configured — the daemon will not act",
                ),
                None,
            );
        }

        let network = match self.wifi.network_state().await {
            Ok(network) => {
                self.resync_pending = false;
                network
            }
            Err(error) => {
                let retry = (!self.resync_pending).then_some(SNAPSHOT_RESYNC_DELAY);
                self.resync_pending = true;
                return (
                    Snapshot::idle(
                        State::Error,
                        format!("could not read NetworkManager state: {error}"),
                    )
                    .with_error(error.to_string()),
                    retry,
                );
            }
        };

        match config.portal_permission(&network) {
            PortalPermission::Disconnected => {
                self.reset_portal_backoff();
                (
                    Snapshot::idle(
                        State::Disconnected,
                        "Wi-Fi disconnected — waiting for NetworkManager",
                    ),
                    None,
                )
            }
            PortalPermission::OtherNetwork(ssid) => {
                self.reset_portal_backoff();
                (
                    Snapshot::on_network(
                        State::OtherNetwork,
                        ssid.into(),
                        "not an allowed NetworkManager connection",
                    ),
                    None,
                )
            }
            PortalPermission::Connecting(ssid) => {
                self.reset_portal_backoff();
                (
                    Snapshot::on_network(
                        State::Connecting,
                        ssid.into(),
                        "target Wi-Fi is changing state — waiting for NetworkManager",
                    ),
                    None,
                )
            }
            PortalPermission::TargetNotDefault(ssid) => {
                self.reset_portal_backoff();
                (
                    Snapshot::on_network(
                        State::TargetNotDefault,
                        ssid.into(),
                        "target Wi-Fi is not the default route — not submitting credentials",
                    ),
                    None,
                )
            }
            PortalPermission::Allowed {
                ssid,
                connectivity: Connectivity::Full,
                ..
            } => {
                self.reset_portal_backoff();
                (
                    Snapshot::on_network(
                        State::Online,
                        ssid.into(),
                        "NetworkManager reports full connectivity",
                    ),
                    None,
                )
            }
            PortalPermission::Allowed {
                ssid,
                connection_uuid,
                connectivity: Connectivity::Portal,
                ..
            } => self.login(config, ssid.into(), connection_uuid).await,
            PortalPermission::Allowed {
                ssid, connectivity, ..
            } => {
                self.reset_portal_backoff();
                (
                    Snapshot::on_network(
                        State::WaitingForNetworkManager,
                        ssid.into(),
                        format!("NetworkManager connectivity is {connectivity:?} — waiting"),
                    ),
                    None,
                )
            }
        }
    }

    async fn login(
        &mut self,
        config: &Config,
        ssid: String,
        connection_uuid: &str,
    ) -> (Snapshot, Option<Duration>) {
        if let Some(retry) = &self.portal_retry {
            if retry.matches(&ssid, connection_uuid) {
                let now = Instant::now();
                if now < retry.deadline {
                    let remaining = retry.deadline.saturating_duration_since(now);
                    return (
                        Snapshot::on_network(
                            State::Captive,
                            ssid,
                            format!(
                                "{} — retrying in {}",
                                retry.message,
                                format_duration(remaining)
                            ),
                        )
                        .with_error(retry.message.clone()),
                        Some(remaining),
                    );
                }
                self.portal_retry = None;
            } else {
                self.reset_portal_backoff();
            }
        }

        let (username, password) = match self.creds.load().await {
            Ok(credentials) => {
                self.keyring_resync_pending = false;
                credentials
            }
            Err(error) if keyring::is_not_found(&error) => {
                return (
                    Snapshot::on_network(
                        State::CredentialsMissing,
                        ssid,
                        "credentials missing — run `wifilogin creds set <username>`",
                    )
                    .with_error("credentials missing"),
                    None,
                );
            }
            Err(error) => {
                let retry = (!self.keyring_resync_pending).then_some(SNAPSHOT_RESYNC_DELAY);
                self.keyring_resync_pending = true;
                return (
                    Snapshot::on_network(
                        State::Error,
                        ssid,
                        format!("could not load credentials: {error}"),
                    )
                    .with_error(error.to_string()),
                    retry,
                );
            }
        };

        let login = match self
            .portal
            .login(&config.portal_url, &username, &password)
            .await
        {
            Ok(login) => login,
            Err(error) => {
                return self.retry(
                    ssid,
                    connection_uuid,
                    format!("portal login failed: {error}"),
                );
            }
        };
        if login.outcome == Outcome::BadCredentials {
            self.reset_portal_backoff();
            return (
                Snapshot::on_network(
                    State::BadCredentials,
                    ssid,
                    "invalid portal credentials — update with `wifilogin creds set`",
                )
                .with_error("invalid credentials"),
                None,
            );
        }

        match self.portal.online(&config.connectivity_url).await {
            Ok(true) => {
                self.reset_portal_backoff();
                (
                    Snapshot::on_network(State::Online, ssid, "portal login verified"),
                    None,
                )
            }
            Ok(false) => self.retry(
                ssid,
                connection_uuid,
                "portal login did not establish connectivity".into(),
            ),
            Err(error) => self.retry(
                ssid,
                connection_uuid,
                format!("could not verify portal login: {error}"),
            ),
        }
    }

    fn retry(
        &mut self,
        ssid: String,
        connection_uuid: &str,
        message: String,
    ) -> (Snapshot, Option<Duration>) {
        let delay = self.portal_backoff.next();
        self.portal_retry = Some(PortalRetry {
            ssid: ssid.clone(),
            connection_uuid: connection_uuid.into(),
            deadline: Instant::now() + delay,
            message: message.clone(),
        });
        (
            Snapshot::on_network(
                State::Captive,
                ssid,
                format!("{message} — retrying in {}", format_duration(delay)),
            )
            .with_error(message),
            Some(delay),
        )
    }

    fn reset_portal_backoff(&mut self) {
        self.portal_retry = None;
        self.portal_backoff.reset();
    }
}

fn format_duration(duration: Duration) -> String {
    if duration.as_secs().is_multiple_of(60) {
        format!("{}m", duration.as_secs() / 60)
    } else {
        format!("{}s", duration.as_secs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wifi::NetworkState;
    use anyhow::Result;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    struct FakeWifi {
        state: Result<NetworkState, String>,
    }

    impl Wifi for FakeWifi {
        async fn network_state(&self) -> Result<NetworkState> {
            self.state.clone().map_err(anyhow::Error::msg)
        }
    }

    struct FakePortal {
        online: Arc<AtomicBool>,
        outcome: Mutex<Outcome>,
        login_count: Arc<AtomicUsize>,
    }

    impl Portal for FakePortal {
        async fn online(&self, _url: &str) -> Result<bool> {
            Ok(self.online.load(Ordering::SeqCst))
        }

        async fn login(
            &self,
            _url: &str,
            _username: &str,
            _password: &str,
        ) -> Result<crate::portal::LoginResult> {
            self.login_count.fetch_add(1, Ordering::SeqCst);
            Ok(crate::portal::LoginResult {
                outcome: *self.outcome.lock().unwrap(),
                http_status: 200,
            })
        }
    }

    struct FakeCreds;

    impl Creds for FakeCreds {
        async fn load(&self) -> Result<(String, String)> {
            Ok(("user".into(), "password".into()))
        }
    }

    fn settings() -> settings::Manager {
        let path = std::env::temp_dir().join(format!(
            "wifilogin-session-test-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        settings::Manager::from_path(path).unwrap()
    }

    fn config() -> Config {
        Config {
            targets: vec![crate::config::Target {
                ssid: "Campus".into(),
                connection_uuid: "d9428888-122b-11e1-b85c-61cd3cbb3210".into(),
            }],
            ..Config::default()
        }
    }

    fn network(
        ssid: &str,
        connection_uuid: &str,
        is_activated: bool,
        is_default: bool,
        connectivity: Connectivity,
    ) -> NetworkState {
        NetworkState::Connected {
            ssid: ssid.into(),
            connection_uuid: connection_uuid.into(),
            is_activated,
            is_default,
            connectivity,
        }
    }

    fn controller(
        state: Result<NetworkState, String>,
        connectivity_after_login: bool,
        outcome: Outcome,
    ) -> (
        Controller<FakeWifi, FakePortal, FakeCreds>,
        Arc<AtomicUsize>,
    ) {
        let login_count = Arc::new(AtomicUsize::new(0));
        (
            Controller::new(
                FakeWifi { state },
                FakePortal {
                    online: Arc::new(AtomicBool::new(connectivity_after_login)),
                    outcome: Mutex::new(outcome),
                    login_count: login_count.clone(),
                },
                FakeCreds,
                settings(),
            ),
            login_count,
        )
    }

    #[tokio::test]
    async fn ignores_a_phone_hotspot_or_any_other_ssid() {
        let (mut controller, login_count) = controller(
            Ok(network(
                "My Phone",
                "phone-profile",
                true,
                true,
                Connectivity::Full,
            )),
            true,
            Outcome::Granted,
        );
        let (snapshot, retry) = controller.step(&config()).await;
        assert_eq!(snapshot.state, State::OtherNetwork);
        assert_eq!(retry, None);
        assert_eq!(login_count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn requires_the_configured_networkmanager_profile() {
        let (mut controller, login_count) = controller(
            Ok(network(
                "Campus",
                "2f1c5a60-6e1f-4c42-b2a8-0d1a9d2f3e40",
                true,
                true,
                Connectivity::Portal,
            )),
            true,
            Outcome::Granted,
        );
        let (snapshot, _) = controller.step(&config()).await;
        assert_eq!(snapshot.state, State::OtherNetwork);
        assert_eq!(login_count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn does_not_login_until_the_connection_is_activated_and_default() {
        let (mut controller, login_count) = controller(
            Ok(network(
                "Campus",
                "d9428888-122b-11e1-b85c-61cd3cbb3210",
                false,
                false,
                Connectivity::Portal,
            )),
            true,
            Outcome::Granted,
        );
        let (snapshot, retry) = controller.step(&config()).await;
        assert_eq!(snapshot.state, State::Connecting);
        assert_eq!(retry, None);
        assert_eq!(login_count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn waits_for_networkmanager_before_posting_credentials() {
        let (mut controller, login_count) = controller(
            Ok(network(
                "Campus",
                "d9428888-122b-11e1-b85c-61cd3cbb3210",
                true,
                true,
                Connectivity::Unknown,
            )),
            true,
            Outcome::Granted,
        );
        let (snapshot, retry) = controller.step(&config()).await;
        assert_eq!(snapshot.state, State::WaitingForNetworkManager);
        assert_eq!(retry, None);
        assert_eq!(login_count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn bad_credentials_do_not_retry() {
        let (mut controller, _) = controller(
            Ok(network(
                "Campus",
                "d9428888-122b-11e1-b85c-61cd3cbb3210",
                true,
                true,
                Connectivity::Portal,
            )),
            false,
            Outcome::BadCredentials,
        );
        let (snapshot, retry) = controller.step(&config()).await;
        assert_eq!(snapshot.state, State::BadCredentials);
        assert_eq!(retry, None);
    }

    #[tokio::test]
    async fn retries_only_an_inconclusive_portal_result() {
        let (mut controller, login_count) = controller(
            Ok(network(
                "Campus",
                "d9428888-122b-11e1-b85c-61cd3cbb3210",
                true,
                true,
                Connectivity::Portal,
            )),
            false,
            Outcome::Granted,
        );
        let (snapshot, retry) = controller.step(&config()).await;
        assert_eq!(snapshot.state, State::Captive);
        assert_eq!(retry, Some(RETRY_BASE));

        // A D-Bus event may call step again immediately, but it must not
        // bypass the pending deadline and submit the form a second time.
        let (snapshot, retry) = controller.step(&config()).await;
        assert_eq!(snapshot.state, State::Captive);
        assert!(retry.is_some_and(|delay| delay <= RETRY_BASE));
        assert_eq!(login_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn transient_snapshot_failure_gets_one_resync() {
        let (mut controller, _) = controller(
            Err("D-Bus object disappeared".into()),
            false,
            Outcome::Granted,
        );
        let (_, first_retry) = controller.step(&config()).await;
        let (_, second_retry) = controller.step(&config()).await;
        assert_eq!(first_retry, Some(SNAPSHOT_RESYNC_DELAY));
        assert_eq!(second_retry, None);
    }
}
