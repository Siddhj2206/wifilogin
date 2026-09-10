use anyhow::Result;
use std::time::Duration;

const LOGIN_TIMEOUT: Duration = Duration::from_secs(15);
const CHECK_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_RESPONSE_BODY: usize = 1024 * 1024;
const USER_AGENT: &str = concat!("wifilogin/", env!("CARGO_PKG_VERSION"));

/// VIT's Pronto Networks login endpoint.
const PORTAL_URL: &str = "http://phc.prontonetworks.com/cgi-bin/authlogin?URI=";
/// Requested once after login to verify that the portal released access.
const CONNECTIVITY_URL: &str = "http://clients3.google.com/generate_204";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Granted,
    BadCredentials,
    Uncertain,
    HttpError,
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Granted => write!(f, "granted"),
            Self::BadCredentials => write!(f, "bad credentials"),
            Self::Uncertain => write!(f, "uncertain"),
            Self::HttpError => write!(f, "http error"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct LoginResult {
    pub outcome: Outcome,
    pub http_status: u16,
}

/// Portal + connectivity checks. A seam so the session controller can be
/// tested without network access.
pub trait Portal: Send + Sync {
    async fn online(&self) -> Result<bool>;
    async fn login(&self, username: &str, password: &str) -> Result<LoginResult>;
}

pub struct PortalClient;

impl Portal for PortalClient {
    /// True if internet is reachable (HTTP 204). Captive portals intercept the
    /// request, so a non-204 (or a redirect) means we need to log in.
    async fn online(&self) -> Result<bool> {
        let client = reqwest::Client::builder()
            .timeout(CHECK_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()?;

        let resp = client
            .get(CONNECTIVITY_URL)
            .header("User-Agent", USER_AGENT)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("connectivity check: {e}"))?;

        Ok(resp.status().as_u16() == 204)
    }

    /// POST to the Pronto Networks portal.
    async fn login(&self, username: &str, password: &str) -> Result<LoginResult> {
        let client = reqwest::Client::builder().timeout(LOGIN_TIMEOUT).build()?;

        let params = [
            ("userId", username),
            ("password", password),
            ("serviceName", "ProntoAuthentication"),
            ("Submit22", "Login"),
        ];

        let mut resp = client
            .post(PORTAL_URL)
            .form(&params)
            .header("User-Agent", USER_AGENT)
            .header("Referer", PORTAL_URL)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("post login form: {e}"))?;

        let status = resp.status().as_u16();
        let mut body = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|error| anyhow::anyhow!("read portal response: {error}"))?
        {
            if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BODY {
                anyhow::bail!("portal response exceeds {MAX_RESPONSE_BODY} bytes");
            }
            body.extend_from_slice(&chunk);
        }
        let body = String::from_utf8_lossy(&body);

        let mut outcome = classify(&body);
        if status >= 400 && outcome == Outcome::Uncertain {
            outcome = Outcome::HttpError;
        }

        Ok(LoginResult {
            outcome,
            http_status: status,
        })
    }
}

pub(crate) fn classify(body: &str) -> Outcome {
    if body.contains("WiFi Access Granted") || body.contains("Access Granted") {
        return Outcome::Granted;
    }
    if body.contains("WiFi Login Portal")
        || body.contains("Sorry, please check your username and password")
    {
        return Outcome::BadCredentials;
    }
    Outcome::Uncertain
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_granted() {
        assert_eq!(
            classify("<title>WiFi Access Granted</title>"),
            Outcome::Granted
        );
    }

    #[test]
    fn classify_bad() {
        assert_eq!(
            classify("Sorry, please check your username and password"),
            Outcome::BadCredentials
        );
    }

    #[test]
    fn classify_uncertain() {
        assert_eq!(classify("something else"), Outcome::Uncertain);
    }
}
