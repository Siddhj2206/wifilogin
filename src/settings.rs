use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

const SETTINGS_FILE: &str = "settings.json";
const DAEMON_STATE_FILE: &str = "daemon.json";
const WAKE_FILE: &str = "wake";

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
        Self::from_path(path)
    }

    /// Explicit path — also what tests use.
    pub fn from_path(path: PathBuf) -> Result<Self> {
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
        save(&self.path, self.get())?;
        Ok(())
    }

    pub fn reload(&self) -> Result<Settings> {
        let s = load_or_create(&self.path)?;
        *self.inner.write().unwrap() = s;
        Ok(s)
    }
}

pub fn settings_path() -> Result<PathBuf> {
    if let Ok(custom) = std::env::var("WIFILOGIN_STATE_PATH") {
        return Ok(PathBuf::from(custom));
    }
    // Legacy alias from the latch days.
    if let Ok(custom) = std::env::var("LATCH_STATE_PATH") {
        return Ok(PathBuf::from(custom));
    }
    let base = dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .context("could not resolve state dir")?;
    Ok(base.join("wifilogin").join(SETTINGS_FILE))
}

/// File the daemon writes after every step so `wifilogin status` can show
/// whether it's alive and what it last did.
pub fn daemon_state_path() -> Result<PathBuf> {
    Ok(state_dir()?.join(DAEMON_STATE_FILE))
}

/// Touching this file wakes the daemon and makes it reload config, settings
/// and credentials. Replaces the old pkill/SIGHUP hack.
pub fn wake_path() -> Result<PathBuf> {
    Ok(state_dir()?.join(WAKE_FILE))
}

fn state_dir() -> Result<PathBuf> {
    Ok(settings_path()?
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_default())
}

fn load_or_create(path: &PathBuf) -> Result<Settings> {
    if !path.exists() {
        let s = Settings::default();
        save(path, s)?;
        return Ok(s);
    }
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let s: Settings =
        serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    Ok(s)
}

fn save(path: &PathBuf, s: Settings) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let data = serde_json::to_string_pretty(&s).context("encode settings")?;
    std::fs::write(path, data + "\n").with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Write an empty wake file so a running daemon picks up credential/config
/// changes immediately. Best-effort; harmless if no daemon is running.
pub fn wake_daemon() {
    if let Ok(path) = wake_path()
        && let Some(dir) = path.parent()
        && std::fs::create_dir_all(dir).is_ok()
    {
        let _ = std::fs::write(&path, "");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_enabled() {
        assert!(Settings::default().enabled);
    }

    #[test]
    fn from_path_roundtrip() {
        let dir = std::env::temp_dir().join(format!(
            "wifilogin-settings-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("settings.json");
        let mgr = Manager::from_path(path.clone()).unwrap();
        assert!(mgr.get().enabled);
        mgr.set_enabled(false).unwrap();
        // Re-read from disk
        let mgr2 = Manager::from_path(path).unwrap();
        assert!(!mgr2.get().enabled);
        let _ = std::fs::remove_dir_all(dir);
    }
}
