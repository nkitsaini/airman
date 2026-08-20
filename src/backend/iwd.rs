use std::{collections::BTreeMap, collections::HashMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use tokio::sync::{broadcast, Mutex};
use zbus::{interface, Connection, Proxy};
use zvariant::{OwnedObjectPath, OwnedValue};

use super::{
    dbus::{dbus_failure, probe_service, spawn_signal_supervisor, ServiceClock, SnapshotFn},
    BackendAction, BackendCommand, BackendDiagnostics, BackendFailure, CapabilityMap,
    OperationAcceptance, ProbeResult, RadioBackend, Secret,
};
use crate::domain::{
    BackendEvent, BackendKind, BackendPayload, Capability, CapabilityState, ConnectionState,
    Connectivity, DesiredState, EntityId, ErrorCategory, InterfaceId, OperationId, OperationPhase,
    Ssid, WifiInterface, WifiNetwork, WifiNetworkId, WifiSecurity, WifiSnapshot,
};

const SERVICE: &str = "net.connman.iwd";
const OBJECT_MANAGER: &str = "org.freedesktop.DBus.ObjectManager";
const DEVICE_INTERFACE: &str = "net.connman.iwd.Device";
const STATION_INTERFACE: &str = "net.connman.iwd.Station";
const NETWORK_INTERFACE: &str = "net.connman.iwd.Network";
const KNOWN_NETWORK_INTERFACE: &str = "net.connman.iwd.KnownNetwork";
const AGENT_MANAGER_INTERFACE: &str = "net.connman.iwd.AgentManager";
const AGENT_MANAGER_PATH: &str = "/net/connman/iwd";
const AGENT_PATH: &str = "/dev/airman/iwd_agent";

type Properties = HashMap<String, OwnedValue>;
type Interfaces = HashMap<String, Properties>;
type ManagedObjects = HashMap<OwnedObjectPath, Interfaces>;

struct IwdCredentialAgent {
    credential: Mutex<Option<Secret>>,
}

#[interface(name = "net.connman.iwd.Agent")]
impl IwdCredentialAgent {
    async fn release(&self) {}

    async fn request_passphrase(&self, _network: OwnedObjectPath) -> zbus::fdo::Result<String> {
        self.take_credential().await
    }

    async fn request_private_key_passphrase(
        &self,
        _network: OwnedObjectPath,
    ) -> zbus::fdo::Result<String> {
        self.take_credential().await
    }

    async fn cancel(&self, reason: String) {
        tracing::debug!(%reason, "iwd canceled its credential request");
    }
}

impl IwdCredentialAgent {
    async fn take_credential(&self) -> zbus::fdo::Result<String> {
        self.credential
            .lock()
            .await
            .take()
            .map(|secret| secret.expose().to_owned())
            .ok_or_else(|| zbus::fdo::Error::Failed("no credential is pending".into()))
    }
}

#[derive(Debug, Clone)]
struct IwdNetwork {
    path: OwnedObjectPath,
    id: WifiNetworkId,
    signal: u8,
    connected: bool,
    saved: bool,
    auto_join: bool,
    station_state: String,
}

pub struct IwdBackend {
    connection: Connection,
    interface_filter: Option<String>,
    clock: Arc<ServiceClock>,
    events: broadcast::Sender<BackendEvent>,
    agent_lock: Mutex<()>,
}

impl IwdBackend {
    pub async fn new(connection: Connection, interface_filter: Option<String>) -> Arc<Self> {
        let (events, _) = broadcast::channel(256);
        let backend = Arc::new(Self {
            connection,
            interface_filter,
            clock: Arc::new(ServiceClock::new(BackendKind::Iwd)),
            events,
            agent_lock: Mutex::new(()),
        });
        let weak = Arc::downgrade(&backend);
        let snapshot: SnapshotFn = Arc::new(move || {
            let weak = weak.clone();
            Box::pin(async move {
                let backend = weak.upgrade().ok_or_else(stopped)?;
                backend.snapshot_inner().await
            })
        });
        spawn_signal_supervisor(
            backend.connection.clone(),
            SERVICE,
            backend.clock.clone(),
            backend.events.clone(),
            snapshot,
        );
        backend
    }

    async fn managed_objects(&self) -> Result<ManagedObjects, BackendFailure> {
        Proxy::new(&self.connection, SERVICE, "/", OBJECT_MANAGER)
            .await
            .map_err(|error| dbus_failure("inspect iwd objects", error))?
            .call("GetManagedObjects", &())
            .await
            .map_err(|error| dbus_failure("inspect iwd objects", error))
    }

    async fn snapshot_inner(&self) -> Result<BackendEvent, BackendFailure> {
        let objects = self.managed_objects().await?;
        let mut interfaces = Vec::new();
        let mut networks = Vec::new();
        let now = super::dbus::monotonic_ms();

        for (station_path, station_properties) in station_entries(&objects) {
            let Some(device_properties) = objects
                .get(station_path)
                .and_then(|entry| entry.get(DEVICE_INTERFACE))
            else {
                continue;
            };
            let Some(name) = string_property(device_properties, "Name") else {
                continue;
            };
            if self
                .interface_filter
                .as_ref()
                .is_some_and(|filter| filter != &name)
            {
                continue;
            }
            let interface = InterfaceId(name);
            let station_state = string_property(station_properties, "State")
                .unwrap_or_else(|| "disconnected".into());
            interfaces.push(WifiInterface {
                id: interface.clone(),
                backend: BackendKind::Iwd,
                powered: bool_property(device_properties, "Powered").unwrap_or(false),
                scanning: bool_property(station_properties, "Scanning").unwrap_or(false),
                last_scan_ms: None,
                addresses: super::system::interface_addresses(&interface.0),
                capabilities: iwd_capabilities(),
            });

            for (network_path, signal) in self.ordered_networks(station_path).await {
                let Some(properties) = objects
                    .get(&network_path)
                    .and_then(|entry| entry.get(NETWORK_INTERFACE))
                else {
                    continue;
                };
                let Some(network) = iwd_network(
                    &interface,
                    &station_state,
                    network_path,
                    properties,
                    &objects,
                    signal,
                ) else {
                    continue;
                };
                networks.push(WifiNetwork {
                    display_name: network.id.ssid.display(),
                    id: network.id,
                    signal: network.signal,
                    state: if network.connected {
                        map_station_state(&network.station_state)
                    } else {
                        ConnectionState::Disconnected
                    },
                    connectivity: if network.connected && network.station_state == "connected" {
                        Connectivity::Unknown
                    } else {
                        Connectivity::None
                    },
                    saved: network.saved,
                    auto_join: network.auto_join,
                    bss_count: 1,
                    active_bssid: None,
                    present: true,
                    last_seen_ms: now,
                });
            }

            for known in objects
                .values()
                .filter_map(|interfaces| interfaces.get(KNOWN_NETWORK_INTERFACE))
            {
                let Some(name) = string_property(known, "Name") else {
                    continue;
                };
                let Some(kind) = string_property(known, "Type") else {
                    continue;
                };
                let id = WifiNetworkId {
                    interface: interface.clone(),
                    ssid: Ssid(name.into_bytes()),
                    security: map_security(&kind),
                };
                if networks.iter().any(|network| network.id == id) {
                    continue;
                }
                networks.push(WifiNetwork {
                    display_name: id.ssid.display(),
                    id,
                    signal: 0,
                    state: ConnectionState::Disconnected,
                    connectivity: Connectivity::None,
                    saved: true,
                    auto_join: bool_property(known, "AutoConnect").unwrap_or(true),
                    bss_count: 0,
                    active_bssid: None,
                    present: false,
                    last_seen_ms: now,
                });
            }
        }

        Ok(self.clock.event(BackendPayload::WifiSnapshot(WifiSnapshot {
            interfaces,
            networks,
        })))
    }

    async fn ordered_networks(&self, station: &OwnedObjectPath) -> Vec<(OwnedObjectPath, i16)> {
        match Proxy::new(&self.connection, SERVICE, station, STATION_INTERFACE).await {
            Ok(proxy) => proxy
                .call("GetOrderedNetworks", &())
                .await
                .unwrap_or_default(),
            Err(error) => {
                tracing::debug!(%error, "could not inspect an iwd station");
                Vec::new()
            }
        }
    }

    async fn find_network(&self, id: &WifiNetworkId) -> Result<IwdNetwork, BackendFailure> {
        let objects = self.managed_objects().await?;
        for (station_path, station_properties) in station_entries(&objects) {
            let Some(device) = objects
                .get(station_path)
                .and_then(|entry| entry.get(DEVICE_INTERFACE))
            else {
                continue;
            };
            if string_property(device, "Name").as_deref() != Some(id.interface.0.as_str()) {
                continue;
            }
            let state = string_property(station_properties, "State").unwrap_or_default();
            for (path, signal) in self.ordered_networks(station_path).await {
                let Some(properties) = objects
                    .get(&path)
                    .and_then(|entry| entry.get(NETWORK_INTERFACE))
                else {
                    continue;
                };
                if let Some(network) =
                    iwd_network(&id.interface, &state, path, properties, &objects, signal)
                {
                    if network.id == *id {
                        return Ok(network);
                    }
                }
            }
        }
        Err(not_found(format!(
            "{} is no longer in iwd's scan results",
            id.ssid.display()
        )))
    }

    async fn find_station(&self, id: &InterfaceId) -> Result<OwnedObjectPath, BackendFailure> {
        self.managed_objects()
            .await?
            .into_iter()
            .find(|(_, interfaces)| {
                interfaces
                    .get(DEVICE_INTERFACE)
                    .and_then(|properties| string_property(properties, "Name"))
                    .as_deref()
                    == Some(id.0.as_str())
                    && interfaces.contains_key(STATION_INTERFACE)
            })
            .map(|(path, _)| path)
            .ok_or_else(|| not_found(format!("iwd no longer controls {}", id.0)))
    }

    async fn connect(
        &self,
        network: &IwdNetwork,
        credential: Option<Secret>,
    ) -> Result<(), BackendFailure> {
        if network.saved || network.id.security == WifiSecurity::Open {
            return self.connect_path(&network.path).await;
        }
        if network.id.security == WifiSecurity::Wep {
            return Err(unsupported(
                "iwd does not support connecting to WEP networks",
            ));
        }
        let credential = credential.ok_or_else(|| BackendFailure {
            category: ErrorCategory::MissingSecrets,
            summary: format!("{} needs a credential", network.id.ssid.display()),
            detail: "iwd has no provisioned profile for this secured network".into(),
            recovery: vec!["Retry and enter the network passphrase".into()],
            retryable: true,
            raw_code: Some("missing-credential".into()),
        })?;
        let _guard = self.agent_lock.lock().await;
        let path = OwnedObjectPath::try_from(AGENT_PATH)
            .map_err(|error| dbus_failure("prepare the iwd credential agent", error))?;
        self.connection
            .object_server()
            .at(
                path.clone(),
                IwdCredentialAgent {
                    credential: Mutex::new(Some(credential)),
                },
            )
            .await
            .map_err(|error| dbus_failure("prepare the iwd credential agent", error))?;
        let manager = Proxy::new(
            &self.connection,
            SERVICE,
            AGENT_MANAGER_PATH,
            AGENT_MANAGER_INTERFACE,
        )
        .await
        .map_err(|error| dbus_failure("register the iwd credential agent", error))?;
        let registered = manager
            .call::<_, _, ()>("RegisterAgent", &(path.clone(),))
            .await
            .is_ok();
        let result = if registered {
            self.connect_path(&network.path).await
        } else {
            Err(BackendFailure {
                category: ErrorCategory::Busy,
                summary: "Another iwd credential agent is already active".into(),
                detail: "iwd permits one credential agent; airman did not replace the existing desktop agent".into(),
                recovery: vec![
                    "Answer the existing desktop prompt, or stop that agent and retry".into(),
                ],
                retryable: true,
                raw_code: Some("iwd-agent-conflict".into()),
            })
        };
        if registered {
            let _ = manager
                .call::<_, _, ()>("UnregisterAgent", &(path.clone(),))
                .await;
        }
        let _ = self
            .connection
            .object_server()
            .remove::<IwdCredentialAgent, _>(path)
            .await;
        result
    }

    async fn connect_path(&self, path: &OwnedObjectPath) -> Result<(), BackendFailure> {
        Proxy::new(&self.connection, SERVICE, path, NETWORK_INTERFACE)
            .await
            .map_err(|error| dbus_failure("connect with iwd", error))?
            .call("Connect", &())
            .await
            .map_err(|error| dbus_failure("connect with iwd", error))
    }
}

#[async_trait]
impl RadioBackend for IwdBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Iwd
    }

    async fn probe(&self) -> ProbeResult {
        probe_service(&self.connection, SERVICE, self.kind()).await
    }

    fn subscribe(&self) -> broadcast::Receiver<BackendEvent> {
        self.events.subscribe()
    }

    async fn snapshot(&self) -> Result<BackendEvent, BackendFailure> {
        self.snapshot_inner().await
    }

    async fn capabilities(&self) -> CapabilityMap {
        iwd_capabilities()
    }

    async fn execute(
        &self,
        command: BackendCommand,
    ) -> Result<OperationAcceptance, BackendFailure> {
        match (command.action, &command.target) {
            (BackendAction::Scan, EntityId::WifiInterface(id)) => {
                let station = self.find_station(id).await?;
                Proxy::new(&self.connection, SERVICE, station, STATION_INTERFACE)
                    .await
                    .map_err(|error| dbus_failure("scan with iwd", error))?
                    .call::<_, _, ()>("Scan", &())
                    .await
                    .map_err(|error| dbus_failure("scan with iwd", error))?;
            }
            (BackendAction::Connect, EntityId::Wifi(id)) => {
                let network = self.find_network(id).await?;
                self.connect(&network, command.credential).await?;
            }
            (BackendAction::Disconnect, EntityId::Wifi(id)) => {
                let station = self.find_station(&id.interface).await?;
                Proxy::new(&self.connection, SERVICE, station, STATION_INTERFACE)
                    .await
                    .map_err(|error| dbus_failure("disconnect with iwd", error))?
                    .call::<_, _, ()>("Disconnect", &())
                    .await
                    .map_err(|error| dbus_failure("disconnect with iwd", error))?;
            }
            (BackendAction::SetPowered(powered), EntityId::WifiInterface(id)) => {
                let device = self.find_station(id).await?;
                Proxy::new(&self.connection, SERVICE, device, DEVICE_INTERFACE)
                    .await
                    .map_err(|error| dbus_failure("change the iwd radio state", error))?
                    .set_property("Powered", powered)
                    .await
                    .map_err(|error| dbus_failure("change the iwd radio state", error))?;
            }
            (BackendAction::Forget, EntityId::Wifi(id)) => {
                let network = self.find_network(id).await?;
                let Some(path) = known_network_path(&self.managed_objects().await?, &network.path)
                else {
                    return Ok(acceptance(command.desired));
                };
                Proxy::new(&self.connection, SERVICE, path, KNOWN_NETWORK_INTERFACE)
                    .await
                    .map_err(|error| dbus_failure("forget the iwd network", error))?
                    .call::<_, _, ()>("Forget", &())
                    .await
                    .map_err(|error| dbus_failure("forget the iwd network", error))?;
            }
            (BackendAction::UpdateProfile(update), EntityId::Wifi(id))
                if update.auto_join.is_some() =>
            {
                let network = self.find_network(id).await?;
                let Some(path) = known_network_path(&self.managed_objects().await?, &network.path)
                else {
                    return Err(unsupported("auto-join requires a known iwd network"));
                };
                Proxy::new(&self.connection, SERVICE, path, KNOWN_NETWORK_INTERFACE)
                    .await
                    .map_err(|error| dbus_failure("update iwd auto-join", error))?
                    .set_property("AutoConnect", update.auto_join.unwrap())
                    .await
                    .map_err(|error| dbus_failure("update iwd auto-join", error))?;
            }
            _ => return Err(unsupported("that action is not available through iwd")),
        }
        Ok(acceptance(command.desired))
    }

    async fn cancel(&self, _operation_id: OperationId) -> Result<(), BackendFailure> {
        Ok(())
    }

    async fn diagnostics(&self) -> BackendDiagnostics {
        let probe = self.probe().await;
        let mut properties = BTreeMap::new();
        properties.insert("service".into(), SERVICE.into());
        properties.insert("epoch".into(), self.clock.epoch().to_string());
        BackendDiagnostics {
            backend: self.kind(),
            owner: probe.owner,
            version: None,
            properties,
            warnings: probe
                .detail
                .into_iter()
                .chain(["iwd exposes SSIDs as D-Bus strings, so invalid UTF-8 SSIDs cannot be represented byte-for-byte".into()])
                .collect(),
        }
    }
}

