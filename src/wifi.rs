use anyhow::{Context, Result};
use futures_util::StreamExt;
use std::collections::HashMap;
use zbus::zvariant::{OwnedObjectPath, Value};
use zbus::{Connection, MessageStream, Proxy};

const NM_SERVICE: &str = "org.freedesktop.NetworkManager";
const NM_PATH: &str = "/org/freedesktop/NetworkManager";
const NM_IFACE: &str = "org.freedesktop.NetworkManager";
const NM_DEVICE_IFACE: &str = "org.freedesktop.NetworkManager.Device";
const NM_WIRELESS_IFACE: &str = "org.freedesktop.NetworkManager.Device.Wireless";
const NM_AP_IFACE: &str = "org.freedesktop.NetworkManager.AccessPoint";
const NM_ACTIVE_CONN_IFACE: &str = "org.freedesktop.NetworkManager.Connection.Active";
const DBUS_PROPS_IFACE: &str = "org.freedesktop.DBus.Properties";
const DEVICE_TYPE_WIFI: u32 = 2;
const ACTIVE_CONNECTION_ACTIVATED: u32 = 2;

/// NetworkManager's connectivity assessment. It is deliberately distinct
/// from reachability: only `Portal` authorizes an automatic login attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connectivity {
    Unknown,
    None,
    Portal,
    Limited,
    Full,
}

impl Connectivity {
    fn from_nm(value: u32) -> Self {
        match value {
            1 => Self::None,
            2 => Self::Portal,
            3 => Self::Limited,
            4 => Self::Full,
            _ => Self::Unknown,
        }
    }
}

/// Facts needed to decide whether portal credentials may be submitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkState {
    /// There is no Wi-Fi adapter, or it has no active access point.
    Disconnected,
    /// Wi-Fi is associated. `is_default` means NetworkManager uses that
    /// connection for the default route rather than Ethernet or a VPN.
    Connected {
        ssid: String,
        connection_uuid: String,
        is_activated: bool,
        is_default: bool,
        connectivity: Connectivity,
    },
}

/// D-Bus changes are intentionally collapsed: a fresh snapshot is more
/// reliable than attempting to interpret every transition in a signal.
#[derive(Debug, Clone, Copy)]
pub enum Event {
    NetworkChanged,
}

/// The session controller's small seam. It neither scans for nor activates
/// networks, so tests and production share the same safety rule.
pub trait Wifi: Send + Sync {
    async fn network_state(&self) -> Result<NetworkState>;
}

#[derive(Debug, Clone)]
pub struct Manager {
    conn: Connection,
}

impl Wifi for Manager {
    async fn network_state(&self) -> Result<NetworkState> {
        self.network_state().await
    }
}

impl Manager {
    pub async fn new() -> Result<Self> {
        let conn = Connection::system()
            .await
            .context("connect to NetworkManager system bus")?;
        Ok(Self { conn })
    }

    /// Reads one coherent-enough view of the active Wi-Fi connection. D-Bus
    /// signals cause a new read, handling missed signals and device hotplug.
    pub async fn network_state(&self) -> Result<NetworkState> {
        let mut first_active = None;
        let mut first_error = None;

        for device_path in self.wifi_device_paths().await? {
            match self.device_network_state(device_path).await {
                Ok(Some(network)) if network.is_default_route() => return Ok(network),
                Ok(Some(network)) if first_active.is_none() => first_active = Some(network),
                Ok(Some(_)) | Ok(None) => {}
                Err(error) if first_error.is_none() => first_error = Some(error),
                Err(_) => {}
            }
        }

        match (first_active, first_error) {
            (Some(network), _) => Ok(network),
            (None, Some(error)) => Err(error),
            (None, None) => Ok(NetworkState::Disconnected),
        }
    }

