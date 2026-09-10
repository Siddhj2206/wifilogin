mod config;
mod keyring;
mod paths;
mod portal;
mod service;
mod session;
mod wifi;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use portal::Portal as _;
use session::{Controller, State};
use std::io::{Read, Write};
use tokio::time::Instant;
use wifi::{Manager, NetworkState};

#[derive(Parser)]
#[command(
    name = "wifilogin",
    version,
    about = "D-Bus-driven VIT captive-portal login for Linux",
    after_help = "Typical setup:\n  wifilogin setup --target R-VIT\n\nPasswords are prompted securely and stored only in the system keyring."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Configure portal username, password, and target Wi-Fi names.
    Setup {
        /// VIT username (uses the existing username or prompts if omitted).
        username: Option<String>,
        /// Wi-Fi name to allow. Repeat --target to configure more than one.
        #[arg(long)]
        target: Vec<String>,
        /// Read the password from stdin instead of prompting securely.
        #[arg(long)]
        stdin: bool,
        /// Configure credentials but do not install or update the user service.
        #[arg(long)]
        no_service: bool,
    },
    /// Manage the Wi-Fi names on which portal credentials may be submitted.
    Target {
        #[command(subcommand)]
        operation: TargetOp,
    },
    /// Run the daemon (the default). Never connects to Wi-Fi itself.
    Run,
    /// Show live NetworkManager state and local setup status.
    Status,
    /// Submit credentials now on an allowed active Wi-Fi connection.
    Login,
    /// Disable, stop, and remove the systemd user service.
    Uninstall,
}

#[derive(Subcommand)]
enum TargetOp {
    /// Show configured target Wi-Fi names.
    List,
    /// Add a Wi-Fi name to the allowlist.
    Add { ssid: String },
    /// Remove a Wi-Fi name from the allowlist.
    Remove { ssid: String },
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

    match Cli::parse().command.unwrap_or(Command::Run) {
        Command::Setup {
            username,
            target,
            stdin,
            no_service,
        } => cmd_setup(username, target, stdin, no_service).await,
        Command::Target { operation } => cmd_target(operation),
        Command::Run => run_daemon().await,
        Command::Status => cmd_status().await,
        Command::Login => cmd_login().await,
        Command::Uninstall => service::uninstall(),
    }
}

async fn cmd_setup(
    username: Option<String>,
    requested_targets: Vec<String>,
    stdin: bool,
    no_service: bool,
) -> Result<()> {
    // `setup` deliberately repairs obsolete or malformed local configuration.
    let existing = config::load().ok();
    let username = match username
        .or_else(|| existing.as_ref().and_then(|config| config.username.clone()))
    {
        Some(username) => username,
        None if stdin => {
            anyhow::bail!(
                "--stdin requires a username on first setup: `wifilogin setup <username> --target <SSID> --stdin`"
            );
        }
        None => prompt_username()?,
    };
    let targets = if requested_targets.is_empty() {
        match existing
            .as_ref()
            .filter(|config| !config.targets.is_empty())
            .map(|config| config.targets.clone())
        {
            Some(targets) => targets,
            None if stdin => {
                anyhow::bail!("--stdin requires --target <SSID> on first setup");
            }
            None => vec![prompt_target()?],
        }
    } else {
        normalize_targets(requested_targets)?
    };
    let password = prompt_password(stdin)?;
    let config = config::Config {
        targets,
        username: Some(username.clone()),
    };
    config::save(&config)?;
    keyring::store(&password)
        .await
        .context("store password in the system keyring")?;
    println!(
        "configured {} target(s) for {username}",
        config.targets.len()
    );
    if no_service {
        println!("user service not installed (--no-service)");
        return Ok(());
    }
    service::install()
}

fn cmd_target(operation: TargetOp) -> Result<()> {
    let mut config = config::load()?;
    match operation {
        TargetOp::List => {
            if config.targets.is_empty() {
                println!("no targets configured");
            } else {
                for target in config.targets {
                    println!("{target}");
                }
            }
            return Ok(());
        }
        TargetOp::Add { ssid } => {
            let ssid = normalize_target(ssid)?;
            if config.is_target(&ssid) {
                println!("target already configured: {ssid}");
                return Ok(());
            }
            config.targets.push(ssid);
        }
        TargetOp::Remove { ssid } => {
            let ssid = normalize_target(ssid)?;
            let old_len = config.targets.len();
            config.targets.retain(|target| target != &ssid);
            if config.targets.len() == old_len {
                anyhow::bail!("target not configured: {ssid}");
            }
        }
    }
    config::save(&config)?;
    println!("targets updated");
    service::restart_if_installed()
}

