use anyhow::Result;
use std::time::Duration;

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
            Self::BadCredentials => write!(f, "bad_credentials"),
            Self::Uncertain => write!(f, "uncertain"),
            Self::HttpError => write!(f, "http_error"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct LoginResult {
    pub outcome: Outcome,
    pub http_status: u16,
    pub body_snippet: String,
}

const DEFAULT_CONNECTIVITY_URL: &str = "http://clients3.google.com/generate_204";
const MAX_BODY: usize = 1024 * 1024;
const SNIPPET: usize = 1024;

/// POST to Pronto portal. Mirrors latch-linux portal.Login.
pub async fn login(portal_url: &str, username: &str, password: &str) -> Result<LoginResult> {
    if portal_url.is_empty() {
        anyhow::bail!("portal_url is required");
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
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
        .header("User-Agent", "wifilogin/0.1")
        .header("Referer", portal_url)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("post login form: {e}"))?;

    let status = resp.status().as_u16();
    let body = resp
        .bytes()
        .await
        .map_err(|e| anyhow::anyhow!("read body: {e}"))?;

    let body_limited = if body.len() > MAX_BODY {
        &body[..MAX_BODY]
    } else {
        &body
    };
    let body_str = String::from_utf8_lossy(body_limited).to_string();
    let snippet = {
        let s = body_str.trim();
        if s.len() > SNIPPET {
            s[..SNIPPET].to_string()
        } else {
            s.to_string()
        }
    };

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

/// Returns true if internet is reachable (204). Captive portals intercept -> not 204.
pub async fn online() -> Result<bool> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;

    let resp = client
        .get(DEFAULT_CONNECTIVITY_URL)
        .header("User-Agent", "wifilogin/0.1")
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("connectivity check: {e}"))?;

    Ok(resp.status().as_u16() == 204)
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
