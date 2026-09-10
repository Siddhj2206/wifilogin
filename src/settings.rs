use crate::paths;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

const SETTINGS_FILE: &str = "settings.json";
const DAEMON_STATE_FILE: &str = "daemon.json";
const CONTROL_SOCKET_FILE: &str = "control.sock";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct Settings {
    /// When false, the daemon observes NetworkManager but never logs in.
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
        Self::from_path(settings_path()?)
    }

    /// Explicit path, used by tests as well as production.
    pub fn from_path(path: PathBuf) -> Result<Self> {
        let settings = load_or_create(&path)?;
        Ok(Self {
            path,
            inner: Arc::new(RwLock::new(settings)),
        })
    }

    pub fn get(&self) -> Settings {
        *self.inner.read().expect("settings lock poisoned")
    }

    pub fn set_enabled(&self, enabled: bool) -> Result<()> {
        {
            let mut settings = self.inner.write().expect("settings lock poisoned");
            settings.enabled = enabled;
        }
        save(&self.path, self.get())
    }

    pub fn reload(&self) -> Result<Settings> {
        let settings = load_or_create(&self.path)?;
        *self.inner.write().expect("settings lock poisoned") = settings;
        Ok(settings)
    }
}

pub fn settings_path() -> Result<PathBuf> {
    Ok(state_dir()?.join(SETTINGS_FILE))
}

/// File the daemon writes after every state transition for `wifilogin status`.
pub fn daemon_state_path() -> Result<PathBuf> {
    Ok(state_dir()?.join(DAEMON_STATE_FILE))
}

/// A local control datagram wakes the daemon without a filesystem watcher.
pub fn control_socket_path() -> Result<PathBuf> {
    Ok(state_dir()?.join(CONTROL_SOCKET_FILE))
}

/// Ask a running daemon to reload its configuration and settings. A missing
/// socket simply means the daemon is not running, which is harmless for CLI
/// commands that update persistent state.
#[cfg(unix)]
pub fn request_reload() {
    use std::os::unix::net::UnixDatagram;

    if let Ok(socket) = UnixDatagram::unbound()
        && let Ok(path) = control_socket_path()
    {
        let _ = socket.send_to(b"reload", path);
    }
}

#[cfg(not(unix))]
pub fn request_reload() {}

fn state_dir() -> Result<PathBuf> {
    paths::state_dir().map(|path| path.join("wifilogin"))
}

fn load_or_create(path: &PathBuf) -> Result<Settings> {
    if !path.exists() {
        let settings = Settings::default();
        save(path, settings)?;
        return Ok(settings);
    }
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))
}

fn save(path: &PathBuf, settings: Settings) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let data = serde_json::to_string_pretty(&settings).context("encode settings")?;
    std::fs::write(path, data + "\n").with_context(|| format!("write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip() {
        let dir = std::env::temp_dir().join(format!(
            "wifilogin-settings-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("settings.json");
        let manager = Manager::from_path(path.clone()).unwrap();
        assert!(manager.get().enabled);
        manager.set_enabled(false).unwrap();
        assert!(!Manager::from_path(path).unwrap().get().enabled);
        let _ = std::fs::remove_dir_all(dir);
    }
}
