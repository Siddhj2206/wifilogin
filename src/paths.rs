use anyhow::{Context, Result};
use std::path::PathBuf;

/// User configuration root according to the XDG Base Directory specification.
pub fn config_dir() -> Result<PathBuf> {
    xdg_dir("XDG_CONFIG_HOME", ".config")
}

fn xdg_dir(variable: &str, fallback: &str) -> Result<PathBuf> {
    if let Ok(path) = std::env::var(variable)
        && !path.is_empty()
    {
        return Ok(PathBuf::from(path));
    }
    let home = std::env::var_os("HOME").context("could not resolve home directory")?;
    Ok(PathBuf::from(home).join(fallback))
}
