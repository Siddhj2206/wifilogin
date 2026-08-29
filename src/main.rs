mod config;
mod keyring;
mod portal;
mod service;
mod session;
mod settings;
mod wifi;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use portal::Portal as _;
use serde::{Deserialize, Serialize};
use session::{Controller, Snapshot, State};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc::Receiver;
use tokio::time::Instant;
use tracing_subscriber::EnvFilter;
use wifi::Manager;

/// How often the daemon retries establishing the D-Bus watch when in
/// polling-fallback mode.
const WATCH_RETRY: Duration = Duration::from_secs(30);
/// Let NM settle after a wake event before acting on it.
const EVENT_DEBOUNCE: Duration = Duration::from_millis(400);

#[derive(Parser)]
#[command(
    name = "wifilogin",
    version,
    about = "Event-driven captive-portal auto-login for NetworkManager (Linux)",
    after_help = "Typical setup:\n  wifilogin config init\n  wifilogin creds set myuser\n  wifilogin status"
)]
struct Cli {
    /// Config file location (default: $XDG_CONFIG_HOME/wifilogin/config.toml)
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon (default command). Event-driven; only acts on target SSIDs.
    Run,
    /// Show daemon, wifi, credentials and config status
    Status,
    /// Check internet connectivity. Exit 0 = online, 1 = captive/offline, 2 = error.
    Online,
    /// Perform a portal login right now, then check connectivity
    Login,
    /// Activate the saved NetworkManager profile for a target SSID
    Ensure {
        /// SSID to ensure (defaults to the first configured target)
        ssid: Option<String>,
    },
    /// Pause auto-login (daemon stays idle until resumed)
    Pause,
    /// Resume auto-login
    Resume,
    /// Manage portal credentials in the system keyring
    Creds {
        #[command(subcommand)]
        op: CredsOp,
    },
    /// Manage the config file
    Config {
        #[command(subcommand)]
        op: ConfigOp,
    },
    /// Manage the systemd user service (install, start, stop, …)
    Service {
        #[command(subcommand)]
        op: ServiceOp,
    },
}

#[derive(Subcommand)]
enum CredsOp {
    /// Store credentials. Password is prompted for securely unless --stdin is given.
    Set {
        /// Portal username (prompted if omitted)
        username: Option<String>,
        /// Read the password from stdin instead of an interactive prompt
        #[arg(long)]
        stdin: bool,
    },
    /// Show whether credentials are stored (never prints the password)
    Get,
    /// Delete stored credentials
    Delete,
}

#[derive(Subcommand)]
enum ConfigOp {
    /// Print the config file path
    Path,
    /// Create a commented config file (refuses to overwrite)
    Init,
    /// Print the current config
    Show,
    /// Open the config in $EDITOR and validate it afterwards
    Edit,
}

