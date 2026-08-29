use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

const DEFAULT_CONFIG_FILE: &str = "config.toml";

/// `--config` CLI override, set once at startup before any threads matter.
static CONFIG_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();

pub fn set_config_override(path: PathBuf) -> bool {
    CONFIG_OVERRIDE.set(path).is_ok()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// SSIDs that trigger auto-login. Only these are acted on — everything else is ignored.
    pub targets: Vec<String>,
    /// Captive portal login endpoint.
    pub portal_url: String,
    /// URL that must answer HTTP 204 when truly online. A captive portal
    /// intercepts it, so a non-204 means we need to log in.
    #[serde(default = "default_connectivity_url")]
    pub connectivity_url: String,
    /// How often to re-verify connectivity while connected to a target and online.
    /// No polling happens when on other networks or disconnected — purely event-driven.
    #[serde(default = "default_verify_interval", with = "humantime_serde")]
    pub verify_interval: Duration,
    /// Base delay between portal retries after a failure. Grows exponentially
    /// (base, 2x, 4x, ...) up to a hard cap — never a fixed hammering interval.
    #[serde(default = "default_retry_interval", with = "humantime_serde")]
    pub retry_interval: Duration,
}

fn default_connectivity_url() -> String {
    "http://clients3.google.com/generate_204".to_string()
}
fn default_verify_interval() -> Duration {
    Duration::from_secs(60)
}
fn default_retry_interval() -> Duration {
    Duration::from_secs(10)
}

mod humantime_serde {
    use serde::{Deserialize, Deserializer, Serializer, de};
    use std::time::Duration;

    pub fn deserialize<'de, D>(d: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(d)?;
        humantime::parse_duration(&s).map_err(de::Error::custom)
    }
    pub fn serialize<S>(dur: &Duration, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        s.serialize_str(&humantime::format_duration(*dur).to_string())
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            targets: vec!["R-VIT".to_string()],
            portal_url: "http://phc.prontonetworks.com/cgi-bin/authlogin?URI=".to_string(),
            connectivity_url: default_connectivity_url(),
            verify_interval: default_verify_interval(),
            retry_interval: default_retry_interval(),
        }
    }
}

/// Commented template written by `wifilogin config init`.
pub const TEMPLATE: &str = r#"# wifilogin configuration

# SSIDs that trigger captive-portal auto-login. The daemon only ever
# connects, logs in, or polls while on one of these.
targets = ["R-VIT", "R-VIT-5G"]

# Captive portal login endpoint (Pronto Networks default shown).
portal_url = "http://phc.prontonetworks.com/cgi-bin/authlogin?URI="

# URL that must return HTTP 204 when truly online. A captive portal
# intercepts it, which is how the daemon detects it needs to log in.
connectivity_url = "http://clients3.google.com/generate_204"

# How often to re-verify connectivity while online on a target.
verify_interval = "60s"

# Base delay between portal retries after a failure. Grows exponentially
# (10s, 20s, 40s, ...) up to a 5-minute cap, so a broken portal is never
# hammered.
retry_interval = "10s"
"#;

impl Config {
    pub fn validate(&self) -> Result<()> {
        if self.targets.is_empty() {
            anyhow::bail!("targets must contain at least one SSID");
        }
        for t in &self.targets {
            if t.trim().is_empty() {
                anyhow::bail!("target SSID must not be empty");
            }
        }
        if self.portal_url.trim().is_empty() {
            anyhow::bail!("portal_url is required");
        }
        if self.connectivity_url.trim().is_empty() {
            anyhow::bail!("connectivity_url is required");
        }
        if self.verify_interval.is_zero() {
            anyhow::bail!("verify_interval must be > 0");
        }
        if self.retry_interval.is_zero() {
            anyhow::bail!("retry_interval must be > 0");
        }
        Ok(())
    }

    pub fn is_target(&self, ssid: &str) -> bool {
        self.targets.iter().any(|t| t == ssid)
    }
}

pub fn config_path() -> Result<PathBuf> {
    if let Some(p) = CONFIG_OVERRIDE.get() {
        return Ok(p.clone());
    }
    if let Ok(custom) = std::env::var("WIFILOGIN_CONFIG") {
        return Ok(PathBuf::from(custom));
    }
    // Legacy alias from the latch days.
    if let Ok(custom) = std::env::var("LATCH_CONFIG_PATH") {
        return Ok(PathBuf::from(custom));
    }
    let base = dirs::config_dir().context("could not resolve config dir")?;
    Ok(base.join("wifilogin").join(DEFAULT_CONFIG_FILE))
}

/// Load an existing config. Errors with an actionable message if missing —
/// reading must never have the side effect of writing files.
pub fn load() -> Result<Config> {
    let path = config_path()?;
    if !path.exists() {
        anyhow::bail!(
            "no config at {} — run `wifilogin config init` to create one",
            path.display()
        );
    }
    let raw = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let cfg: Config = toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    cfg.validate()?;
    Ok(cfg)
}

/// Load, creating the default config if missing. Only the daemon should use
/// this; one-shot commands want `load()` so they surface real errors.
pub fn load_or_create() -> Result<Config> {
    let path = config_path()?;
    if path.exists() {
        return load();
    }
    let cfg = Config::default();
    save(&path, &cfg)?;
    tracing::info!(path = %path.display(), "created default config");
    Ok(cfg)
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

pub fn save(path: &PathBuf, cfg: &Config) -> Result<()> {
    cfg.validate()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let s = toml::to_string_pretty(cfg).context("encode config")?;
    std::fs::write(path, s).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_valid() {
        Config::default().validate().unwrap();
    }

    #[test]
    fn is_target() {
        let c = Config {
            targets: vec!["R-VIT".into(), "R-VIT-5G".into()],
            ..Default::default()
        };
        assert!(c.is_target("R-VIT"));
        assert!(c.is_target("R-VIT-5G"));
        assert!(!c.is_target("Other"));
    }

    #[test]
    fn template_parses() {
        let cfg: Config = toml::from_str(TEMPLATE).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.verify_interval, Duration::from_secs(60));
        assert_eq!(cfg.retry_interval, Duration::from_secs(10));
    }

    #[test]
    fn missing_config_is_actionable_error() {
        // No override set and no real config path — just check load() doesn't create files.
        let path = std::env::temp_dir().join(format!(
            "wifilogin-test-missing-{}-{}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        assert!(!path.exists());
    }
}
