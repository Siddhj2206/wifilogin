mod config;
mod keyring;
mod paths;
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
use tokio::net::UnixDatagram;
use tokio::time::Instant;
use wifi::{Manager, NetworkState};

#[derive(Parser)]
#[command(
    name = "wifilogin",
    version,
    about = "D-Bus-driven captive-portal login for NetworkManager on Linux",
    after_help = "Set up VIT Wi-Fi:\n  wifilogin setup\n\nThis stores your username and the R-VIT target list in config, prompts for a password in the system keyring, then starts the user service."
)]
struct Cli {
    /// Config file location (default: $XDG_CONFIG_HOME/wifilogin/config.toml)
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Set up VIT targets, portal username, password, and user service (replaces config).
    Setup {
        /// VIT username (prompted if omitted).
        username: Option<String>,
        /// Read the password from stdin instead of prompting securely.
        #[arg(long)]
        stdin: bool,
        /// Configure credentials but do not install or start the user service.
        #[arg(long)]
        no_service: bool,
    },
    /// Run the daemon (the default). Never connects to Wi-Fi itself.
    Run,
    /// Show the daemon's last state and current NetworkManager state.
    Status,
    /// Check HTTP connectivity now. Exit 0 online, 1 captive/offline, 2 error.
    Online,
    /// Submit credentials now on an allowed active Wi-Fi connection.
    Login,
    /// Pause automatic login; the daemon continues to observe no network state.
    Pause,
    /// Resume automatic login.
    Resume,
    /// Manage the portal username and password.
    Creds {
        #[command(subcommand)]
        operation: CredsOp,
    },
    /// Manage the configuration file.
    Config {
        #[command(subcommand)]
        operation: ConfigOp,
    },
    /// Manage the systemd user service.
    Service {
        #[command(subcommand)]
        operation: ServiceOp,
    },
}

#[derive(Subcommand)]
enum CredsOp {
    /// Store the username in config and password in the system keyring.
    Set {
        username: Option<String>,
        #[arg(long)]
        stdin: bool,
    },
    /// Show whether credentials are stored (never prints the password).
    Get,
    /// Delete stored credentials.
    Delete,
}

#[derive(Subcommand)]
enum ConfigOp {
    /// Print the configuration path.
    Path,
    /// Create a commented configuration file (refuses to overwrite).
    Init,
    /// Print the current configuration.
    Show,
    /// Open $EDITOR, validate the result, and tell the daemon to reload.
    Edit,
}

#[derive(Subcommand)]
enum ServiceOp {
    Install,
    Uninstall,
    Start,
    Stop,
    Restart,
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    if let Some(path) = cli.config {
        config::set_config_override(path);
    }