#[derive(Subcommand)]
enum ServiceOp {
    /// Install the user unit and enable + start it (idempotent; re-run after upgrades)
    Install,
    /// Stop, disable, and remove the unit (credentials and config are kept)
    Uninstall,
    /// Start the service
    Start,
    /// Stop the service
    Stop,
    /// Restart the service (e.g. after changing config that needs a fresh start)
    Restart,
    /// Show the systemd unit status
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Die silently on SIGPIPE (e.g. `wifilogin status | head`) instead of
    // panicking when the pipe closes — standard unix tool behavior.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    if let Some(path) = cli.config {
        config::set_config_override(path);
    }
    let cmd = cli.cmd.unwrap_or(Command::Run);

    match cmd {
        Command::Run => run_daemon().await,
        Command::Status => cmd_status().await,
        Command::Online => cmd_online().await,
        Command::Login => cmd_login().await,
        Command::Ensure { ssid } => cmd_ensure(ssid).await,
        Command::Pause => cmd_pause_resume(false).await,
        Command::Resume => cmd_pause_resume(true).await,
        Command::Creds { op } => match op {
            CredsOp::Set { username, stdin } => cmd_creds_set(username, stdin).await,
            CredsOp::Get => {
                match keyring::load().await {
                    Ok((u, _)) => println!("credentials present for {u}"),
                    Err(e) if keyring::is_not_found(&e) => println!("no credentials in keyring"),
                    Err(e) => return Err(e),
                }
                Ok(())
            }
            CredsOp::Delete => {
                keyring::delete().await?;
                println!("credentials deleted");
                settings::wake_daemon();
                Ok(())
            }
        },
        Command::Config { op } => match op {
            ConfigOp::Path => {
                println!("{}", config::config_path()?.display());
                Ok(())
            }
            ConfigOp::Init => {
                let path = config::init()?;
                println!("created {}", path.display());
                println!("edit it, then run `wifilogin creds set <username>`");
                Ok(())
            }
            ConfigOp::Show => {
                let cfg = config::load()?;
                println!("{}", toml::to_string_pretty(&cfg)?);
                Ok(())
            }
            ConfigOp::Edit => cmd_config_edit().await,
        },
        Command::Service { op } => match op {
            ServiceOp::Install => service::install(),
            ServiceOp::Uninstall => service::uninstall(),
            ServiceOp::Start => service::start(),
            ServiceOp::Stop => service::stop(),
            ServiceOp::Restart => service::restart(),
            ServiceOp::Status => service::status(),
        },
    }
}

async fn cmd_status() -> Result<()> {
    // Daemon health first — that's the question people actually have.
    match read_daemon_state()? {
        None => println!("daemon: not running (`wifilogin run` to start)"),
        Some(st) => {
            let age = unix_now().saturating_sub(st.updated);
            let age = humantime::format_duration(Duration::from_secs(age)).to_string();
            if pid_alive(st.pid) {
                println!("daemon: running (pid {}, last update {age} ago)", st.pid);
            } else {
                println!(
                    "daemon: NOT running (stale state from pid {}, last update {age} ago)",
                    st.pid
                );
            }
            println!("daemon state: {} — {}", st.state, st.message);
            if let Some(e) = &st.last_error {
                println!("last error: {e}");
            }
        }
    }

    // Live wifi state
    let cfg = match config::load() {
        Ok(c) => Some(c),
        Err(e) => {
            println!("config: {e}");
            None
        }
    };

    let wifi = wifi::Manager::new().await.ok();
    let ssid = match &wifi {
        Some(w) => w.current_ssid().await.unwrap_or(None),
        None => None,
    };
    match (&ssid, &cfg) {
        (None, _) => println!("wifi: disconnected"),
        (Some(s), Some(c)) => {
            let on = c.is_target(s);
            println!("wifi: {s} (target: {on})");
            if on {
                let online = portal::PortalClient
                    .online(&c.connectivity_url)
                    .await
                    .unwrap_or(false);
                println!("connectivity: {}", if online { "online" } else { "captive" });
            }
        }
        (Some(s), None) => println!("wifi: {s}"),
    }

    match keyring::load().await {
        Ok((u, _)) => println!("creds: present ({u})"),
        Err(e) if keyring::is_not_found(&e) => println!("creds: missing (`wifilogin creds set`)"),
        Err(e) => println!("creds: error: {e}"),
    }

    println!("config: {}", config::config_path()?.display());
    println!("state:  {}", settings::settings_path()?.display());
    Ok(())
}

async fn cmd_pause_resume(enabled: bool) -> Result<()> {
    let mgr = settings::Manager::new()?;
    mgr.set_enabled(enabled)?;
    let verb = if enabled { "resumed" } else { "paused" };
    println!("{verb} — auto-login {}", if enabled { "enabled" } else { "disabled" });
    settings::wake_daemon();
    Ok(())
}

async fn cmd_online() -> Result<()> {
    let cfg = config::load()?;
    match portal::PortalClient.online(&cfg.connectivity_url).await {
        Ok(true) => {
            println!("online");
            std::process::exit(0);
        }
        Ok(false) => {
            println!("captive/offline");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::exit(2);
        }
    }
}

