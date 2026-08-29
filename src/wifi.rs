use anyhow::{Context, Result};
use futures_util::StreamExt;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, MessageStream, Proxy};

const NM_SERVICE: &str = "org.freedesktop.NetworkManager";
const NM_PATH: &str = "/org/freedesktop/NetworkManager";
const NM_IFACE: &str = "org.freedesktop.NetworkManager";
const NM_DEVICE_IFACE: &str = "org.freedesktop.NetworkManager.Device";
const NM_WIRELESS_IFACE: &str = "org.freedesktop.NetworkManager.Device.Wireless";
const NM_AP_IFACE: &str = "org.freedesktop.NetworkManager.AccessPoint";
const NM_SETTINGS_PATH: &str = "/org/freedesktop/NetworkManager/Settings";
const NM_SETTINGS_IFACE: &str = "org.freedesktop.NetworkManager.Settings";
const NM_CONN_IFACE: &str = "org.freedesktop.NetworkManager.Settings.Connection";
const DBUS_PROPS_IFACE: &str = "org.freedesktop.DBus.Properties";
const DEVICE_TYPE_WIFI: u32 = 2;

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
    #[allow(dead_code)]
    pub ssid: String,
    #[allow(dead_code)]
    pub active_path: OwnedObjectPath,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum Event {
    SsidChanged { ssid: String, connected: bool },
    DeviceStateChanged { state: u32 },
}

pub struct Manager {
    conn: Connection,
}

impl Manager {
    pub async fn new() -> Result<Self> {
        let conn = Connection::system()
            .await
            .context("connect to system bus")?;
        Ok(Self { conn })
    }

    /// Current SSID on the WiFi device, if any.
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
        let ssid = String::from_utf8_lossy(&ssid_bytes).to_string();
        Ok(Some(ssid))
    }

    pub async fn ensure_connected(&self, target: &str) -> Result<EnsureResult> {
        if let Some(cur) = self.current_ssid().await?
            && cur == target
        {
            return Ok(EnsureResult {
                already_connected: true,
                connection_id: cur.clone(),
                ssid: cur,
                active_path: OwnedObjectPath::try_from("/").unwrap(),
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
                    anyhow::Error::from(NeedsProvisioning(target.to_string()))
                } else {
                    anyhow::anyhow!("ActivateConnection {}: {e}", conn_id)
                }
            })?;

        Ok(EnsureResult {
            already_connected: false,
            connection_id: conn_id,
            ssid: target.to_string(),
            active_path,
        })
    }

    /// Event stream: D-Bus PropertiesChanged for ActiveAccessPoint + Device State.
    /// Caller should `select!` on this; no polling timer needed.
    pub async fn watch(&self) -> Result<tokio::sync::mpsc::Receiver<Event>> {
        let device_path = self.wifi_device_path().await?;
        let device_str = device_path.to_string();

        // Add match rules. Using string form for compatibility across zbus versions.
        // We watch both interfaces on the same device path.
        let rule_wireless = format!(
            "type='signal',sender='{}',interface='{}',member='PropertiesChanged',path='{}',arg0='{}'",
            NM_SERVICE, DBUS_PROPS_IFACE, device_str, NM_WIRELESS_IFACE
        );
        let rule_device = format!(
            "type='signal',sender='{}',interface='{}',member='PropertiesChanged',path='{}',arg0='{}'",
            NM_SERVICE, DBUS_PROPS_IFACE, device_str, NM_DEVICE_IFACE
        );

        // zbus 5: add_match takes MatchRule — we use raw string via low-level call if needed.
        // Fallback: use `call` AddMatch directly.
        self.add_match_raw(&rule_wireless).await?;
        self.add_match_raw(&rule_device).await?;

        let mut stream = MessageStream::from(self.conn.clone());
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let conn = self.conn.clone();

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
                // path filter
                if header
                    .path()
                    .map(|p| p.as_str() != device_str)
                    .unwrap_or(true)
                {
                    continue;
                }
                let body = msg.body();
                // Body: (s, a{sv}, as)
                let Ok((iface, changed, _invalidated)): Result<
                    (
                        String,
                        std::collections::HashMap<String, Value<'_>>,
                        Vec<String>,
                    ),
                    _,
                > = body.deserialize() else {
                    continue;
                };

                match iface.as_str() {
                    NM_WIRELESS_IFACE => {
                        if let Some(v) = changed.get("ActiveAccessPoint") {
                            let ap_str: String = match v {
                                Value::ObjectPath(p) => p.to_string(),
                                _ => {
                                    // try decode as OwnedObjectPath via OwnedValue
                                    if let Ok(ov) = OwnedValue::try_from(v.clone()) {
                                        if let Ok(p) = OwnedObjectPath::try_from(ov) {
                                            p.to_string()
                                        } else {
                                            v.to_string().trim_matches('"').to_string()
                                        }
                                    } else {
                                        v.to_string().trim_matches('"').to_string()
                                    }
                                }
                            };
                            if ap_str == "/" || ap_str.is_empty() {
                                let _ = tx
                                    .send(Event::SsidChanged {
                                        ssid: String::new(),
                                        connected: false,
                                    })
                                    .await;
                            } else {
                                match ssid_by_ap(&conn, &ap_str).await {
                                    Ok(ssid) => {
                                        let _ = tx
                                            .send(Event::SsidChanged {
                                                ssid,
                                                connected: true,
                                            })
                                            .await;
                                    }
                                    Err(_) => {
                                        let _ = tx
                                            .send(Event::SsidChanged {
                                                ssid: String::new(),
                                                connected: true,
                                            })
                                            .await;
                                    }
                                }
                            }
                        }
                    }
                    NM_DEVICE_IFACE => {
                        if let Some(v) = changed.get("State") {
                            // try u32 decode via multiple paths
                            let state_opt: Option<u32> = u32::try_from(v.clone()).ok().or({
                                if let Value::U32(u) = v {
                                    Some(*u)
                                } else {
                                    None
                                }
                            });
                            if let Some(state) = state_opt {
                                let _ = tx.send(Event::DeviceStateChanged { state }).await;
                            }
                        }
                    }
                    _ => {}
                }
            }
        });

        Ok(rx)
    }

    async fn add_match_raw(&self, rule: &str) -> Result<()> {
        // Use low-level dbus call org.freedesktop.DBus.AddMatch
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
            let settings: std::collections::HashMap<
                String,
                std::collections::HashMap<String, OwnedValue>,
            > = match cproxy.call("GetSettings", &()).await {
                Ok(v) => v,
                Err(_) => continue,
            };
            let Some(wsec) = settings.get("802-11-wireless") else {
                continue;
            };
            let Some(v) = wsec.get("ssid") else { continue };
            let ssid_bytes: Vec<u8> = match Vec::<u8>::try_from(v.clone()) {
                Ok(b) => b,
                Err(_) => continue,
            };
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

async fn ssid_by_ap(conn: &Connection, ap_path: &str) -> Result<String> {
    let proxy = Proxy::new(conn, NM_SERVICE, ap_path, NM_AP_IFACE).await?;
    let bytes: Vec<u8> = proxy.get_property("Ssid").await.context("read Ssid")?;
    Ok(String::from_utf8_lossy(&bytes).to_string())
}

pub fn is_provisioning(err: &anyhow::Error) -> bool {
    err.downcast_ref::<NeedsProvisioning>().is_some()
}
