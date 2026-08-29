use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

const SETTINGS_FILE: &str = "settings.json";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct Settings {
    /// When false, daemon stays idle and never auto-connects or logs in.
    /// Toggle via `wifilogin pause` / `wifilogin resume`.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone)]
pub struct Manager {
    path: PathBuf,
    inner: Arc<RwLock<Settings>>,
}

impl Manager {
    pub fn new() -> Result<Self> {
        let path = settings_path()?;
        let settings = load_or_create(&path)?;
        Ok(Self {
            path,
            inner: Arc::new(RwLock::new(settings)),
        })
    }

    pub fn get(&self) -> Settings {
        *self.inner.read().unwrap()
    }

    pub fn set_enabled(&self, enabled: bool) -> Result<()> {
        {
            let mut w = self.inner.write().unwrap();
            w.enabled = enabled;
        }
        let s = self.get();
        save(&self.path, &s)?;
        Ok(())
    }

    pub fn reload(&self) -> Result<Settings> {
        let s = load_or_create(&self.path)?;
        *self.inner.write().unwrap() = s;
        Ok(s)
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }
}

pub fn settings_path() -> Result<PathBuf> {
    if let Ok(custom) = std::env::var("WIFILOGIN_STATE_PATH") {
        return Ok(PathBuf::from(custom));
    }
    if let Ok(custom) = std::env::var("LATCH_STATE_PATH") {
        return Ok(PathBuf::from(custom));
    }
    let base = dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .context("could not resolve state dir")?;
    Ok(base.join("wifilogin").join(SETTINGS_FILE))
}

fn load_or_create(path: &PathBuf) -> Result<Settings> {
    if !path.exists() {
        let s = Settings::default();
        save(path, &s)?;
        return Ok(s);
    }
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let s: Settings =
        serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    Ok(s)
}

fn save(path: &PathBuf, s: &Settings) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let data = serde_json::to_string_pretty(s).context("encode settings")?;
    std::fs::write(path, data + "\n").with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_enabled() {
        assert!(Settings::default().enabled);
    }
}