async fn cmd_login() -> Result<()> {
    let cfg = config::load()?;
    let (u, p) = keyring::load()
        .await
        .context("no credentials — run `wifilogin creds set <username>`")?;
    let client = portal::PortalClient;
    println!("logging in to {} as {u}…", cfg.portal_url);
    let res = client.login(&cfg.portal_url, &u, &p).await?;
    println!("portal: {} (HTTP {})", res.outcome, res.http_status);
    if res.outcome == portal::Outcome::BadCredentials {
        std::process::exit(1);
    }
    let online = client.online(&cfg.connectivity_url).await.unwrap_or(false);
    if online {
        println!("connectivity: online");
        Ok(())
    } else {
        println!("connectivity: still captive/offline");
        std::process::exit(1);
    }
}

async fn cmd_ensure(ssid: Option<String>) -> Result<()> {
    let cfg = config::load()?;
    let target = match ssid {
        Some(s) => s,
        None => cfg
            .targets
            .first()
            .cloned()
            .context("no targets configured")?,
    };
    let wifi = wifi::Manager::new().await?;
    let r = wifi.ensure_connected(&target).await?;
    if r.already_connected {
        println!("already on {target}");
    } else {
        println!("connected to {} ({})", target, r.connection_id);
    }
    Ok(())
}

async fn cmd_creds_set(username: Option<String>, stdin: bool) -> Result<()> {
    let username = match username {
        Some(u) => u,
        None => {
            print!("username: ");
            std::io::stdout().flush()?;
            let mut buf = String::new();
            std::io::stdin().read_line(&mut buf)?;
            buf.trim().to_string()
        }
    };

    let password = if stdin {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        let p = buf.trim_end_matches(['\r', '\n']).to_string();
        if p.is_empty() {
            anyhow::bail!("empty password on stdin");
        }
        p
    } else {
        let p = rpassword::prompt_password("password: ")?;
        let confirm = rpassword::prompt_password("confirm password: ")?;
        if p != confirm {
            anyhow::bail!("passwords do not match");
        }
        p
    };

    keyring::store(&username, &password).await?;
    println!("credentials stored for {username}");
    settings::wake_daemon();
    Ok(())
}

async fn cmd_config_edit() -> Result<()> {
    let path = config::config_path()?;
    if !path.exists() {
        config::init()?;
    }
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
    let status = std::process::Command::new(editor)
        .arg(&path)
        .status()
        .context("failed to launch $EDITOR")?;
    if !status.success() {
        anyhow::bail!("editor exited with {status}");
    }
    // Validate what the user wrote.
    config::load()?;
    println!("config ok: {}", path.display());
    settings::wake_daemon();
    Ok(())
}

// ---------------------------------------------------------------------------
// Daemon
// ---------------------------------------------------------------------------

type DaemonController = Controller<Manager, portal::PortalClient, keyring::KeyringCreds>;

#[derive(Serialize, Deserialize)]
struct DaemonState {
    pid: u32,
    state: String,
    message: String,
    ssid: Option<String>,
    online: bool,
    updated: u64,
    last_error: Option<String>,
    last_login: Option<String>,
}

