use anyhow::{Context, Result};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::time::Duration;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, MessageStream, Proxy};

const NM_SERVICE: &str = "org.freedesktop.NetworkManager";
const NM_PATH: &str = "/org/freedesktop/NetworkManager";
const NM_IFACE: &str = "org.freedesktop.NetworkManager";
const NM_DEVICE_IFACE: &str = "org.freedesktop.NetworkManager.Device";
const NM_WIRELESS_IFACE: &str = "org.freedesktop.NetworkManager.Device.Wireless";
const NM_AP_IFACE: &str = "org.freedesktop.NetworkManager.AccessPoint";
const NM_ACTIVE_CONN_IFACE: &str = "org.freedesktop.NetworkManager.Connection.Active";
const NM_SETTINGS_PATH: &str = "/org/freedesktop/NetworkManager/Settings";
const NM_SETTINGS_IFACE: &str = "org.freedesktop.NetworkManager.Settings";
const NM_CONN_IFACE: &str = "org.freedesktop.NetworkManager.Settings.Connection";
const DBUS_PROPS_IFACE: &str = "org.freedesktop.DBus.Properties";
const DEVICE_TYPE_WIFI: u32 = 2;
const ACTIVATION_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
#[error("no WiFi device found")]
pub struct NoWifiDevice;

#[derive(Debug, thiserror::Error)]
#[error("saved wifi profile not found; connect once manually to {0}")]
pub struct NeedsProvisioning(pub String);

#[derive(Debug, Clone)]
pub struct EnsureResult {
    pub already_connected: bool,
    pub connection_id: String,
}

/// Anything that can report the current SSID and activate a saved profile.
/// A seam so the session controller can be tested without D-Bus.
pub trait Wifi: Send + Sync {
    async fn current_ssid(&self) -> Result<Option<String>>;
    async fn ensure_connected(&self, target: &str) -> Result<EnsureResult>;
}

/// Wake-up event. The daemon doesn't care *what* changed — it re-reads state
/// via `current_ssid()` on every step, which also handles device hotplug.
#[derive(Debug, Clone)]
pub enum Event {
    Wake,
}

#[derive(Debug, Clone)]
pub struct Manager {
    conn: Connection,
}

// Connection is Clone, so the manager is too — lets the daemon and controller
// share one system bus connection.
impl Wifi for Manager {
    async fn current_ssid(&self) -> Result<Option<String>> {
        self.current_ssid().await
    }
    async fn ensure_connected(&self, target: &str) -> Result<EnsureResult> {
        self.ensure_connected(target).await
    }
}

impl Manager {
    pub async fn new() -> Result<Self> {
        let conn = Connection::system()
            .await
            .context("connect to system bus")?;
        Ok(Self { conn })
    }

    /// Current SSID on the WiFi device, if any. Re-resolves the device on
    /// every call, so hotplugged/renamed devices are handled.
    pub async fn current_ssid(&self) -> Result<Option<String>> {
        let device = match self.wifi_device_path().await {
            Ok(p) => p,
            Err(e) if e.downcast_ref::<NoWifiDevice>().is_some() => return Ok(None),
            Err(e) => return Err(e),
        };

        let proxy = Proxy::new(&self.conn, NM_SERVICE, device, NM_WIRELESS_IFACE).await?;
        let ap_path: OwnedObjectPath = proxy
            .get_property("ActiveAccessPoint")
            .await
            .context("read ActiveAccessPoint")?;

        if ap_path.as_str() == "/" {
            return Ok(None);
        }

        let ap_proxy = Proxy::new(&self.conn, NM_SERVICE, ap_path, NM_AP_IFACE).await?;
        let ssid_bytes: Vec<u8> = ap_proxy.get_property("Ssid").await.context("read Ssid")?;

        if ssid_bytes.is_empty() {
            return Ok(None);
        }
        Ok(Some(String::from_utf8_lossy(&ssid_bytes).to_string()))
    }

    /// Activate a saved NM profile for `target` and wait until it is actually
    /// activated (ActivateConnection only starts activation).
    pub async fn ensure_connected(&self, target: &str) -> Result<EnsureResult> {
        if let Some(cur) = self.current_ssid().await?
            && cur == target
        {
            return Ok(EnsureResult {
                already_connected: true,
                connection_id: cur,
            });
        }

        let (conn_path, conn_id) = self
            .saved_connection_by_ssid(target)
            .await?
            .ok_or_else(|| NeedsProvisioning(target.to_string()))?;

        let device_path = self.wifi_device_path().await?;

        let nm_proxy = Proxy::new(&self.conn, NM_SERVICE, NM_PATH, NM_IFACE).await?;
        let active_path: OwnedObjectPath = nm_proxy
            .call(
                "ActivateConnection",
                &(conn_path, device_path, ObjectPath::try_from("/").unwrap()),
            )
            .await
            .map_err(|e| {
                let msg = e.to_string();
                if msg.contains("Not authorized")
                    || msg.contains("not authorized")
                    || msg.contains("AccessDenied")
                    || msg.contains("Permission denied")
                {
                    anyhow::Error::new(NeedsProvisioning(target.to_string()))
                } else {
                    anyhow::anyhow!("ActivateConnection {}: {e}", conn_id)
                }
            })?;

        self.wait_activated(&active_path, target).await?;

        Ok(EnsureResult {
            already_connected: false,
            connection_id: conn_id,
        })
    }