    match cli.command.unwrap_or(Command::Run) {
        Command::Setup {
            username,
            stdin,
            no_service,
        } => cmd_setup(username, stdin, no_service).await,
        Command::Run => run_daemon().await,
        Command::Status => cmd_status().await,
        Command::Online => cmd_online().await,
        Command::Login => cmd_login().await,
        Command::Pause => cmd_pause_resume(false),
        Command::Resume => cmd_pause_resume(true),
        Command::Creds { operation } => cmd_creds(operation).await,
        Command::Config { operation } => cmd_config(operation).await,
        Command::Service { operation } => match operation {
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
    match read_daemon_state()? {
        Some(state) if pid_alive(state.pid) => {
            println!(
                "daemon: running (pid {}, updated {} ago)",
                state.pid,
                age(state.updated)
            );
            println!("daemon state: {} — {}", state.state, state.message);
            if let Some(error) = state.last_error {
                println!("last error: {error}");
            }
        }
        Some(state) => println!(
            "daemon: not running (stale state from pid {}, updated {} ago)",
            state.pid,
            age(state.updated)
        ),
        None => println!("daemon: not running"),
    }

    let config = match config::load() {
        Ok(config) => Some(config),
        Err(error) => {
            println!("config: invalid or unavailable: {error}");
            None
        }
    };
    match Manager::new().await?.network_state().await? {
        NetworkState::Disconnected => println!("wifi: disconnected"),
        NetworkState::Connected {
            ssid,
            connection_uuid,
            is_default,
            connectivity,
            ..
        } => {
            let allowed = config
                .as_ref()
                .is_some_and(|config| config.is_target(&ssid));
            println!("wifi: {ssid} (allowed portal network: {allowed})");
            println!("NetworkManager connection: {connection_uuid}");
            println!(
                "default route: {}",
                if is_default {
                    "wifi"
                } else {
                    "another connection"
                }
            );
            println!("NetworkManager connectivity: {connectivity:?}");
        }
    }
    match (
        config
            .as_ref()
            .and_then(|config| config.username.as_deref()),
        keyring::load().await,
    ) {
        (Some(username), Ok(_)) => println!("creds: password present ({username})"),
        (None, Ok(_)) => println!("creds: password present; username missing from config"),
        (Some(username), Err(error)) if keyring::is_not_found(&error) => {
            println!("creds: password missing ({username})")
        }
        (None, Err(error)) if keyring::is_not_found(&error) => {
            println!("creds: username and password missing")
        }
        (_, Err(error)) => println!("creds: error: {error}"),
    }
    println!("config: {}", config::config_path()?.display());
    Ok(())
}

fn cmd_pause_resume(enabled: bool) -> Result<()> {
    let settings = settings::Manager::new()?;
    settings.set_enabled(enabled)?;
    settings::request_reload();
    println!(
        "automatic portal login {}",
        if enabled { "enabled" } else { "paused" }
    );
    Ok(())
}

async fn cmd_online() -> Result<()> {
    let config = config::load()?;
    match portal::PortalClient.online(&config.connectivity_url).await {
        Ok(true) => {
            println!("online");
            Ok(())
        }
        Ok(false) => {
            println!("captive/offline");
            std::process::exit(1);
        }
        Err(error) => {
            eprintln!("error: {error:#}");
            std::process::exit(2);
        }
    }
}

async fn cmd_login() -> Result<()> {
    let config = config::load()?;
    let authorized_connection = require_authorized_connection(&config).await?;
    let username = config
        .username
        .as_deref()
        .context("no username — run `wifilogin setup` or `wifilogin creds set <username>`")?;
    let password = keyring::load()
        .await
        .context("no password — run `wifilogin setup` or `wifilogin creds set <username>`")?;
    if require_authorized_connection(&config).await? != authorized_connection {
        anyhow::bail!("refusing portal login: the active Wi-Fi connection changed");
    }
    let portal = portal::PortalClient;
    println!("logging in to {} as {username}…", config.portal_url);
    let result = portal
        .login(&config.portal_url, username, &password)
        .await?;
    println!("portal: {} (HTTP {})", result.outcome, result.http_status);
    if result.outcome == portal::Outcome::BadCredentials {
        std::process::exit(1);
    }
    if portal
        .online(&config.connectivity_url)
        .await
        .unwrap_or(false)
    {
        println!("connectivity: online");
        Ok(())
    } else {
        println!("connectivity: still captive/offline");
        std::process::exit(1);
    }
}

/// The explicit command shares the daemon's network-identity guard. It may
/// bypass NetworkManager's `Portal` assessment, but never the target SSID,
/// activation, or default-route requirements.
async fn require_authorized_connection(config: &config::Config) -> Result<(String, String)> {
    let network = Manager::new().await?.network_state().await?;
    match config.portal_permission(&network) {
        config::PortalPermission::Allowed {
            ssid,
            connection_uuid,
            ..
        } => Ok((ssid.into(), connection_uuid.into())),
        config::PortalPermission::Disconnected => {
            anyhow::bail!("refusing portal login: Wi-Fi is disconnected");
        }
        config::PortalPermission::OtherNetwork(ssid) => {
            anyhow::bail!("refusing portal login: {ssid} is not an allowed target Wi-Fi network");
        }
        config::PortalPermission::Connecting(ssid) => {
            anyhow::bail!("refusing portal login: {ssid} is still changing state");
        }
        config::PortalPermission::TargetNotDefault(ssid) => {
            anyhow::bail!("refusing portal login: {ssid} is not the default route");
        }
    }
}

/// Configure the fixed VIT portal defaults without asking people to edit TOML.
/// This intentionally replaces an existing wifilogin configuration; passwords
/// are stored only in the system keyring.
async fn cmd_setup(username: Option<String>, stdin: bool, no_service: bool) -> Result<()> {
    let username = username.unwrap_or(prompt_username()?);
    let password = prompt_password(stdin)?;
    let path = config::setup(username.clone())?;
    keyring::store(&password)
        .await
        .context("store password in the system keyring")?;
    settings::request_reload();
    println!("configured VIT target R-VIT for {username}");
    println!("saved username to {}", path.display());
    println!("stored password in the system keyring");
    if no_service {
        println!("user service not installed (--no-service)");
        return Ok(());
    }
    service::install()
}

async fn cmd_creds(operation: CredsOp) -> Result<()> {
    match operation {
        CredsOp::Set { username, stdin } => {
            let username = username.unwrap_or(prompt_username()?);
            let password = prompt_password(stdin)?;
            let path = config::config_path()?;
            let mut config = if path.exists() {
                config::load()?
            } else {
                config::Config::default()
            };
            config.username = Some(username.clone());
            config::save(&config)?;
            keyring::store(&password).await?;
            settings::request_reload();
            println!("username saved to {}", path.display());
            println!("password stored in the system keyring for {username}");
        }
        CredsOp::Get => match keyring::load().await {
            Ok(_) => println!("password present in keyring"),
            Err(error) if keyring::is_not_found(&error) => println!("no password in keyring"),
            Err(error) => return Err(error),
        },
        CredsOp::Delete => {
            keyring::delete().await?;
            settings::request_reload();
            println!("password deleted from keyring (username remains in config)");
        }
    }
    Ok(())
}

fn prompt_username() -> Result<String> {
    print!("username: ");
    std::io::stdout().flush()?;
    let mut username = String::new();
    std::io::stdin().read_line(&mut username)?;
    let username = username.trim().to_string();
    if username.is_empty() {
        anyhow::bail!("username is required");
    }
    Ok(username)
}

fn prompt_password(from_stdin: bool) -> Result<String> {
    if from_stdin {
        let mut password = String::new();
        std::io::stdin().read_to_string(&mut password)?;
        let password = password.trim_end_matches(['\r', '\n']).to_string();
        if password.is_empty() {
            anyhow::bail!("empty password on stdin");
        }
        return Ok(password);
    }
    let password = rpassword::prompt_password("password: ")?;
    let confirmation = rpassword::prompt_password("confirm password: ")?;
    if password != confirmation {
        anyhow::bail!("passwords do not match");
    }
    if password.is_empty() {
        anyhow::bail!("password is required");
    }
    Ok(password)
}

async fn cmd_config(operation: ConfigOp) -> Result<()> {
    match operation {
        ConfigOp::Path => println!("{}", config::config_path()?.display()),
        ConfigOp::Init => {
            let path = config::init()?;
            println!("created {}", path.display());
            println!("run `wifilogin creds set <username>` to finish setup without editing it");
        }
        ConfigOp::Show => println!("{}", toml::to_string_pretty(&config::load()?)?),
        ConfigOp::Edit => {
            let path = config::config_path()?;
            if !path.exists() {
                config::init()?;
            }
            let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
            let status = std::process::Command::new(editor)
                .arg(&path)
                .status()
                .context("launch $EDITOR")?;
            if !status.success() {
                anyhow::bail!("editor exited with {status}");
            }
            config::load()?;
            settings::request_reload();
            println!("config reloaded: {}", path.display());
        }
    }
    Ok(())
}

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
}

async fn run_daemon() -> Result<()> {
    let mut config = config::load()?;
    let settings = settings::Manager::new()?;
    let wifi = Manager::new()
        .await
        .context("connect to NetworkManager (is it running and accessible?)")?;
    // Subscribe before reading state so a transition during startup is queued.
    let mut events = wifi
        .watch()
        .await
        .context("subscribe to NetworkManager D-Bus signals")?;
    let control = bind_control_socket().await?;
    let state_path = settings::daemon_state_path()?;
    let _ = std::fs::remove_file(&state_path);

    tracing::info!(targets = ?config.targets, enabled = settings.get().enabled, "starting daemon");
    let mut controller: DaemonController = Controller::new(
        wifi,
        portal::PortalClient,
        keyring::KeyringCreds,
        settings.clone(),
    );
    let mut previous_state = None;
    let mut next_retry = None;
    let mut should_step = true;
    let mut signal_buffer = [0; 32];
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

    let result = loop {
        if should_step {
            // `select!` drops `tick` when an event wins, cancelling an
            // in-flight HTTP request before a new network state is processed.
            tokio::select! {
                _ = sigterm.recv() => break Ok(()),
                _ = sigint.recv() => break Ok(()),
                received = control.recv(&mut signal_buffer) => {
                    received.context("receive control datagram")?;
                    reload(&mut config, &settings);
                }
                event = events.recv() => match event {
                    Some(_) => {}
                    None => break Err(anyhow::anyhow!("NetworkManager D-Bus event stream closed")),
                },
                retry = tick(&mut controller, &config, &state_path, &mut previous_state) => {
                    next_retry = retry;
                    should_step = false;
                }
            }
        } else {
            tokio::select! {
                _ = sigterm.recv() => break Ok(()),
                _ = sigint.recv() => break Ok(()),
                received = control.recv(&mut signal_buffer) => {
                    received.context("receive control datagram")?;
                    reload(&mut config, &settings);
                    should_step = true;
                }
                event = events.recv() => match event {
                    Some(_) => should_step = true,
                    None => break Err(anyhow::anyhow!("NetworkManager D-Bus event stream closed")),
                },
                _ = wait_until(next_retry) => should_step = true,
            }
        }
    };
    let _ = std::fs::remove_file(settings::control_socket_path()?);
    let _ = std::fs::remove_file(state_path);
    result
}

async fn bind_control_socket() -> Result<UnixDatagram> {
    let path = settings::control_socket_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    match UnixDatagram::bind(&path) {
        Ok(socket) => Ok(socket),
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
            // An unbound sender gets ECONNREFUSED for a stale filesystem
            // socket. Never delete a live daemon's control socket.
            let probe = std::os::unix::net::UnixDatagram::unbound()
                .and_then(|socket| socket.send_to(b"ping", &path));
            if probe.is_ok() {
                anyhow::bail!("another wifilogin daemon is already running");
            }
            std::fs::remove_file(&path)
                .with_context(|| format!("remove stale {}", path.display()))?;
            UnixDatagram::bind(&path).with_context(|| format!("bind {}", path.display()))
        }
        Err(error) => Err(error).with_context(|| format!("bind {}", path.display())),
    }
}

