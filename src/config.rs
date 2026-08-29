use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

const DEFAULT_CONFIG_FILE: &str = "config.toml";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// SSIDs that trigger auto-login. Only these are acted on — everything else is ignored.
    pub targets: Vec<String>,
    pub portal_url: String,
    /// How often to re-verify connectivity while connected to a target and online.
    /// No polling happens when on other networks or disconnected — purely event-driven.
    #[serde(default = "default_verify_interval", with = "humantime_serde")]
    pub verify_interval: Duration,
    /// Interval for retrying after captive failures (used as base for backoff).
    #[serde(default = "default_retry_interval", with = "humantime_serde")]
    pub retry_interval: Duration,
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
            verify_interval: default_verify_interval(),
            retry_interval: default_retry_interval(),
        }
    }
}

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
    if let Ok(custom) = std::env::var("WIFILOGIN_CONFIG") {
        return Ok(PathBuf::from(custom));
    }
    if let Ok(custom) = std::env::var("LATCH_CONFIG_PATH") {
        return Ok(PathBuf::from(custom));
    }
    let base = dirs::config_dir().context("could not resolve config dir")?;
    Ok(base.join("wifilogin").join(DEFAULT_CONFIG_FILE))
}

pub fn load() -> Result<Config> {
    let path = config_path()?;
    if !path.exists() {
        let cfg = Config::default();
        save(&path, &cfg)?;
        tracing::info!(path = %path.display(), "created default config");
        return Ok(cfg);
    }
    let raw = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let cfg: Config = toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    cfg.validate()?;
    Ok(cfg)
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
}
