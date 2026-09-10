use crate::paths;
use crate::wifi::{Connectivity, NetworkState};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const CONFIG_FILE: &str = "config.toml";

/// Why the active Wi-Fi connection may or may not receive portal credentials.
/// Both automatic and explicit login use this same classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortalPermission<'a> {
    Disconnected,
    OtherNetwork(&'a str),
    Connecting(&'a str),
    TargetNotDefault(&'a str),
    Allowed {
        ssid: &'a str,
        connection_uuid: &'a str,
        connectivity: Connectivity,
    },
}

/// Non-secret choices made through the CLI. Passwords live only in the system
/// keyring, and the VIT portal URLs live in `portal`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Wi-Fi names on which portal login is permitted.
    #[serde(default)]
    pub targets: Vec<String>,
    /// Portal username. Passwords are stored separately in the system keyring.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        for (index, target) in self.targets.iter().enumerate() {
            if target.trim().is_empty() {
                anyhow::bail!("targets[{index}] must not be empty");
            }
        }
        if self
            .username
            .as_deref()
            .is_some_and(|username| username.trim().is_empty())
        {
            anyhow::bail!("username must not be empty");
        }
        Ok(())
    }

    pub fn is_target(&self, ssid: &str) -> bool {
        self.targets.iter().any(|target| target == ssid)
    }

    pub fn portal_permission<'a>(&self, network: &'a NetworkState) -> PortalPermission<'a> {
        match network {
            NetworkState::Disconnected => PortalPermission::Disconnected,
            NetworkState::Connected { ssid, .. } if !self.is_target(ssid) => {
                PortalPermission::OtherNetwork(ssid)
            }
            NetworkState::Connected {
                ssid,
                is_activated: false,
                ..
            } => PortalPermission::Connecting(ssid),
            NetworkState::Connected {
                ssid,
                is_default: false,
                ..
            } => PortalPermission::TargetNotDefault(ssid),
            NetworkState::Connected {
                ssid,
                connection_uuid,
                connectivity,
                ..
            } => PortalPermission::Allowed {
                ssid,
                connection_uuid,
                connectivity: *connectivity,
            },
        }
    }
}

pub fn config_path() -> Result<PathBuf> {
    Ok(paths::config_dir()?.join("wifilogin").join(CONFIG_FILE))
}

pub fn load() -> Result<Config> {
    let path = config_path()?;
    if !path.exists() {
        anyhow::bail!("not configured — run `wifilogin setup` before starting the daemon");
    }
    let raw = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let config: Config =
        toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    config.validate()?;
    Ok(config)
}

/// Create or replace the non-secret configuration. This is called only from
/// CLI commands; ordinary users do not need to edit TOML.
pub fn save(config: &Config) -> Result<()> {
    config.validate()?;
    let path = config_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let raw = toml::to_string_pretty(config).context("encode configuration")?;
    std::fs::write(&path, raw).with_context(|| format!("write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_matching_is_exact() {
        let config = Config {
            targets: vec!["R-VIT".into()],
            username: Some("user".into()),
        };
        assert!(config.is_target("R-VIT"));
        assert!(!config.is_target("R-VIT-Guest"));
    }

    #[test]
    fn empty_target_is_rejected() {
        let config = Config {
            targets: vec!["".into()],
            username: None,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn unknown_configuration_is_rejected() {
        let error = toml::from_str::<Config>(
            r#"
                targets = ["R-VIT"]
                username = "user"
                portal_url = "https://portal.example"
            "#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("unknown field"));
    }
}
