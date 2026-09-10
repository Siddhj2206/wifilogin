//! Manage the systemd user service.
//!
//! The unit file is embedded in the binary at compile time and rendered with
//! the path of the *currently running* executable, so `setup` always points at
//! the binary that performed setup.

use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use std::process::Command;

const UNIT_NAME: &str = "wifilogin.service";
const UNIT_TEMPLATE: &str = include_str!("../systemd/wifilogin.service.in");

/// Where the rendered unit lives for the current user.
pub fn unit_path() -> Result<PathBuf> {
    let dir = crate::paths::config_dir()?;
    Ok(dir.join("systemd/user").join(UNIT_NAME))
}

/// Render the unit for a given executable path.
pub fn render_unit(exe: &str) -> String {
    UNIT_TEMPLATE.replace("@EXE@", exe)
}

fn require_systemctl() -> Result<()> {
    if Command::new("systemctl").arg("--version").output().is_err() {
        bail!("systemctl not found — `service` commands require a systemd user session");
    }
    Ok(())
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

    let path = unit_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }

    let contents = render_unit(&exe);
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

/// Apply target changes immediately when wifilogin is service-managed.
pub fn restart_if_installed() -> Result<()> {
    let path = unit_path()?;
    if !path.exists() {
        println!("service not installed; targets will apply when `wifilogin setup` installs it");
        return Ok(());
    }
    require_systemctl()?;
    systemctl(&["restart", UNIT_NAME])?;
    println!("restarted {UNIT_NAME}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_exe() {
        let u = render_unit("/home/sid/.cargo/bin/wifilogin");
        assert!(u.contains("ExecStart=/home/sid/.cargo/bin/wifilogin run"));
        assert!(!u.contains("@EXE@"));
    }

    #[test]
    fn renders_without_unresolved_placeholders() {
        let u = render_unit("/usr/local/bin/wifilogin");
        assert!(u.contains("ExecStart=/usr/local/bin/wifilogin run"));
        // The placeholder line is gone entirely, leaving no stray blank issues
        assert!(!u.contains("@"));
    }
}
