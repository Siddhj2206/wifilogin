use crate::paths;
use crate::wifi::{Connectivity, NetworkState};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::OnceLock;

const DEFAULT_CONFIG_FILE: &str = "config.toml";

/// `--config` CLI override, set once at startup before any tasks matter.
static CONFIG_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();

pub fn set_config_override(path: PathBuf) -> bool {
    CONFIG_OVERRIDE.set(path).is_ok()
}

/// One trusted NetworkManager connection on which credentials may be submitted.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    /// Displayed Wi-Fi name. It prevents surprising behavior in status output.
    pub ssid: String,
    /// NetworkManager connection UUID. Unlike an SSID, this is tied to the
    /// local profile the user explicitly created and selected.
    pub connection_uuid: String,
}

impl Target {
    pub fn matches(&self, ssid: &str, connection_uuid: &str) -> bool {
        self.ssid == ssid && self.connection_uuid.eq_ignore_ascii_case(connection_uuid)
    }
}

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

/// The networks on which wifilogin is allowed to submit portal credentials.
///
/// This is an allow-list, not a list of networks to join. The daemon never
/// activates a NetworkManager connection; joining a network is always left to
/// NetworkManager and the user.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Explicit local NetworkManager connections on which login is permitted.
    #[serde(default)]
    pub targets: Vec<Target>,
    /// Captive portal login endpoint.
    pub portal_url: String,
    /// URL that must answer HTTP 204 after a login. It is used only to verify
    /// an attempted login; NetworkManager's D-Bus Connectivity property drives
    /// normal daemon decisions.
    #[serde(default = "default_connectivity_url")]
    pub connectivity_url: String,
}

fn default_connectivity_url() -> String {
    "http://clients3.google.com/generate_204".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // An empty allow-list is safe: a new installation cannot join or
            // authenticate to any network until its owner opts in.
            targets: Vec::new(),
            portal_url: "http://phc.prontonetworks.com/cgi-bin/authlogin?URI=".to_string(),
            connectivity_url: default_connectivity_url(),
        }
    }
}

/// Commented template written by `wifilogin config init`.
pub const TEMPLATE: &str = r#"# wifilogin configuration

# Local NetworkManager connections where wifilogin may submit credentials.
# This is deliberately empty by default. The daemon NEVER joins WiFi or
# changes NetworkManager's autoconnect behavior; it only acts after you or
# NetworkManager have connected to a listed local profile.
#
# Replace `targets = []` with one block per intended connection. Obtain its
# UUID with: nmcli -g UUID connection show "Campus WiFi"
#
# [[targets]]
# ssid = "Campus WiFi"
# connection_uuid = "00000000-0000-0000-0000-000000000000"
targets = []

# Captive portal login endpoint (Pronto Networks default shown).
portal_url = "http://phc.prontonetworks.com/cgi-bin/authlogin?URI="

# Used once to verify an attempted login. Normal decisions come from
# NetworkManager's D-Bus Connectivity state, not periodic HTTP polling.
connectivity_url = "http://clients3.google.com/generate_204"
"#;

impl Config {
    pub fn validate(&self) -> Result<()> {
        for (index, target) in self.targets.iter().enumerate() {
            if target.ssid.trim().is_empty() {
                anyhow::bail!("targets[{index}].ssid must not be empty");
            }
            if !is_uuid(&target.connection_uuid) {
                anyhow::bail!(
                    "targets[{index}] ({}) has an invalid connection_uuid; expected a UUID",
                    target.ssid
                );
            }
        }
        if self.portal_url.trim().is_empty() {
            anyhow::bail!("portal_url is required");
        }
        if self.connectivity_url.trim().is_empty() {
            anyhow::bail!("connectivity_url is required");
        }
        validate_http_url("portal_url", &self.portal_url)?;
        validate_http_url("connectivity_url", &self.connectivity_url)?;
        Ok(())
    }