async fn run_daemon() -> Result<()> {
    let mut cfg = config::load_or_create()?;
    let settings_mgr = settings::Manager::new()?;
    let wifi = Manager::new()
        .await
        .context("init wifi manager (is NetworkManager running and accessible?)")?;

    let state_path = settings::daemon_state_path()?;
    // Stale state from a previous run means nothing while we're alive.
    let _ = std::fs::remove_file(&state_path);

    tracing::info!(
        targets = ?cfg.targets,
        portal = %cfg.portal_url,
        verify = ?cfg.verify_interval,
        retry = ?cfg.retry_interval,
        enabled = settings_mgr.get().enabled,
        "starting wifilogin daemon"
    );

    let mut controller: DaemonController = Controller::new(
        wifi.clone(),
        portal::PortalClient,
        keyring::KeyringCreds,
        settings_mgr.clone(),
        cfg.retry_interval,
    );

    // D-Bus event channel — None if the watch fails, then we fall back to
    // polling and periodically retry establishing the watch.
    let mut events = try_watch(&wifi).await;
    let mut next_watch_attempt = Instant::now() + WATCH_RETRY;

    // Watch config, settings and wake files — reload + step on any change.
    let (fs_tx, mut fs_rx) = tokio::sync::mpsc::channel::<()>(4);
    let watch_paths: Vec<PathBuf> = [
        config::config_path().ok(),
        settings::settings_path().ok(),
        settings::wake_path().ok(),
    ]
    .into_iter()
    .flatten()
    .collect();
    let _fs_watcher = spawn_fs_watcher(watch_paths, fs_tx);

    let mut prev_state: Option<State> = None;

    // Signals
    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;

    // Initial step
    let mut next_retry = tick(&mut controller, &cfg, &state_path, &mut prev_state).await;

    loop {
        let has_events = events.is_some();
        let sleep_fut = async {
            let watch_deadline = (!has_events).then_some(next_watch_attempt);
            let deadline = match (next_retry, watch_deadline) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            match deadline {
                Some(d) => tokio::time::sleep_until(d).await,
                None => std::future::pending::<()>().await,
            }
        };

        tokio::select! {
            _ = sigterm.recv() => {
                tracing::info!("SIGTERM — shutting down");
                break;
            }
            _ = sigint.recv() => {
                tracing::info!("SIGINT — shutting down");
                break;
            }
            _ = sighup.recv() => {
                tracing::info!("SIGHUP — reloading config & settings");
                reload(&mut cfg, &settings_mgr);
                next_retry = tick(&mut controller, &cfg, &state_path, &mut prev_state).await;
            }
            _ = fs_rx.recv() => {
                // Coalesce bursts of filesystem events into one reload.
                while fs_rx.try_recv().is_ok() {}
                tracing::info!("config/settings/wake file changed — reloading");
                reload(&mut cfg, &settings_mgr);
                next_retry = tick(&mut controller, &cfg, &state_path, &mut prev_state).await;
            }
            ev = async {
                if let Some(rx) = events.as_mut() {
                    rx.recv().await
                } else {
                    std::future::pending().await
                }
            } => {
                match ev {
                    Some(_) => {
                        tokio::time::sleep(EVENT_DEBOUNCE).await;
                        next_retry = tick(&mut controller, &cfg, &state_path, &mut prev_state).await;
                    }
                    None => {
                        tracing::warn!("D-Bus watch channel closed — falling back to polling");
                        events = None;
                        next_watch_attempt = Instant::now() + WATCH_RETRY;
                        next_retry = Some(Instant::now() + cfg.verify_interval);
                    }
                }
            }
            _ = sleep_fut => {
                let now = Instant::now();
                if !has_events && now >= next_watch_attempt {
                    next_watch_attempt = now + WATCH_RETRY;
                    if let Some(rx) = try_watch(&wifi).await {
                        tracing::info!("D-Bus watch re-established — event-driven again");
                        events = Some(rx);
                    }
                }
                next_retry = tick(&mut controller, &cfg, &state_path, &mut prev_state).await;
            }
        }
    }

    let _ = std::fs::remove_file(&state_path);
    Ok(())
}

async fn try_watch(wifi: &Manager) -> Option<Receiver<wifi::Event>> {
    match wifi.watch().await {
        Ok(rx) => Some(rx),
        Err(e) => {
            tracing::warn!(error = %e, "D-Bus watch unavailable — polling fallback");
            None
        }
    }
}

fn reload(cfg: &mut config::Config, settings_mgr: &settings::Manager) {
    match config::load_or_create() {
        Ok(c) => *cfg = c,
        Err(e) => tracing::warn!(error = %e, "config reload failed — keeping previous config"),
    }
    if let Err(e) = settings_mgr.reload() {
        tracing::warn!(error = %e, "settings reload failed");
    }
}