async fn cmd_status() -> Result<()> {
    let config = config::load()?;
    println!(
        "targets: {}",
        if config.targets.is_empty() {
            "none".to_string()
        } else {
            config.targets.join(", ")
        }
    );
    println!(
        "username: {}",
        config.username.as_deref().unwrap_or("not configured")
    );
    match keyring::load().await {
        Ok(_) => println!("password: present in keyring"),
        Err(error) if keyring::is_not_found(&error) => println!("password: missing"),
        Err(error) => println!("password: error: {error}"),
    }

    match Manager::new().await?.network_state().await? {
        NetworkState::Disconnected => println!("wifi: disconnected"),
        NetworkState::Connected {
            ssid,
            is_default,
            connectivity,
            ..
        } => {
            println!("wifi: {ssid} (target: {})", config.is_target(&ssid));
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
    Ok(())
}

async fn cmd_login() -> Result<()> {
    let config = config::load()?;
    let authorized_connection = require_authorized_connection(&config).await?;
    let username = config
        .username
        .as_deref()
        .context("no username — run `wifilogin setup`")?;
    let password = keyring::load()
        .await
        .context("no password — run `wifilogin setup`")?;
    if require_authorized_connection(&config).await? != authorized_connection {
        anyhow::bail!("refusing portal login: the active Wi-Fi connection changed");
    }
    let portal = portal::PortalClient;
    println!("logging in as {username}…");
    let result = portal.login(username, &password).await?;
    println!("portal: {} (HTTP {})", result.outcome, result.http_status);
    if result.outcome == portal::Outcome::BadCredentials {
        std::process::exit(1);
    }
    if portal.online().await.unwrap_or(false) {
        println!("connectivity: online");
        Ok(())
    } else {
        println!("connectivity: still captive/offline");
        std::process::exit(1);
    }
}

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

fn normalize_targets(targets: Vec<String>) -> Result<Vec<String>> {
    let mut normalized = Vec::new();
    for target in targets {
        let target = normalize_target(target)?;
        if !normalized.contains(&target) {
            normalized.push(target);
        }
    }
    Ok(normalized)
}

fn normalize_target(target: String) -> Result<String> {
    let target = target.trim().to_string();
    if target.is_empty() {
        anyhow::bail!("target Wi-Fi name must not be empty");
    }
    Ok(target)
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

fn prompt_target() -> Result<String> {
    print!("target Wi-Fi name: ");
    std::io::stdout().flush()?;
    let mut target = String::new();
    std::io::stdin().read_line(&mut target)?;
    normalize_target(target)
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

type DaemonController = Controller<Manager, portal::PortalClient, keyring::KeyringCreds>;

async fn run_daemon() -> Result<()> {
    let config = config::load()?;
    let wifi = Manager::new()
        .await
        .context("connect to NetworkManager (is it running and accessible?)")?;
    let mut events = wifi
        .watch()
        .await
        .context("subscribe to NetworkManager D-Bus signals")?;
    let mut controller: DaemonController =
        Controller::new(wifi, portal::PortalClient, keyring::KeyringCreds);
    let mut previous_state = None;
    let mut next_retry = None;
    let mut should_step = true;
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

    loop {
        if should_step {
            tokio::select! {
                _ = sigterm.recv() => return Ok(()),
                _ = sigint.recv() => return Ok(()),
                event = events.recv() => match event {
                    Some(_) => {}
                    None => anyhow::bail!("NetworkManager D-Bus event stream closed"),
                },
                retry = tick(&mut controller, &config, &mut previous_state) => {
                    next_retry = retry;
                    should_step = false;
                }
            }
        } else {
            tokio::select! {
                _ = sigterm.recv() => return Ok(()),
                _ = sigint.recv() => return Ok(()),
                event = events.recv() => match event {
                    Some(_) => should_step = true,
                    None => anyhow::bail!("NetworkManager D-Bus event stream closed"),
                },
                _ = wait_until(next_retry) => should_step = true,
            }
        }
    }
}

async fn wait_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending::<()>().await,
    }
}

async fn tick(
    controller: &mut DaemonController,
    config: &config::Config,
    previous_state: &mut Option<State>,
) -> Option<Instant> {
    let (snapshot, retry) = controller.step(config).await;
    if *previous_state != Some(snapshot.state) {
        tracing::info!(state = %snapshot.state, message = %snapshot.message, "daemon state changed");
    }
    if let Some(error) = &snapshot.last_error {
        tracing::warn!(%error, "daemon state error");
    }
    *previous_state = Some(snapshot.state);
    retry.map(|delay| Instant::now() + delay)
}