    pub fn is_target(&self, ssid: &str, connection_uuid: &str) -> bool {
        self.targets
            .iter()
            .any(|target| target.matches(ssid, connection_uuid))
    }

    pub fn portal_permission<'a>(&self, network: &'a NetworkState) -> PortalPermission<'a> {
        match network {
            NetworkState::Disconnected => PortalPermission::Disconnected,
            NetworkState::Connected {
                ssid,
                connection_uuid,
                ..
            } if !self.is_target(ssid, connection_uuid) => PortalPermission::OtherNetwork(ssid),
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

fn validate_http_url(name: &str, value: &str) -> Result<()> {
    let url = reqwest::Url::parse(value).with_context(|| format!("invalid {name} URL"))?;
    if !matches!(url.scheme(), "http" | "https") {
        anyhow::bail!("{name} must use http or https");
    }
    if url.host_str().is_none() {
        anyhow::bail!("{name} must include a host");
    }
    Ok(())
}

fn is_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

pub fn config_path() -> Result<PathBuf> {
    if let Some(path) = CONFIG_OVERRIDE.get() {
        return Ok(path.clone());
    }
    if let Ok(path) = std::env::var("WIFILOGIN_CONFIG") {
        return Ok(PathBuf::from(path));
    }
    Ok(user_config_dir()?
        .join("wifilogin")
        .join(DEFAULT_CONFIG_FILE))
}

/// XDG configuration root, shared with the systemd user-unit installer.
pub fn user_config_dir() -> Result<PathBuf> {
    paths::config_dir()
}

/// Load an existing config. Reading configuration never creates files.
pub fn load() -> Result<Config> {
    let path = config_path()?;
    if !path.exists() {
        anyhow::bail!(
            "no config at {} — run `wifilogin config init` to create one",
            path.display()
        );
    }
    let raw = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let config: Config =
        toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    config.validate()?;
    Ok(config)
}

/// Write the commented template. Refuses to clobber an existing file.
pub fn init() -> Result<PathBuf> {
    let path = config_path()?;
    if path.exists() {
        anyhow::bail!(
            "config already exists at {} — edit it instead, or remove it first",
            path.display()
        );
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    std::fs::write(&path, TEMPLATE).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_safe_and_valid() {
        let config = Config::default();
        config.validate().unwrap();
        assert!(config.targets.is_empty());
    }

    #[test]
    fn target_matching_is_exact() {
        let config = Config {
            targets: vec![Target {
                ssid: "Campus".into(),
                connection_uuid: "d9428888-122b-11e1-b85c-61cd3cbb3210".into(),
            }],
            ..Default::default()
        };
        assert!(config.is_target("Campus", "d9428888-122b-11e1-b85c-61cd3cbb3210"));
        assert!(config.is_target("Campus", "D9428888-122B-11E1-B85C-61CD3CBB3210"));
        assert!(!config.is_target("Campus-Guest", "d9428888-122b-11e1-b85c-61cd3cbb3210"));
        assert!(!config.is_target("Campus", "2f1c5a60-6e1f-4c42-b2a8-0d1a9d2f3e40"));
    }

    #[test]
    fn template_parses_to_an_empty_allow_list() {
        let config: Config = toml::from_str(TEMPLATE).unwrap();
        config.validate().unwrap();
        assert!(config.targets.is_empty());
    }

    #[test]
    fn obsolete_configuration_is_rejected() {
        let error = toml::from_str::<Config>(
            r#"
                targets = []
                portal_url = "https://portal.example"
                verify_interval = "60s"
            "#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn non_http_urls_are_rejected() {
        let config = Config {
            portal_url: "file:///tmp/portal".into(),
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn invalid_target_uuid_is_rejected() {
        let config = Config {
            targets: vec![Target {
                ssid: "Campus".into(),
                connection_uuid: "not-a-uuid".into(),
            }],
            ..Default::default()
        };
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("targets[0]")
        );
    }
}
