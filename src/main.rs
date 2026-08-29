mod config;
mod keyring;
mod portal;
mod session;
mod settings;
mod wifi;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::time::Duration;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "wifilogin",
    version,
    about = "Efficient auto-login for R-VIT captive portal — event-driven, only on target SSIDs"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon (default). Efficient: sleeps until D-Bus WiFi event or `verify_interval` while on target.
    Run,
    /// Show current WiFi and target status
    Status,
    /// Check connectivity (204)
    Online,
    /// Force portal login now (uses keyring + config)
    Login,
    /// Ensure connection to a target (activates saved NM profile)
    Ensure {
        /// SSID to ensure (defaults to first target)
        ssid: Option<String>,
    },
    /// Pause auto-login (daemon stays idle until resumed)
    Pause,
    /// Resume auto-login
    Resume,
    /// Manage credentials in system keyring
    Creds {
        #[command(subcommand)]
        op: CredsOp,
    },
    /// Print or init config file
    Config {
        #[command(subcommand)]
        op: ConfigOp,
    },
}

#[derive(Subcommand)]
enum CredsOp {
    Set { username: String, password: String },
    Get,
    Delete,
}

#[derive(Subcommand)]
enum ConfigOp {
    Path,
    Init,
    Show,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse().unwrap()))
        .init();

    let cli = Cli::parse();
    let cmd = cli.cmd.unwrap_or(Command::Run);

    match cmd {
        Command::Run => run_daemon().await,
        Command::Status => cmd_status().await,
        Command::Online => cmd_online().await,
        Command::Login => cmd_login().await,
        Command::Ensure { ssid } => cmd_ensure(ssid).await,
        Command::Pause => cmd_pause().await,
        Command::Resume => cmd_resume().await,
        Command::Creds { op } => match op {
            CredsOp::Set { username, password } => {
                keyring::store(&username, &password).await?;
                println!("credentials stored for {username}");
                // Try to wake daemon right away (if running) so it retries immediately
                try_wake_daemon();
                Ok(())
            }
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
                Ok(())
            }
        },
        Command::Config { op } => match op {
            ConfigOp::Path => {
                println!("{}", config::config_path()?.display());
                Ok(())
            }
            ConfigOp::Init => {
                let cfg = config::load()?;
                println!(
                    "config at {} — targets={:?}",
                    config::config_path()?.display(),
                    cfg.targets
                );
                Ok(())
            }
            ConfigOp::Show => {
                let cfg = config::load()?;
                println!("{}", toml::to_string_pretty(&cfg)?);
                Ok(())
            }
        },
    }
}

async fn cmd_status() -> Result<()> {
    let cfg = config::load()?;
    let wifi = wifi::Manager::new().await?;
    let cur = wifi.current_ssid().await?;
    let settings = settings::Manager::new().unwrap_or_else(|_| {
        // fallback to enabled if settings can't be loaded
        settings::Manager::new().expect("settings fallback")
    });
    let enabled = settings.get().enabled;
    println!("targets: {}", cfg.targets.join(", "));
    println!("portal: {}", cfg.portal_url);
    println!(
        "enabled: {} ({} to toggle)",
        enabled,
        if enabled { "pause" } else { "resume" }
    );
    match cur {
        None => println!("wifi: disconnected"),
        Some(ssid) => {
            let on = cfg.is_target(&ssid);
            println!("wifi: {ssid} (on_target={on})");
            if on {
                let online = portal::online().await.unwrap_or(false);
                println!("online: {online}");
            }
        }
    }
    match keyring::load().await {
        Ok((u, _)) => println!("creds: present ({u})"),
        Err(e) if keyring::is_not_found(&e) => println!("creds: missing"),
        Err(e) => println!("creds: error {e}"),
    }
    println!("settings: {}", settings.path().display());
    Ok(())
}

async fn cmd_pause() -> Result<()> {
    let mgr = settings::Manager::new()?;
    mgr.set_enabled(false)?;
    println!("paused — auto-login disabled");
    println!("settings at {}", mgr.path().display());
    try_wake_daemon();
    Ok(())
}

async fn cmd_resume() -> Result<()> {
    let mgr = settings::Manager::new()?;
    mgr.set_enabled(true)?;
    println!("resumed — auto-login enabled");
    println!("settings at {}", mgr.path().display());
    try_wake_daemon();
    Ok(())
}

async fn cmd_online() -> Result<()> {
    let online = portal::online().await?;
    if online {
        println!("online");
    } else {
        println!("offline/captive");
    }
    Ok(())
}

async fn cmd_login() -> Result<()> {
    let cfg = config::load()?;
    let (u, p) = keyring::load()
        .await
        .context("load credentials; run `wifilogin creds set <user> <pass>`")?;
    let res = portal::login(&cfg.portal_url, &u, &p).await?;
    println!(
        "outcome={} http={} snippet={:?}",
        res.outcome,
        res.http_status,
        &res.body_snippet[..res.body_snippet.len().min(200)]
    );
    let online = portal::online().await.unwrap_or(false);
    println!("online={online}");
    Ok(())
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
        println!("activating {target} ({})", r.connection_id);
    }
    Ok(())
}

fn try_wake_daemon() {
    // Best-effort: try to send SIGHUP to running daemon so it retries immediately.
    // Works for both manual `wifilogin run` and systemd user service.
    let _ = std::process::Command::new("pkill")
        .args(["-HUP", "-x", "wifilogin"])
        .output();
    // Also try systemd user kill (if under systemd)
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "kill", "-s", "HUP", "wifilogin.service"])
        .output();
}

