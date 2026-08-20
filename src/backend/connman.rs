use std::{collections::BTreeMap, collections::HashMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use tokio::sync::{broadcast, Mutex};
use zbus::{interface, Connection, Proxy};
use zvariant::{OwnedObjectPath, OwnedValue, Str, Value};

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

const SERVICE: &str = "net.connman";
const MANAGER_INTERFACE: &str = "net.connman.Manager";
const TECHNOLOGY_INTERFACE: &str = "net.connman.Technology";
const SERVICE_INTERFACE: &str = "net.connman.Service";
const AGENT_PATH: &str = "/dev/airman/connman_agent";

type Properties = HashMap<String, OwnedValue>;
type ObjectList = Vec<(OwnedObjectPath, Properties)>;

struct ConnManCredentialAgent {
    credential: Mutex<Option<Secret>>,
}

#[interface(name = "net.connman.Agent")]
impl ConnManCredentialAgent {
    async fn release(&self) {}

    async fn report_error(&self, service: OwnedObjectPath, error: String) {
        tracing::debug!(service = %service, %error, "ConnMan reported an agent error");
    }

    async fn request_input(
        &self,
        _service: OwnedObjectPath,
        fields: HashMap<String, HashMap<String, OwnedValue>>,
    ) -> zbus::fdo::Result<HashMap<String, OwnedValue>> {
        let mut response = HashMap::new();
        if fields.contains_key("Passphrase") {
            let secret = self
                .credential
                .lock()
                .await
                .take()
                .ok_or_else(|| zbus::fdo::Error::Failed("no credential is pending".into()))?;
            response.insert(
                "Passphrase".into(),
                OwnedValue::from(Str::from(secret.expose())),
            );
        }
        Ok(response)
    }

    async fn cancel(&self) {}
}

#[derive(Debug, Clone)]
struct Technology {
    path: OwnedObjectPath,
    powered: bool,
    interfaces: Vec<InterfaceId>,
}

#[derive(Debug, Clone)]
struct ServiceRecord {
    path: OwnedObjectPath,
    id: WifiNetworkId,
    state: String,
    error: Option<String>,
    strength: u8,
    favorite: bool,
    auto_connect: bool,
}

pub struct ConnManBackend {
    connection: Connection,
    interface_filter: Option<String>,
    clock: Arc<ServiceClock>,
    events: broadcast::Sender<BackendEvent>,
    agent_lock: Mutex<()>,
}

impl ConnManBackend {
    pub async fn new(connection: Connection, interface_filter: Option<String>) -> Arc<Self> {
        let (events, _) = broadcast::channel(256);
        let backend = Arc::new(Self {
            connection,
            interface_filter,
            clock: Arc::new(ServiceClock::new(BackendKind::ConnMan)),
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

    async fn manager(&self) -> Result<Proxy<'_>, BackendFailure> {
        Proxy::new(&self.connection, SERVICE, "/", MANAGER_INTERFACE)
            .await
            .map_err(|error| dbus_failure("inspect ConnMan", error))
    }

    async fn technologies(&self) -> Result<Vec<Technology>, BackendFailure> {
        let list: ObjectList = self
            .manager()
            .await?
            .call("GetTechnologies", &())
            .await
            .map_err(|error| dbus_failure("enumerate ConnMan technologies", error))?;
        Ok(list
            .into_iter()
            .filter_map(|(path, properties)| {
                (string_property(&properties, "Type").as_deref() == Some("wifi")).then(|| {
                    let interfaces = string_array_property(&properties, "Interfaces")
                        .unwrap_or_default()
                        .into_iter()
                        .map(InterfaceId)
                        .filter(|id| {
                            self.interface_filter
                                .as_ref()
                                .is_none_or(|filter| filter == &id.0)
                        })
                        .collect();
                    Technology {
                        path,
                        powered: bool_property(&properties, "Powered").unwrap_or(false),
                        interfaces,
                    }
                })
            })
            .collect())
    }

    async fn services(
        &self,
        interface: &InterfaceId,
    ) -> Result<Vec<ServiceRecord>, BackendFailure> {
        let list: ObjectList = self
            .manager()
            .await?
            .call("GetServices", &())
            .await
            .map_err(|error| dbus_failure("enumerate ConnMan services", error))?;
        Ok(list
            .into_iter()
            .filter_map(|(path, properties)| service_record(interface, path, &properties))
            .collect())
    }

    async fn snapshot_inner(&self) -> Result<BackendEvent, BackendFailure> {
        let technologies = self.technologies().await?;
        let now = super::dbus::monotonic_ms();
        let mut interfaces = Vec::new();
        let mut networks = Vec::new();
        for technology in technologies {
            for interface in technology.interfaces {
                interfaces.push(WifiInterface {
                    id: interface.clone(),
                    backend: BackendKind::ConnMan,
                    powered: technology.powered,
                    scanning: false,
                    last_scan_ms: None,
                    addresses: super::system::interface_addresses(&interface.0),
                    capabilities: connman_capabilities(),
                });
                for service in self.services(&interface).await? {
                    let failed = service.state == "failure";
                    networks.push(WifiNetwork {
                        display_name: service.id.ssid.display(),
                        id: service.id,
                        signal: service.strength,
                        state: map_service_state(&service.state),
                        connectivity: map_connectivity(&service.state),
                        saved: service.favorite,
                        auto_join: service.auto_connect,
                        bss_count: 1,
                        active_bssid: None,
                        present: service.strength > 0
                            || matches!(
                                service.state.as_str(),
                                "association" | "configuration" | "ready" | "online"
                            ),
                        last_seen_ms: now,
                    });
                    if failed {
                        tracing::debug!(error = ?service.error, "ConnMan service is in failure state");
                    }
                }
            }
        }
        Ok(self.clock.event(BackendPayload::WifiSnapshot(WifiSnapshot {
            interfaces,
            networks,
        })))
    }

    async fn technology(&self, id: &InterfaceId) -> Result<Technology, BackendFailure> {
        self.technologies()
            .await?
            .into_iter()
            .find(|technology| technology.interfaces.contains(id))
            .ok_or_else(|| not_found(format!("ConnMan no longer reports {}", id.0)))
    }

    async fn service(&self, id: &WifiNetworkId) -> Result<ServiceRecord, BackendFailure> {
        self.services(&id.interface)
            .await?
            .into_iter()
            .find(|service| service.id == *id)
            .ok_or_else(|| not_found(format!("{} is no longer available", id.ssid.display())))
    }

    async fn connect(
        &self,
        service: &ServiceRecord,
        credential: Option<Secret>,
    ) -> Result<(), BackendFailure> {
        if service.favorite || service.id.security == WifiSecurity::Open {
            return self.connect_path(&service.path).await;
        }
        if service.id.security == WifiSecurity::Enterprise {
            return Err(unsupported(
                "provision an enterprise ConnMan service before selecting it",
            ));
        }
        let credential = credential.ok_or_else(|| BackendFailure {
            category: ErrorCategory::MissingSecrets,
            summary: format!("{} needs a Wi-Fi credential", service.id.ssid.display()),
            detail: "ConnMan has no saved credential for this service".into(),
            recovery: vec!["Retry and enter the network credential".into()],
            retryable: true,
            raw_code: Some("missing-credential".into()),
        })?;
        let _guard = self.agent_lock.lock().await;
        let path = OwnedObjectPath::try_from(AGENT_PATH)
            .map_err(|error| dbus_failure("prepare the ConnMan credential agent", error))?;
        self.connection
            .object_server()
            .at(
                path.clone(),
                ConnManCredentialAgent {
                    credential: Mutex::new(Some(credential)),
                },
            )
            .await
            .map_err(|error| dbus_failure("prepare the ConnMan credential agent", error))?;
        let manager = self.manager().await?;
        manager
            .call::<_, _, ()>("RegisterAgent", &(path.clone(),))
            .await
            .map_err(|error| dbus_failure("register the ConnMan credential agent", error))?;
        let result = self.connect_path(&service.path).await;
        let _ = manager
            .call::<_, _, ()>("UnregisterAgent", &(path.clone(),))
            .await;
        let _ = self
            .connection
            .object_server()
            .remove::<ConnManCredentialAgent, _>(path)
            .await;
        result
    }

    async fn connect_path(&self, path: &OwnedObjectPath) -> Result<(), BackendFailure> {
        Proxy::new(&self.connection, SERVICE, path, SERVICE_INTERFACE)
            .await
            .map_err(|error| dbus_failure("connect with ConnMan", error))?
            .call("Connect", &())
            .await
            .map_err(|error| dbus_failure("connect with ConnMan", error))
    }
}

#[async_trait]
impl RadioBackend for ConnManBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::ConnMan
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
        connman_capabilities()
    }

    async fn execute(
        &self,
        command: BackendCommand,
    ) -> Result<OperationAcceptance, BackendFailure> {
        match (&command.action, &command.target) {
            (BackendAction::Scan, EntityId::WifiInterface(id)) => {
                let technology = self.technology(id).await?;
                Proxy::new(
                    &self.connection,
                    SERVICE,
                    technology.path,
                    TECHNOLOGY_INTERFACE,
                )
                .await
                .map_err(|error| dbus_failure("scan with ConnMan", error))?
                .call::<_, _, ()>("Scan", &())
                .await
                .map_err(|error| dbus_failure("scan with ConnMan", error))?;
            }
            (BackendAction::Connect, EntityId::Wifi(id)) => {
                let service = self.service(id).await?;
                self.connect(&service, command.credential).await?;
            }
            (BackendAction::Disconnect, EntityId::Wifi(id)) => {
                let service = self.service(id).await?;
                Proxy::new(&self.connection, SERVICE, service.path, SERVICE_INTERFACE)
                    .await
                    .map_err(|error| dbus_failure("disconnect with ConnMan", error))?
                    .call::<_, _, ()>("Disconnect", &())
                    .await
                    .map_err(|error| dbus_failure("disconnect with ConnMan", error))?;
            }
            (BackendAction::SetPowered(powered), EntityId::WifiInterface(id)) => {
                let technology = self.technology(id).await?;
                Proxy::new(
                    &self.connection,
                    SERVICE,
                    technology.path,
                    TECHNOLOGY_INTERFACE,
                )
                .await
                .map_err(|error| dbus_failure("change the ConnMan radio state", error))?
                .call::<_, _, ()>("SetProperty", &("Powered", Value::new(*powered)))
                .await
                .map_err(|error| dbus_failure("change the ConnMan radio state", error))?;
            }
            (BackendAction::Forget, EntityId::Wifi(id)) => {
                let service = self.service(id).await?;
                Proxy::new(&self.connection, SERVICE, service.path, SERVICE_INTERFACE)
                    .await
                    .map_err(|error| dbus_failure("forget a ConnMan service", error))?
                    .call::<_, _, ()>("Remove", &())
                    .await
                    .map_err(|error| dbus_failure("forget a ConnMan service", error))?;
            }
            (BackendAction::UpdateProfile(update), EntityId::Wifi(id))
                if update.auto_join.is_some() =>
            {
                let service = self.service(id).await?;
                Proxy::new(&self.connection, SERVICE, service.path, SERVICE_INTERFACE)
                    .await
                    .map_err(|error| dbus_failure("update ConnMan auto-connect", error))?
                    .call::<_, _, ()>(
                        "SetProperty",
                        &("AutoConnect", Value::new(update.auto_join.unwrap())),
                    )
                    .await
                    .map_err(|error| dbus_failure("update ConnMan auto-connect", error))?;
            }
            _ => return Err(unsupported("that action does not apply to this service")),
        }
        Ok(acceptance(command.desired))
    }

    async fn cancel(&self, _operation_id: OperationId) -> Result<(), BackendFailure> {
        Ok(())
    }

    async fn diagnostics(&self) -> BackendDiagnostics {
        let probe = self.probe().await;
        BackendDiagnostics {
            backend: self.kind(),
            owner: probe.owner,
            version: None,
            properties: BTreeMap::from([
                ("service".into(), SERVICE.into()),
                ("epoch".into(), self.clock.epoch().to_string()),
            ]),
            warnings: probe.detail.into_iter().collect(),
        }
    }
}

fn service_record(
    interface: &InterfaceId,
    path: OwnedObjectPath,
    properties: &Properties,
) -> Option<ServiceRecord> {
    if string_property(properties, "Type").as_deref() != Some("wifi") {
        return None;
    }
    let security_values = string_array_property(properties, "Security").unwrap_or_default();
    let security = connman_security(&security_values);
    let ssid = ssid_from_service_path(&path).unwrap_or_else(|| {
        Ssid(
            string_property(properties, "Name")
                .unwrap_or_else(|| "<hidden>".into())
                .into_bytes(),
        )
    });
    Some(ServiceRecord {
        path,
        id: WifiNetworkId {
            interface: interface.clone(),
            ssid,
            security,
        },
        state: string_property(properties, "State").unwrap_or_else(|| "idle".into()),
        error: string_property(properties, "Error"),
        strength: u8_property(properties, "Strength").unwrap_or(0),
        favorite: bool_property(properties, "Favorite").unwrap_or(false),
        auto_connect: bool_property(properties, "AutoConnect").unwrap_or(false),
    })
}

fn ssid_from_service_path(path: &OwnedObjectPath) -> Option<Ssid> {
    let component = path.rsplit('/').next()?;
    let mut pieces = component.split('_');
    (pieces.next()? == "wifi").then_some(())?;
    pieces.next()?;
    let encoded = pieces.next()?;
    if encoded.len() % 2 != 0 || !encoded.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let bytes = encoded
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            std::str::from_utf8(pair)
                .ok()
                .and_then(|value| u8::from_str_radix(value, 16).ok())
        })
        .collect::<Option<Vec<_>>>()?;
    Some(Ssid(bytes))
}

