use anyhow::{Context, Result};
use keyring_core::api::CredentialStoreApi as _;

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

/// Open an entry against a freshly created Secret Service store.
///
/// The `keyring` v4 facade initializes its platform store once per process and
/// caches the result forever, so a boot race with the Secret Service daemon
/// (e.g. Fedora 45's `oo7-daemon` not yet owning `org.freedesktop.secrets`)
/// permanently wedges that process: every later `Entry::new` returns
/// `NoDefaultStore`. Building a new store per operation keeps a failed read
/// recoverable, so the daemon's next retry can succeed after the store comes up
/// or restarts.
fn open_entry(service: &str) -> Result<keyring_core::Entry> {
    let store = zbus_secret_service_keyring_store::Store::new()
        .context("system credential store unavailable (Secret Service)")?;
    store
        .build(service, PASS_KEY, None)
        .with_context(|| format!("open keyring entry for service '{service}'"))
}

fn map_not_found(e: keyring_core::Error) -> anyhow::Error {
    match e {
        keyring_core::Error::NoEntry => NotFound.into(),
        other => anyhow::anyhow!(other),
    }
}

pub fn is_not_found(err: &anyhow::Error) -> bool {
    err.downcast_ref::<NotFound>().is_some()
}