    async fn wait_activated(&self, active_path: &OwnedObjectPath, target: &str) -> Result<()> {
        let proxy = Proxy::new(&self.conn, NM_SERVICE, active_path.clone(), NM_ACTIVE_CONN_IFACE)
            .await?;
        let deadline = tokio::time::Instant::now() + ACTIVATION_TIMEOUT;
        loop {
            let state: u32 = proxy.get_property("State").await.unwrap_or(0);
            match state {
                2 => return Ok(()), // NM_ACTIVE_CONNECTION_STATE_ACTIVATED
                3 | 4 => {
                    anyhow::bail!("activation of {target} failed or was deactivated");
                }
                _ => {}
            }
            if tokio::time::Instant::now() >= deadline {
                anyhow::bail!("timed out waiting for {target} to activate");
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Event stream: D-Bus PropertiesChanged for any NM device State or
    /// ActiveAccessPoint change. Matches are not pinned to one device path so
    /// suspend/resume and hotplug don't silently kill the watch.
    pub async fn watch(&self) -> Result<tokio::sync::mpsc::Receiver<Event>> {
        let rules = [
            format!(
                "type='signal',sender='{NM_SERVICE}',interface='{DBUS_PROPS_IFACE}',member='PropertiesChanged',arg0='{NM_DEVICE_IFACE}'"
            ),
            format!(
                "type='signal',sender='{NM_SERVICE}',interface='{DBUS_PROPS_IFACE}',member='PropertiesChanged',arg0='{NM_WIRELESS_IFACE}'"
            ),
        ];
        for rule in &rules {
            self.add_match_raw(rule).await?;
        }

        let mut stream = MessageStream::from(self.conn.clone());
        let (tx, rx) = tokio::sync::mpsc::channel(16);

        tokio::spawn(async move {
            while let Some(msg) = stream.next().await {
                let Ok(msg) = msg else { continue };
                let header = msg.header();
                if header.message_type() != zbus::message::Type::Signal {
                    continue;
                }
                if header
                    .member()
                    .map(|m| m.as_str() != "PropertiesChanged")
                    .unwrap_or(true)
                {
                    continue;
                }
                // Body: (interface, a{sv} changed, as invalidated)
                let body = msg.body();
                let Ok((iface, changed, _invalidated)): Result<
                    (String, HashMap<String, Value<'_>>, Vec<String>),
                    _,
                > = body.deserialize()
                else {
                    continue;
                };
                let relevant = match iface.as_str() {
                    NM_DEVICE_IFACE => changed.contains_key("State"),
                    NM_WIRELESS_IFACE => changed.contains_key("ActiveAccessPoint"),
                    _ => false,
                };
                if relevant && tx.send(Event::Wake).await.is_err() {
                    break; // receiver dropped — daemon is shutting down
                }
            }
        });

        Ok(rx)
    }

    async fn add_match_raw(&self, rule: &str) -> Result<()> {
        let proxy = Proxy::new(
            &self.conn,
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
        )
        .await?;
        proxy.call::<_, _, ()>("AddMatch", &(rule,)).await?;
        Ok(())
    }

    async fn wifi_device_path(&self) -> Result<OwnedObjectPath> {
        let proxy = Proxy::new(&self.conn, NM_SERVICE, NM_PATH, NM_IFACE).await?;
        let devices: Vec<OwnedObjectPath> =
            proxy.call("GetDevices", &()).await.context("GetDevices")?;

        for path in devices {
            let p = Proxy::new(&self.conn, NM_SERVICE, path.clone(), NM_DEVICE_IFACE).await?;
            let Ok(dtype): Result<u32, _> = p.get_property("DeviceType").await else {
                continue;
            };
            if dtype == DEVICE_TYPE_WIFI {
                return Ok(path);
            }
        }
        Err(NoWifiDevice.into())
    }

    async fn saved_connection_by_ssid(
        &self,
        target: &str,
    ) -> Result<Option<(OwnedObjectPath, String)>> {
        let proxy = Proxy::new(&self.conn, NM_SERVICE, NM_SETTINGS_PATH, NM_SETTINGS_IFACE).await?;
        let conns: Vec<OwnedObjectPath> = proxy
            .call("ListConnections", &())
            .await
            .context("ListConnections")?;

        for path in conns {
            let cproxy = Proxy::new(&self.conn, NM_SERVICE, path.clone(), NM_CONN_IFACE).await?;
            let settings: HashMap<String, HashMap<String, OwnedValue>> =
                match cproxy.call("GetSettings", &()).await {
                    Ok(v) => v,
                    Err(_) => continue,
                };
            let Some(wsec) = settings.get("802-11-wireless") else {
                continue;
            };
            let Some(v) = wsec.get("ssid") else { continue };
            let Ok(ssid_bytes) = Vec::<u8>::try_from(v.clone()) else { continue };
            let ssid = String::from_utf8_lossy(&ssid_bytes).to_string();
            if ssid != target {
                continue;
            }
            let id = settings
                .get("connection")
                .and_then(|m| m.get("id"))
                .and_then(|v| String::try_from(v.clone()).ok())
                .unwrap_or_else(|| ssid.clone());
            return Ok(Some((path, id)));
        }
        Ok(None)
    }
}

pub fn is_provisioning(err: &anyhow::Error) -> bool {
    err.downcast_ref::<NeedsProvisioning>().is_some()
}