async fn run_daemon() -> Result<()> {
    let cfg = config::load()?;
    cfg.validate()?;
    let settings_mgr = settings::Manager::new()?;
    tracing::info!(
        targets = ?cfg.targets,
        portal = %cfg.portal_url,
        verify = ?cfg.verify_interval,
        enabled = settings_mgr.get().enabled,
        settings = %settings_mgr.path().display(),
        "starting wifilogin daemon"
    );

    let wifi = wifi::Manager::new().await.context("init wifi manager")?;
    let controller = session::Controller::new(&cfg, &wifi, &settings_mgr);

    // D-Bus event channel — None if no wifi device / watch fails, then we fallback to polling
    let mut events: Option<tokio::sync::mpsc::Receiver<wifi::Event>> = match wifi.watch().await {
        Ok(rx) => {
            tracing::info!(
                "watching NetworkManager D-Bus for SSID/State changes (event-driven, no polling)"
            );
            Some(rx)
        }
        Err(e) => {
            tracing::warn!(error = %e, "D-Bus watch failed — falling back to polling every verify_interval");
            None
        }
    };

    // Watch settings file for changes (pause/resume) — event-driven, not polling
    let (settings_tx, mut settings_rx) = tokio::sync::mpsc::channel::<()>(4);
    let settings_path = settings_mgr.path().clone();
    let _watcher = spawn_settings_watcher(settings_path, settings_tx);

    // Initial step immediately
    let (snap, retry) = controller.step().await;
    log_snapshot(&snap);
    let mut next_retry = retry.map(|d| tokio::time::Instant::now() + d);

    // Signal handling
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    let mut sigusr1 =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())?;

    loop {
        // Build sleep future: None = sleep forever until event/signal
        let sleep_fut = async {
            if let Some(deadline) = next_retry {
                tokio::time::sleep_until(deadline).await;
            } else {
                std::future::pending::<()>().await;
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
                tracing::info!("SIGHUP — reload & retry (creds/settings changed)");
                let _ = settings_mgr.reload();
                let (snap, retry) = controller.step().await;
                log_snapshot(&snap);
                next_retry = retry.map(|d| tokio::time::Instant::now() + d);
            }
            _ = sigusr1.recv() => {
                tracing::info!("SIGUSR1 — retry");
                let (snap, retry) = controller.step().await;
                log_snapshot(&snap);
                next_retry = retry.map(|d| tokio::time::Instant::now() + d);
            }
            _ = settings_rx.recv() => {
                tracing::info!("settings changed — reload & retry");
                let _ = settings_mgr.reload();
                let (snap, retry) = controller.step().await;
                log_snapshot(&snap);
                next_retry = retry.map(|d| tokio::time::Instant::now() + d);
            }
            ev = async {
                if let Some(rx) = events.as_mut() {
                    rx.recv().await
                } else {
                    std::future::pending().await
                }
            } => {
                if let Some(ev) = ev {
                    tracing::info!(?ev, "wifi event — waking");
                    // Debounce: small delay to let NM settle after event
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    let (snap, retry) = controller.step().await;
                    log_snapshot(&snap);
                    next_retry = retry.map(|d| tokio::time::Instant::now() + d);
                } else {
                    // Channel closed — NM gone, fallback to polling
                    tracing::warn!("D-Bus watch channel closed — falling back to polling");
                    events = None;
                    next_retry = Some(tokio::time::Instant::now() + cfg.verify_interval);
                }
            }
            _ = sleep_fut => {
                tracing::debug!("timer fired — re-checking");
                let (snap, retry) = controller.step().await;
                log_snapshot(&snap);
                next_retry = retry.map(|d| tokio::time::Instant::now() + d);
            }
        }
    }

    Ok(())
}

fn spawn_settings_watcher(
    path: std::path::PathBuf,
    tx: tokio::sync::mpsc::Sender<()>,
) -> Option<notify::RecommendedWatcher> {
    use notify::{EventKind, RecursiveMode, Watcher};
    let parent = path.parent()?.to_path_buf();
    let watch_path = path.clone();
    let mut watcher = notify::recommended_watcher(move |res: Result<notify::Event, _>| {
        if let Ok(ev) = res {
            // Only wake on modify/create for our settings file
            let is_our_file = ev.paths.iter().any(|p| p == &watch_path);
            if is_our_file
                && matches!(
                    ev.kind,
                    EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
                )
            {
                let _ = tx.blocking_send(());
            }
        }
    })
    .ok()?;
    // Watch parent dir (file may not exist yet)
    let _ = watcher.watch(&parent, RecursiveMode::NonRecursive);
    // Ensure file exists so we get events
    let _ = std::fs::create_dir_all(&parent);
    Some(watcher)
}

fn log_snapshot(s: &session::Snapshot) {
    tracing::info!(
        state = %s.state,
        msg = %s.message,
        ssid = ?s.current_ssid,
        on_target = s.on_target,
        online = s.online,
        "tick"
    );
    if let Some(e) = &s.last_error {
        tracing::warn!(error = %e, "last error");
    }
    if let Some(l) = &s.last_login {
        tracing::info!(outcome = %l.outcome, http = l.http_status, "last login");
    }
}