fn connman_security(values: &[String]) -> WifiSecurity {
    if values.iter().any(|value| value == "ieee8021x") {
        WifiSecurity::Enterprise
    } else if values
        .iter()
        .any(|value| matches!(value.as_str(), "psk" | "wps" | "wps_advertising"))
    {
        WifiSecurity::Personal
    } else if values.iter().any(|value| value == "wep") {
        WifiSecurity::Wep
    } else if values.iter().any(|value| value == "none") {
        WifiSecurity::Open
    } else {
        WifiSecurity::Unknown(values.join(","))
    }
}

fn map_service_state(value: &str) -> ConnectionState {
    match value {
        "association" => ConnectionState::Associating,
        "configuration" => ConnectionState::ObtainingAddress,
        "ready" | "online" => ConnectionState::Connected,
        "disconnect" => ConnectionState::Disconnecting,
        "failure" => ConnectionState::Failed,
        _ => ConnectionState::Disconnected,
    }
}

fn map_connectivity(value: &str) -> Connectivity {
    match value {
        "online" => Connectivity::Internet,
        "ready" => Connectivity::Local,
        "idle" | "failure" => Connectivity::None,
        _ => Connectivity::Unknown,
    }
}

fn string_property(properties: &Properties, name: &str) -> Option<String> {
    properties
        .get(name)
        .and_then(|value| <&str>::try_from(value).ok())
        .map(str::to_owned)
}

