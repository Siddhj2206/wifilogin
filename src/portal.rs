use anyhow::Result;
use std::time::Duration;

const LOGIN_TIMEOUT: Duration = Duration::from_secs(15);
const CHECK_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_BODY: usize = 1024 * 1024;
const SNIPPET: usize = 1024;
const USER_AGENT: &str = concat!("wifilogin/", env!("CARGO_PKG_VERSION"));

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
    pub body_snippet: String,
}

/// Portal + connectivity checks. A seam so the session controller can be
/// tested without network access.
pub trait Portal: Send + Sync {
    async fn online(&self, connectivity_url: &str) -> Result<bool>;
    async fn login(&self, portal_url: &str, username: &str, password: &str)
    -> Result<LoginResult>;
}

pub struct PortalClient;

impl Portal for PortalClient {
    /// True if internet is reachable (HTTP 204). Captive portals intercept the
    /// request, so a non-204 (or a redirect) means we need to log in.
    async fn online(&self, connectivity_url: &str) -> Result<bool> {
        let client = reqwest::Client::builder()
            .timeout(CHECK_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()?;

        let resp = client
            .get(connectivity_url)
            .header("User-Agent", USER_AGENT)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("connectivity check: {e}"))?;

        Ok(resp.status().as_u16() == 204)
    }

    /// POST to the Pronto Networks portal.
    async fn login(
        &self,
        portal_url: &str,
        username: &str,
        password: &str,
    ) -> Result<LoginResult> {
        if portal_url.is_empty() {
            anyhow::bail!("portal_url is required");
        }
        let client = reqwest::Client::builder()
            .timeout(LOGIN_TIMEOUT)
            .cookie_store(true)
            .build()?;

        let params = [
            ("userId", username),
            ("password", password),
            ("serviceName", "ProntoAuthentication"),
            ("Submit22", "Login"),
        ];

        let resp = client
            .post(portal_url)
            .form(&params)
            .header("User-Agent", USER_AGENT)
            .header("Referer", portal_url)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("post login form: {e}"))?;

        let status = resp.status().as_u16();
        let body = resp
            .bytes()
            .await
            .map_err(|e| anyhow::anyhow!("read body: {e}"))?;

        let body_limited = &body[..body.len().min(MAX_BODY)];
        let body_str = String::from_utf8_lossy(body_limited);
        // Char-boundary-safe truncation: the portal is not the only one who
        // can produce multi-byte UTF-8, and a naive byte slice panics.
        let snippet = truncate(body_str.trim(), SNIPPET).to_string();

        let mut outcome = classify(&body_str);
        if status >= 400 && outcome == Outcome::Uncertain {
            outcome = Outcome::HttpError;
        }

        Ok(LoginResult {
            outcome,
            http_status: status,
            body_snippet: snippet,
        })
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
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

    #[test]
    fn truncate_is_char_boundary_safe() {
        // "é" is 2 bytes; slicing at 1 would panic without the boundary walk.
        let s = "ééééé".repeat(300); // 3000 bytes, 1500 chars
        let t = truncate(&s, 1024);
        assert!(t.len() <= 1024);
        assert_eq!(t.chars().count(), 512);
        assert!(s.ends_with(t) || t.is_empty());
    }
}