fn station_entries(objects: &ManagedObjects) -> Vec<(&OwnedObjectPath, &Properties)> {
    objects
        .iter()
        .filter_map(|(path, value)| {
            value
                .get(STATION_INTERFACE)
                .map(|properties| (path, properties))
        })
        .collect()
}

fn iwd_network(
    interface: &InterfaceId,
    station_state: &str,
    path: OwnedObjectPath,
    properties: &Properties,
    objects: &ManagedObjects,
    signal: i16,
) -> Option<IwdNetwork> {
    let name = string_property(properties, "Name")?;
    let security = map_security(&string_property(properties, "Type")?);
    let known = path_property(properties, "KnownNetwork");
    let auto_join = known
        .as_ref()
        .and_then(|path| objects.get(path))
        .and_then(|interfaces| interfaces.get(KNOWN_NETWORK_INTERFACE))
        .and_then(|properties| bool_property(properties, "AutoConnect"))
        .unwrap_or(known.is_some());
    Some(IwdNetwork {
        path,
        id: WifiNetworkId {
            interface: interface.clone(),
            ssid: Ssid(name.into_bytes()),
            security,
        },
        signal: signal_percent(signal),
        connected: bool_property(properties, "Connected").unwrap_or(false),
        saved: known.is_some(),
        auto_join,
        station_state: station_state.into(),
    })
}

