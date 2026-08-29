use anyhow::Result;

const SERVICE: &str = "wifilogin";
const USER_KEY: &str = "username";
const PASS_KEY: &str = "password";

// Keep compat with old latch service so existing creds still work if wifilogin not found.
const LEGACY_SERVICE: &str = "latch";

#[derive(Debug, thiserror::Error)]
#[error("credentials not found in keyring")]
pub struct NotFound;

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
    // Inside tokio: spawn_blocking to not block the async worker, then spawn a
    // raw OS thread to escape the Handle so zbus::blocking can create its own runtime.

    tokio::task::spawn_blocking(move || {
        std::thread::spawn(f)
            .join()
            .unwrap_or_else(|_| Err(anyhow::anyhow!("keyring thread panicked")))
    })
    .await
    .map_err(|e| anyhow::anyhow!("keyring join: {e}"))?
}

pub async fn store(username: &str, password: &str) -> Result<()> {
    if username.trim().is_empty() {
        anyhow::bail!("username is required");
    }
    if password.is_empty() {
        anyhow::bail!("password is required");
    }
    let u = username.to_string();
    let p = password.to_string();
    on_keyring_thread(move || {
        let user_entry = keyring::Entry::new(SERVICE, USER_KEY)?;
        let pass_entry = keyring::Entry::new(SERVICE, PASS_KEY)?;
        user_entry.set_password(&u)?;
        pass_entry.set_password(&p)?;
        Ok(())
    })
    .await
}

pub async fn load() -> Result<(String, String)> {
    on_keyring_thread(|| {
        // Try new service first, then legacy.
        if let Ok(v) = try_load(SERVICE) {
            return Ok(v);
        }
        if let Ok(v) = try_load(LEGACY_SERVICE) {
            return Ok(v);
        }
        Err(NotFound.into())
    })
    .await
}

fn try_load(service: &str) -> Result<(String, String)> {
    let user_entry = keyring::Entry::new(service, USER_KEY)?;
    let pass_entry = keyring::Entry::new(service, PASS_KEY)?;
    let u = user_entry.get_password().map_err(map_not_found)?;
    let p = pass_entry.get_password().map_err(map_not_found)?;
    Ok((u, p))
}

fn map_not_found(e: keyring::Error) -> anyhow::Error {
    match e {
        keyring::Error::NoEntry => NotFound.into(),
        other => anyhow::anyhow!(other),
    }
}

pub async fn delete() -> Result<()> {
    on_keyring_thread(|| {
        for svc in [SERVICE, LEGACY_SERVICE] {
            let _ = keyring::Entry::new(svc, USER_KEY).and_then(|e| match e.delete_credential() {
                Ok(()) => Ok(()),
                Err(keyring::Error::NoEntry) => Ok(()),
                Err(other) => Err(other),
            });
            let _ = keyring::Entry::new(svc, PASS_KEY).and_then(|e| match e.delete_credential() {
                Ok(()) => Ok(()),
                Err(keyring::Error::NoEntry) => Ok(()),
                Err(other) => Err(other),
            });
        }
        Ok(())
    })
    .await
}

pub fn is_not_found(err: &anyhow::Error) -> bool {
    err.downcast_ref::<NotFound>().is_some()
}