fn string_array_property(properties: &Properties, name: &str) -> Option<Vec<String>> {
    properties
        .get(name)
        .and_then(|value| value.try_clone().ok())
        .and_then(|value| Vec::<String>::try_from(value).ok())
}

fn bool_property(properties: &Properties, name: &str) -> Option<bool> {
    properties
        .get(name)
        .and_then(|value| bool::try_from(value).ok())
}

fn u8_property(properties: &Properties, name: &str) -> Option<u8> {
    properties
        .get(name)
        .and_then(|value| u8::try_from(value).ok())
}

fn connman_capabilities() -> CapabilityMap {
    [
        Capability::RadioToggle,
        Capability::Scan,
        Capability::HiddenNetwork,
        Capability::Enterprise,
        Capability::Forget,
        Capability::AutoJoin,
        Capability::Priority,
        Capability::IpConfiguration,
        Capability::DnsConfiguration,
        Capability::ProxyConfiguration,
        Capability::Hotspot,
    ]
    .into_iter()
    .map(|capability| (capability, CapabilityState::Supported))
    .collect()
}

fn acceptance(desired: DesiredState) -> OperationAcceptance {
    OperationAcceptance {
        phase: OperationPhase::AwaitingConfirmation("waiting for ConnMan state".into()),
        deadline_ms: super::dbus::monotonic_ms()
            + Duration::from_secs(if desired == DesiredState::Connected {
                50
            } else {
                20
            })
            .as_millis() as u64,
    }
}

fn stopped() -> BackendFailure {
    BackendFailure {
        category: ErrorCategory::ServiceUnavailable,
        summary: "ConnMan backend stopped".into(),
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
        detail: "ConnMan no longer reports the selected object".into(),
        recovery: vec!["Scan and retry if the network is nearby".into()],
        retryable: true,
        raw_code: None,
    }
}

fn unsupported(detail: &str) -> BackendFailure {
    BackendFailure {
        category: ErrorCategory::Unsupported,
        summary: "This ConnMan operation is not supported".into(),
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
    fn connman_path_recovers_raw_ssid_bytes() {
        let path =
            OwnedObjectPath::try_from("/net/connman/service/wifi_001122334455_6100ff_managed_psk")
                .unwrap();
        assert_eq!(
            ssid_from_service_path(&path),
            Some(Ssid(vec![b'a', 0, 0xff]))
        );
    }

    #[test]
    fn service_states_distinguish_ip_configuration_and_online() {
        assert_eq!(
            map_service_state("configuration"),
            ConnectionState::ObtainingAddress
        );
        assert_eq!(map_service_state("online"), ConnectionState::Connected);
        assert_eq!(map_connectivity("online"), Connectivity::Internet);
    }
}