fn known_network_path(
    objects: &ManagedObjects,
    network: &OwnedObjectPath,
) -> Option<OwnedObjectPath> {
    objects
        .get(network)
        .and_then(|interfaces| interfaces.get(NETWORK_INTERFACE))
        .and_then(|properties| path_property(properties, "KnownNetwork"))
}

fn string_property(properties: &Properties, name: &str) -> Option<String> {
    properties
        .get(name)
        .and_then(|value| <&str>::try_from(value).ok())
        .map(str::to_owned)
}

fn bool_property(properties: &Properties, name: &str) -> Option<bool> {
    properties
        .get(name)
        .and_then(|value| bool::try_from(value).ok())
}

fn path_property(properties: &Properties, name: &str) -> Option<OwnedObjectPath> {
    properties
        .get(name)
        .and_then(|value| <&zvariant::ObjectPath<'_>>::try_from(value).ok())
        .and_then(|path| OwnedObjectPath::try_from(path.as_str()).ok())
}

fn map_security(value: &str) -> WifiSecurity {
    match value {
        "open" => WifiSecurity::Open,
        "wep" => WifiSecurity::Wep,
        "psk" => WifiSecurity::Personal,
        "8021x" => WifiSecurity::Enterprise,
        value => WifiSecurity::Unknown(value.into()),
    }
}

fn map_station_state(value: &str) -> ConnectionState {
    match value {
        "connecting" => ConnectionState::Associating,
        "connected" | "roaming" => ConnectionState::Connected,
        "disconnecting" => ConnectionState::Disconnecting,
        _ => ConnectionState::Disconnected,
    }
}

fn signal_percent(milli_dbm: i16) -> u8 {
    let dbm = i32::from(milli_dbm) / 100;
    ((dbm + 100) * 2).clamp(0, 100) as u8
}

fn iwd_capabilities() -> CapabilityMap {
    [
        Capability::RadioToggle,
        Capability::Scan,
        Capability::HiddenNetwork,
        Capability::Enterprise,
        Capability::Forget,
        Capability::AutoJoin,
        Capability::Hotspot,
    ]
    .into_iter()
    .map(|capability| (capability, CapabilityState::Supported))
    .collect()
}

fn acceptance(desired: DesiredState) -> OperationAcceptance {
    OperationAcceptance {
        phase: OperationPhase::AwaitingConfirmation("waiting for iwd state".into()),
        deadline_ms: super::dbus::monotonic_ms()
            + Duration::from_secs(if desired == DesiredState::Connected {
                45
            } else {
                20
            })
            .as_millis() as u64,
    }
}

fn stopped() -> BackendFailure {
    BackendFailure {
        category: ErrorCategory::ServiceUnavailable,
        summary: "iwd backend stopped".into(),
        detail: "The backend was dropped while rebuilding state".into(),
        recovery: Vec::new(),
        retryable: true,
        raw_code: None,
    }
}

fn not_found(summary: String) -> BackendFailure {
    BackendFailure {
        category: ErrorCategory::NotFound,
        summary,
        detail: "iwd no longer reports the selected object".into(),
        recovery: vec!["Scan and retry if the network is still nearby".into()],
        retryable: true,
        raw_code: None,
    }
}

fn unsupported(detail: &str) -> BackendFailure {
    BackendFailure {
        category: ErrorCategory::Unsupported,
        summary: "This iwd operation is not supported".into(),
        detail: detail.into(),
        recovery: Vec::new(),
        retryable: false,
        raw_code: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iwd_signal_is_normalized_without_wrapping() {
        assert_eq!(signal_percent(-10_000), 0);
        assert_eq!(signal_percent(-7_500), 50);
        assert_eq!(signal_percent(-5_000), 100);
        assert_eq!(signal_percent(-2_000), 100);
    }

    #[test]
    fn iwd_security_values_are_not_collapsed() {
        assert_eq!(map_security("open"), WifiSecurity::Open);
        assert_eq!(map_security("psk"), WifiSecurity::Personal);
        assert_eq!(map_security("8021x"), WifiSecurity::Enterprise);
    }
}