async fn wait_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending::<()>().await,
    }
}

fn reload(config: &mut config::Config, settings: &settings::Manager) {
    match config::load() {
        Ok(new_config) => *config = new_config,
        Err(error) => {
            tracing::warn!(%error, "configuration reload failed; keeping the previous configuration")
        }
    }
    if let Err(error) = settings.reload() {
        tracing::warn!(%error, "settings reload failed");
    }
}

async fn tick(
    controller: &mut DaemonController,
    config: &config::Config,
    state_path: &std::path::Path,
    previous_state: &mut Option<State>,
) -> Option<Instant> {
    let (snapshot, retry) = controller.step(config).await;
    if *previous_state != Some(snapshot.state) {
        tracing::info!(state = %snapshot.state, message = %snapshot.message, "daemon state changed");
    }
    if let Some(error) = &snapshot.last_error {
        tracing::warn!(%error, "daemon state error");
    }
    write_daemon_state(state_path, &snapshot);
    *previous_state = Some(snapshot.state);
    retry.map(|delay| Instant::now() + delay)
}

fn write_daemon_state(path: &std::path::Path, snapshot: &Snapshot) {
    let state = DaemonState {
        pid: std::process::id(),
        state: snapshot.state.to_string(),
        message: snapshot.message.clone(),
        ssid: snapshot.current_ssid.clone(),
        online: snapshot.is_online(),
        updated: unix_now(),
        last_error: snapshot.last_error.clone(),
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(data) = serde_json::to_string(&state) {
        let _ = std::fs::write(path, data);
    }
}

fn read_daemon_state() -> Result<Option<DaemonState>> {
    let path = settings::daemon_state_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(path).context("read daemon state")?;
    Ok(serde_json::from_str(&raw).ok())
}

fn pid_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

fn age(updated: u64) -> String {
    let seconds = unix_now().saturating_sub(updated);
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else {
        format!("{}h", seconds / 3600)
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}