async fn tick(
    controller: &mut DaemonController,
    cfg: &config::Config,
    state_path: &std::path::Path,
    prev_state: &mut Option<State>,
) -> Option<Instant> {
    let (snap, retry) = controller.step(cfg).await;
    log_snapshot(&snap);
    write_daemon_state(state_path, &snap);
    maybe_notify(*prev_state, &snap);
    *prev_state = Some(snap.state);
    retry.map(|d| Instant::now() + d)
}

fn log_snapshot(s: &Snapshot) {
    tracing::info!(
        state = %s.state,
        msg = %s.message,
        ssid = ?s.current_ssid,
        on_target = s.on_target,
        online = s.online,
        "step"
    );
    if let Some(e) = &s.last_error {
        tracing::warn!(error = %e, "last error");
    }
    if let Some(l) = &s.last_login {
        tracing::info!(outcome = %l.outcome, http = l.http_status, "last login");
        if tracing::enabled!(tracing::Level::DEBUG) {
            tracing::debug!(snippet = %l.body_snippet, "portal response snippet");
        }
    }
}

fn write_daemon_state(path: &std::path::Path, s: &Snapshot) {
    let st = DaemonState {
        pid: std::process::id(),
        state: s.state.to_string(),
        message: s.message.clone(),
        ssid: s.current_ssid.clone(),
        online: s.online,
        updated: unix_now(),
        last_error: s.last_error.clone(),
        last_login: s.last_login.as_ref().map(|l| l.outcome.to_string()),
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(data) = serde_json::to_string(&st) {
        let _ = std::fs::write(path, data);
    }
}

fn read_daemon_state() -> Result<Option<DaemonState>> {
    let path = settings::daemon_state_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    Ok(serde_json::from_str(&raw).ok())
}

fn pid_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Desktop notification on state *transitions* only, so a persistent problem
/// never spams. Silent if notify-send isn't installed.
fn maybe_notify(prev: Option<State>, snap: &Snapshot) {
    if prev == Some(snap.state) {
        return;
    }
    match snap.state {
        State::BadCredentials => notify_send(
            "WiFi login failed",
            "Invalid username or password — run `wifilogin creds set`",
        ),
        State::CredentialsMissing => notify_send(
            "WiFi login not configured",
            "Run `wifilogin creds set <username>` to store credentials",
        ),
        State::Captive => notify_send("WiFi portal login failed", &snap.message),
        State::NeedsProvision => notify_send("wifilogin", &snap.message),
        State::Online => {
            let recovered = matches!(
                prev,
                Some(State::BadCredentials
                    | State::CredentialsMissing
                    | State::Captive
                    | State::Error)
            );
            if recovered {
                notify_send("WiFi online", "captive portal authenticated");
            }
        }
        _ => {}
    }
}

fn notify_send(summary: &str, body: &str) {
    tracing::info!(summary, body, "notification");
    let _ = std::process::Command::new("notify-send")
        .args(["-a", "wifilogin", summary, body])
        .spawn();
}

fn spawn_fs_watcher(
    paths: Vec<PathBuf>,
    tx: tokio::sync::mpsc::Sender<()>,
) -> Option<notify::RecommendedWatcher> {
    use notify::{EventKind, RecursiveMode, Watcher};
    if paths.is_empty() {
        return None;
    }
    let watched = paths.clone();
    let mut watcher = notify::recommended_watcher(
        move |res: Result<notify::Event, notify::Error>| {
            if let Ok(ev) = res
                && matches!(
                    ev.kind,
                    EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
                )
                && ev.paths.iter().any(|p| watched.iter().any(|w| w == p))
            {
                let _ = tx.blocking_send(());
            }
        },
    )
    .ok()?;

    // Watch parent dirs so newly created files (config init, wake touch) are
    // seen too.
    let mut parents: Vec<PathBuf> = Vec::new();
    for p in paths {
        if let Some(parent) = p.parent()
            && !parents.contains(&parent.to_path_buf())
        {
            parents.push(parent.to_path_buf());
        }
    }
    for parent in parents {
        let _ = watcher.watch(&parent, RecursiveMode::NonRecursive);
    }
    Some(watcher)
}