    async fn device_network_state(
        &self,
        device_path: OwnedObjectPath,
    ) -> Result<Option<NetworkState>> {
        let wireless = Proxy::new(
            &self.conn,
            NM_SERVICE,
            device_path.clone(),
            NM_WIRELESS_IFACE,
        )
        .await?;
        let access_point: OwnedObjectPath = wireless
            .get_property("ActiveAccessPoint")
            .await
            .context("read active Wi-Fi access point")?;
        if access_point.as_str() == "/" {
            return Ok(None);
        }

        let ap = Proxy::new(&self.conn, NM_SERVICE, access_point, NM_AP_IFACE).await?;
        let ssid: Vec<u8> = ap.get_property("Ssid").await.context("read Wi-Fi SSID")?;
        if ssid.is_empty() {
            return Ok(None);
        }

        // ActiveConnection belongs to the base Device interface, while
        // ActiveAccessPoint belongs to the Wireless interface.
        let device = Proxy::new(&self.conn, NM_SERVICE, device_path, NM_DEVICE_IFACE).await?;
        let active_connection: OwnedObjectPath = device
            .get_property("ActiveConnection")
            .await
            .context("read active Wi-Fi connection")?;
        if active_connection.as_str() == "/" {
            return Ok(None);
        }
        let active = Proxy::new(
            &self.conn,
            NM_SERVICE,
            active_connection,
            NM_ACTIVE_CONN_IFACE,
        )
        .await?;
        let connection_uuid: String = active
            .get_property("Uuid")
            .await
            .context("read active Wi-Fi connection UUID")?;
        let is_activated = active
            .get_property::<u32>("State")
            .await
            .map(|state| state == ACTIVE_CONNECTION_ACTIVATED)
            .unwrap_or(false);
        // `Default` covers IPv4 and `Default6` IPv6. Either is sufficient:
        // portal traffic must be allowed on a connection that owns at least
        // one default route.
        let is_default = active.get_property("Default").await.unwrap_or(false)
            || active.get_property("Default6").await.unwrap_or(false);

        let manager = Proxy::new(&self.conn, NM_SERVICE, NM_PATH, NM_IFACE).await?;
        let connectivity = manager
            .get_property::<u32>("Connectivity")
            .await
            .map(Connectivity::from_nm)
            .unwrap_or(Connectivity::Unknown);

        Ok(Some(NetworkState::Connected {
            ssid: String::from_utf8_lossy(&ssid).into_owned(),
            connection_uuid,
            is_activated,
            is_default,
            connectivity,
        }))
    }

