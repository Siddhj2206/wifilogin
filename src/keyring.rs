use anyhow::Result;

const SERVICE: &str = "wifilogin";
const PASS_KEY: &str = "password";

#[derive(Debug)]
pub struct NotFound;

impl std::fmt::Display for NotFound {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "credentials not found in keyring")
    }
}

impl std::error::Error for NotFound {}

/// Source of portal passwords. A seam so the session controller can be tested
/// without touching the real keyring.
pub trait Creds: Send + Sync {
    async fn load(&self) -> Result<String>;
}

pub struct KeyringCreds;

impl Creds for KeyringCreds {
    async fn load(&self) -> Result<String> {
        load().await
    }
}

/// Run a blocking keyring operation off the tokio runtime.
/// `secret-service` blocking API creates its own `zbus::blocking::Connection`
/// which panics with "Cannot start a runtime from within a runtime" if called
/// inside `#[tokio::main]`. We escape by running on a fresh OS thread with no
/// tokio Handle (spawn_blocking still has a Handle, so we double-spawn).
async fn on_keyring_thread<F, T>(f: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    // If we're not inside a tokio runtime, just run directly (e.g. unit tests)
    if tokio::runtime::Handle::try_current().is_err() {
        return f();
    }
    tokio::task::spawn_blocking(move || {
        std::thread::spawn(f)
            .join()
            .unwrap_or_else(|_| Err(anyhow::anyhow!("keyring thread panicked")))
    })
    .await
    .map_err(|e| anyhow::anyhow!("keyring join: {e}"))?
}

pub async fn store(password: &str) -> Result<()> {
    if password.is_empty() {
        anyhow::bail!("password is required");
    }
    let p = password.to_string();
    on_keyring_thread(move || {
        let pass_entry = open_entry(SERVICE)?;
        pass_entry.set_password(&p)?;
        Ok(())
    })
    .await
}

pub async fn load() -> Result<String> {
    on_keyring_thread(|| try_load(SERVICE)).await
}

fn try_load(service: &str) -> Result<String> {
    let pass_entry = open_entry(service)?;
    let p = pass_entry.get_password().map_err(map_not_found)?;
    Ok(p)
}

/// Open a keyring entry, replacing keyring v4's opaque `NoDefaultStore` error
/// ("No default store has been set...") with a short, actionable one when no
/// credential store is reachable.
fn open_entry(service: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(service, PASS_KEY).map_err(|error| {
        if keyring::Entry::store_status().is_err() {
            anyhow::anyhow!(
                "system credential store unavailable (no Secret Service provider on D-Bus)"
            )
        } else {
            anyhow::anyhow!(error)
        }
    })
}

fn map_not_found(e: keyring::Error) -> anyhow::Error {
    match e {
        keyring::Error::NoEntry => NotFound.into(),
        other => anyhow::anyhow!(other),
    }
}

pub fn is_not_found(err: &anyhow::Error) -> bool {
    err.downcast_ref::<NotFound>().is_some()
}
