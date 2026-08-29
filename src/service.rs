//! Manage the systemd user service.
//!
//! The unit file is embedded in the binary at compile time (so `cargo install
//! --git` needs nothing from the repo on disk) and rendered with the path of
//! the *currently running* executable, so the service always points at
//! whatever binary ran `service install`.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::process::Command;

const UNIT_NAME: &str = "wifilogin.service";
const UNIT_TEMPLATE: &str = include_str!("../systemd/wifilogin.service.in");

/// Where the rendered unit lives for the current user.
pub fn unit_path() -> Result<PathBuf> {
    let dir = dirs::config_dir().context("could not resolve config dir")?;
    Ok(dir.join("systemd/user").join(UNIT_NAME))
}

/// Render the unit for a given executable path. `extra_env` holds
/// `KEY=VALUE` strings that get baked in as `Environment=` lines (e.g. a
/// `WIFILOGIN_CONFIG` override set at install time).
pub fn render_unit(exe: &str, extra_env: &[String]) -> String {
    let env_lines: String = extra_env
        .iter()
        .map(|kv| format!("Environment={kv}\n"))
        .collect();
    UNIT_TEMPLATE
        .replace("@EXE@", exe)
        .replace("@EXTRA_ENV@", &env_lines)
}

fn require_systemctl() -> Result<()> {
    if Command::new("systemctl").arg("--version").output().is_err() {
        bail!("systemctl not found — `service` commands require a systemd user session");
    }
    Ok(())
}

fn require_installed() -> Result<PathBuf> {
    let path = unit_path()?;
    if !path.exists() {
        bail!("service not installed — run `wifilogin service install` first");
    }
    Ok(path)
}

fn systemctl_raw(args: &[&str]) -> Result<std::process::Output> {
    Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .with_context(|| format!("run systemctl --user {}", args.join(" ")))
}

fn systemctl(args: &[&str]) -> Result<()> {
    let out = systemctl_raw(args)?;
    if !out.status.success() {
        bail!(
            "`systemctl --user {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Write the unit, reload systemd, and enable + start the service.
/// Idempotent: safe to re-run after upgrades or binary moves.
pub fn install() -> Result<()> {
    require_systemctl()?;

    let exe = std::env::current_exe().context("resolve current executable path")?;
    let exe = exe.display().to_string();

    // Persist env overrides the user runs install with — the systemd
    // environment won't have them otherwise.
    let mut extra_env = Vec::new();
    if let Ok(v) = std::env::var("WIFILOGIN_CONFIG") {
        extra_env.push(format!("WIFILOGIN_CONFIG={v}"));
    }

    let path = unit_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("create {}", dir.display()))?;
    }

    let contents = render_unit(&exe, &extra_env);
    let existed = path.exists();
    let changed = std::fs::read_to_string(&path).ok().as_deref() != Some(contents.as_str());
    if changed {
        std::fs::write(&path, &contents).with_context(|| format!("write {}", path.display()))?;
        println!(
            "{} {}",
            if existed { "updated" } else { "created" },
            path.display()
        );
    } else {
        println!("unit file already up to date: {}", path.display());
    }

    systemctl(&["daemon-reload"])?;
    println!("reloaded user units");
    systemctl(&["enable", "--now", UNIT_NAME])?;
    println!("enabled and started {UNIT_NAME}");

    println!();
    println!("logs:  journalctl --user -u wifilogin -f");
    println!("check: wifilogin status");
    Ok(())
}

/// Disable, stop, and remove the unit. Idempotent.
pub fn uninstall() -> Result<()> {
    require_systemctl()?;
    let path = unit_path()?;
    if !path.exists() {
        println!("service not installed — nothing to do");
        return Ok(());
    }
    let out = systemctl_raw(&["disable", "--now", UNIT_NAME])?;
    if !out.status.success() {
        eprintln!(
            "note: could not disable service: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
    println!("removed {}", path.display());
    systemctl(&["daemon-reload"])?;
    println!("wifilogin service uninstalled (credentials and config kept)");
    Ok(())
}

pub fn start() -> Result<()> {
    require_systemctl()?;
    require_installed()?;
    systemctl(&["start", UNIT_NAME])?;
    println!("started {UNIT_NAME}");
    Ok(())
}

pub fn stop() -> Result<()> {
    require_systemctl()?;
    require_installed()?;
    systemctl(&["stop", UNIT_NAME])?;
    println!("stopped {UNIT_NAME}");
    Ok(())
}

pub fn restart() -> Result<()> {
    require_systemctl()?;
    require_installed()?;
    systemctl(&["restart", UNIT_NAME])?;
    println!("restarted {UNIT_NAME}");
    Ok(())
}

/// Passthrough to `systemctl --user status` — exit code propagates
/// (0 active, 3 inactive, non-zero other failures).
pub fn status() -> Result<()> {
    require_systemctl()?;
    require_installed()?;
    let code = Command::new("systemctl")
        .arg("--user")
        .arg("status")
        .arg(UNIT_NAME)
        .status()?
        .code()
        .unwrap_or(1);
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_exe_and_env() {
        let u = render_unit(
            "/home/sid/.cargo/bin/wifilogin",
            &["WIFILOGIN_CONFIG=/tmp/cfg.toml".to_string()],
        );
        assert!(u.contains("ExecStart=/home/sid/.cargo/bin/wifilogin run"));
        assert!(u.contains("Environment=WIFILOGIN_CONFIG=/tmp/cfg.toml"));
        assert!(!u.contains("@EXE@"));
        assert!(!u.contains("@EXTRA_ENV@"));
    }

    #[test]
    fn renders_without_extra_env() {
        let u = render_unit("/usr/local/bin/wifilogin", &[]);
        assert!(u.contains("ExecStart=/usr/local/bin/wifilogin run"));
        // The placeholder line is gone entirely, leaving no stray blank issues
        assert!(!u.contains("@"));
    }
}