    /// Subscribe before the daemon begins processing. This watches
    /// connectivity, default-route changes, Wi-Fi association, and active
    /// connection state; no periodic D-Bus or HTTP polling is used.
    pub async fn watch(&self) -> Result<tokio::sync::mpsc::Receiver<Event>> {
        for interface in [
            NM_IFACE,
            NM_DEVICE_IFACE,
            NM_WIRELESS_IFACE,
            NM_ACTIVE_CONN_IFACE,
        ] {
            self.add_match_raw(&format!(
                "type='signal',sender='{NM_SERVICE}',interface='{DBUS_PROPS_IFACE}',member='PropertiesChanged',arg0='{interface}'"
            ))
            .await?;
        }
        for (interface, member) in [
            (NM_IFACE, "DeviceAdded"),
            (NM_IFACE, "DeviceRemoved"),
            (NM_IFACE, "StateChanged"),
            (NM_DEVICE_IFACE, "StateChanged"),
            (NM_ACTIVE_CONN_IFACE, "StateChanged"),
        ] {
            self.add_match_raw(&format!(
                "type='signal',sender='{NM_SERVICE}',interface='{interface}',member='{member}'"
            ))
            .await?;
        }
        self.add_match_raw(&format!(
            "type='signal',sender='org.freedesktop.DBus',interface='org.freedesktop.DBus',member='NameOwnerChanged',arg0='{NM_SERVICE}'"
        ))
        .await?;

        let mut stream = MessageStream::from(self.conn.clone());
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        tokio::spawn(async move {
            while let Some(message) = stream.next().await {
                let message = match message {
                    Ok(message) => message,
                    Err(error) => {
                        tracing::warn!(%error, "NetworkManager D-Bus event stream failed");
                        break;
                    }
                };
                let header = message.header();
                if header.message_type() != zbus::message::Type::Signal {
                    continue;
                }
                let interface = header.interface().map(|interface| interface.as_str());
                let member = header.member().map(|member| member.as_str());
                if is_lifecycle_signal(interface, member) {
                    let _ = sender.try_send(Event::NetworkChanged);
                    continue;
                }
                if interface != Some(DBUS_PROPS_IFACE) || member != Some("PropertiesChanged") {
                    continue;
                }
                let body = message.body();
                let Ok((interface, changed, _invalidated)): Result<
                    (String, HashMap<String, Value<'_>>, Vec<String>),
                    _,
                > = body.deserialize() else {
                    continue;
                };
                if affects_network_state(&interface, &changed) {
                    // One pending event is enough: processing it reads a fresh
                    // snapshot. Dropping duplicates prevents signal storms.
                    let _ = sender.try_send(Event::NetworkChanged);
                }
            }
        });
        Ok(receiver)
    }

    async fn add_match_raw(&self, rule: &str) -> Result<()> {
        let dbus = Proxy::new(
            &self.conn,
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
        )
        .await?;
        dbus.call::<_, _, ()>("AddMatch", &(rule,)).await?;
        Ok(())
    }

    async fn wifi_device_paths(&self) -> Result<Vec<OwnedObjectPath>> {
        let manager = Proxy::new(&self.conn, NM_SERVICE, NM_PATH, NM_IFACE).await?;
        let devices: Vec<OwnedObjectPath> = manager.call("GetDevices", &()).await?;
        let mut wifi_devices = Vec::new();
        for path in devices {
            let device = Proxy::new(&self.conn, NM_SERVICE, path.clone(), NM_DEVICE_IFACE).await?;
            if device.get_property::<u32>("DeviceType").await.ok() == Some(DEVICE_TYPE_WIFI) {
                wifi_devices.push(path);
            }
        }
        Ok(wifi_devices)
    }
}

fn is_lifecycle_signal(interface: Option<&str>, member: Option<&str>) -> bool {
    matches!(
        (interface, member),
        (
            Some(NM_IFACE),
            Some("DeviceAdded" | "DeviceRemoved" | "StateChanged")
        ) | (
            Some(NM_DEVICE_IFACE | NM_ACTIVE_CONN_IFACE),
            Some("StateChanged")
        ) | (Some("org.freedesktop.DBus"), Some("NameOwnerChanged"))
    )
}

impl NetworkState {
    fn is_default_route(&self) -> bool {
        matches!(
            self,
            Self::Connected {
                is_default: true,
                ..
            }
        )
    }
}

fn affects_network_state(interface: &str, changed: &HashMap<String, Value<'_>>) -> bool {
    match interface {
        NM_IFACE => {
            changed.contains_key("Connectivity") || changed.contains_key("PrimaryConnection")
        }
        NM_DEVICE_IFACE => changed.contains_key("State"),
        NM_WIRELESS_IFACE => {
            changed.contains_key("ActiveAccessPoint") || changed.contains_key("ActiveConnection")
        }
        NM_ACTIVE_CONN_IFACE => changed.contains_key("State") || changed.contains_key("Default"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_networkmanager_connectivity_values() {
        assert_eq!(Connectivity::from_nm(0), Connectivity::Unknown);
        assert_eq!(Connectivity::from_nm(2), Connectivity::Portal);
        assert_eq!(Connectivity::from_nm(4), Connectivity::Full);
        assert_eq!(Connectivity::from_nm(99), Connectivity::Unknown);
    }
}
