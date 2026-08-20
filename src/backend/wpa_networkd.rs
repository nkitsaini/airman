use std::{collections::BTreeMap, collections::HashMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use futures_util::{stream, StreamExt};
use tokio::sync::broadcast;
use zbus::{Connection, Proxy};
use zvariant::{OwnedObjectPath, OwnedValue, Value};

use super::{
    dbus::{dbus_failure, probe_service, spawn_signal_supervisor, ServiceClock, SnapshotFn},
    BackendAction, BackendCommand, BackendDiagnostics, BackendFailure, CapabilityMap,
    OperationAcceptance, ProbeResult, ProbeStatus, RadioBackend,
};
use crate::domain::{
    BackendEvent, BackendKind, BackendPayload, Capability, CapabilityState, ConnectionState,
    Connectivity, DesiredState, EntityId, ErrorCategory, HardwareAddress, InterfaceId, OperationId,
    OperationPhase, Ssid, WifiInterface, WifiNetwork, WifiNetworkId, WifiSecurity, WifiSnapshot,
};

const WPA_SERVICE: &str = "fi.w1.wpa_supplicant1";
const WPA_ROOT: &str = "/fi/w1/wpa_supplicant1";
const WPA_ROOT_INTERFACE: &str = "fi.w1.wpa_supplicant1";
const WPA_INTERFACE: &str = "fi.w1.wpa_supplicant1.Interface";
const WPA_BSS_INTERFACE: &str = "fi.w1.wpa_supplicant1.BSS";
const WPA_NETWORK_INTERFACE: &str = "fi.w1.wpa_supplicant1.Network";
const NETWORKD_SERVICE: &str = "org.freedesktop.network1";
const NETWORKD_PATH: &str = "/org/freedesktop/network1";
const NETWORKD_MANAGER: &str = "org.freedesktop.network1.Manager";
const NETWORKD_LINK: &str = "org.freedesktop.network1.Link";

#[derive(Debug, Clone)]
struct SupplicantInterface {
    path: OwnedObjectPath,
    id: InterfaceId,
    state: String,
    scanning: bool,
    current_bss: Option<OwnedObjectPath>,
    bss_paths: Vec<OwnedObjectPath>,
    network_paths: Vec<OwnedObjectPath>,
}

#[derive(Debug, Clone)]
struct SupplicantBss {
    path: OwnedObjectPath,
    ssid: Ssid,
    security: WifiSecurity,
    signal: u8,
    bssid: HardwareAddress,
}

#[derive(Debug, Clone)]
struct SupplicantProfile {
    path: OwnedObjectPath,
    ssid: Ssid,
    security: WifiSecurity,
    enabled: bool,
}

pub struct WpaNetworkdBackend {
    connection: Connection,
    interface_filter: Option<String>,
    clock: Arc<ServiceClock>,
    events: broadcast::Sender<BackendEvent>,
}

impl WpaNetworkdBackend {
    pub async fn new(connection: Connection, interface_filter: Option<String>) -> Arc<Self> {
        let (events, _) = broadcast::channel(256);
        let backend = Arc::new(Self {
            connection,
            interface_filter,
            clock: Arc::new(ServiceClock::new(BackendKind::WpaNetworkd)),
            events,
        });
        let weak = Arc::downgrade(&backend);
        let snapshot: SnapshotFn = Arc::new(move || {
            let weak = weak.clone();
            Box::pin(async move {
                let backend = weak.upgrade().ok_or_else(stopped)?;
                backend.snapshot_inner().await
            })
        });
        for service in [WPA_SERVICE, NETWORKD_SERVICE] {
            spawn_signal_supervisor(
                backend.connection.clone(),
                service,
                backend.clock.clone(),
                backend.events.clone(),
                snapshot.clone(),
            );
        }
        backend
    }

    async fn interfaces(&self) -> Result<Vec<SupplicantInterface>, BackendFailure> {
        let root = Proxy::new(&self.connection, WPA_SERVICE, WPA_ROOT, WPA_ROOT_INTERFACE)
            .await
            .map_err(|error| dbus_failure("inspect wpa_supplicant", error))?;
        let paths: Vec<OwnedObjectPath> = root
            .get_property("Interfaces")
            .await
            .map_err(|error| dbus_failure("enumerate wpa_supplicant interfaces", error))?;
        Ok(stream::iter(paths)
            .map(|path| self.read_interface(path))
            .buffer_unordered(8)
            .filter_map(async |result| result.ok().flatten())
            .collect()
            .await)
    }

    async fn read_interface(
        &self,
        path: OwnedObjectPath,
    ) -> Result<Option<SupplicantInterface>, BackendFailure> {
        let proxy = Proxy::new(&self.connection, WPA_SERVICE, path.clone(), WPA_INTERFACE)
            .await
            .map_err(|error| dbus_failure("inspect a wpa_supplicant interface", error))?;
        let name: String = proxy
            .get_property("Ifname")
            .await
            .map_err(|error| dbus_failure("read a wpa_supplicant interface name", error))?;
        if self
            .interface_filter
            .as_ref()
            .is_some_and(|filter| filter != &name)
        {
            return Ok(None);
        }
        let current_bss: OwnedObjectPath = proxy
            .get_property("CurrentBSS")
            .await
            .unwrap_or_else(|_| OwnedObjectPath::try_from("/").expect("valid root path"));
        Ok(Some(SupplicantInterface {
            path,
            id: InterfaceId(name),
            state: proxy
                .get_property("State")
                .await
                .unwrap_or_else(|_| "unknown".into()),
            scanning: proxy.get_property("Scanning").await.unwrap_or(false),
            current_bss: (current_bss.as_str() != "/").then_some(current_bss),
            bss_paths: proxy.get_property("BSSs").await.unwrap_or_default(),
            network_paths: proxy.get_property("Networks").await.unwrap_or_default(),
        }))
    }

    async fn snapshot_inner(&self) -> Result<BackendEvent, BackendFailure> {
        let wpa = probe_service(&self.connection, WPA_SERVICE, self.kind()).await;
        if wpa.status != ProbeStatus::Available {
            return Ok(self.clock.event(BackendPayload::Health {
                health: crate::domain::BackendHealth::Unavailable,
                detail: Some(
                    wpa.detail
                        .unwrap_or_else(|| "wpa_supplicant has no D-Bus owner".into()),
                ),
            }));
        }
        let networkd = probe_service(&self.connection, NETWORKD_SERVICE, self.kind()).await;
        if networkd.status != ProbeStatus::Available {
            return Err(BackendFailure {
                category: ErrorCategory::ServiceUnavailable,
                summary: "systemd-networkd is unavailable".into(),
                detail: networkd
                    .detail
                    .unwrap_or_else(|| "the networkd D-Bus service has no owner".into()),
                recovery: vec!["Start systemd-networkd or select a different Wi-Fi backend".into()],
                retryable: true,
                raw_code: Some("networkd-unavailable".into()),
            });
        }
        let interfaces = self.interfaces().await?;
        let now = super::dbus::monotonic_ms();
        let mut wifi_interfaces = Vec::new();
        let mut networks = BTreeMap::<WifiNetworkId, WifiNetwork>::new();

        for interface in &interfaces {
            let networkd = self.networkd_state(&interface.id).await;
            wifi_interfaces.push(WifiInterface {
                id: interface.id.clone(),
                backend: BackendKind::WpaNetworkd,
                powered: interface.state != "interface_disabled",
                scanning: interface.scanning,
                last_scan_ms: None,
                addresses: super::system::interface_addresses(&interface.id.0),
                capabilities: wpa_capabilities(),
            });
            let profiles = self.profiles(interface).await;
            let bsses = stream::iter(interface.bss_paths.clone())
                .map(|path| self.read_bss(path))
                .buffer_unordered(24)
                .filter_map(async |result| result.ok())
                .collect::<Vec<_>>()
                .await;
            for bss in bsses {
                if bss.ssid.0.is_empty() {
                    continue;
                }
                let id = WifiNetworkId {
                    interface: interface.id.clone(),
                    ssid: bss.ssid.clone(),
                    security: bss.security.clone(),
                };
                let active = interface.current_bss.as_ref() == Some(&bss.path);
                let profile = profiles
                    .iter()
                    .find(|profile| profile.ssid == id.ssid && profile.security == id.security);
                let entry = networks.entry(id.clone()).or_insert_with(|| WifiNetwork {
                    id: id.clone(),
                    display_name: id.ssid.display(),
                    signal: 0,
                    state: ConnectionState::Disconnected,
                    connectivity: Connectivity::None,
                    saved: profile.is_some(),
                    auto_join: profile.is_some_and(|profile| profile.enabled),
                    bss_count: 0,
                    active_bssid: None,
                    present: true,
                    last_seen_ms: now,
                });
                entry.bss_count += 1;
                entry.signal = entry.signal.max(bss.signal);
                if active {
                    entry.state = map_supplicant_state(&interface.state, networkd.as_ref());
                    entry.connectivity = map_networkd_connectivity(networkd.as_ref());
                    entry.active_bssid = Some(bss.bssid);
                }
            }
            for profile in profiles {
                let id = WifiNetworkId {
                    interface: interface.id.clone(),
                    ssid: profile.ssid,
                    security: profile.security,
                };
                networks.entry(id.clone()).or_insert(WifiNetwork {
                    display_name: id.ssid.display(),
                    id,
                    signal: 0,
                    state: ConnectionState::Disconnected,
                    connectivity: Connectivity::None,
                    saved: true,
                    auto_join: profile.enabled,
                    bss_count: 0,
                    active_bssid: None,
                    present: false,
                    last_seen_ms: now,
                });
            }
        }

        Ok(self.clock.event(BackendPayload::WifiSnapshot(WifiSnapshot {
            interfaces: wifi_interfaces,
            networks: networks.into_values().collect(),
        })))
    }

    async fn read_bss(&self, path: OwnedObjectPath) -> Result<SupplicantBss, BackendFailure> {
        let proxy = Proxy::new(
            &self.connection,
            WPA_SERVICE,
            path.clone(),
            WPA_BSS_INTERFACE,
        )
        .await
        .map_err(|error| dbus_failure("inspect a wpa_supplicant BSS", error))?;
        let privacy = proxy.get_property("Privacy").await.unwrap_or(false);
        let wpa: HashMap<String, OwnedValue> = proxy.get_property("WPA").await.unwrap_or_default();
        let rsn: HashMap<String, OwnedValue> = proxy.get_property("RSN").await.unwrap_or_default();
        let bssid: Vec<u8> = proxy.get_property("BSSID").await.unwrap_or_default();
        Ok(SupplicantBss {
            path,
            ssid: Ssid(
                proxy
                    .get_property("SSID")
                    .await
                    .map_err(|error| dbus_failure("read a wpa_supplicant SSID", error))?,
            ),
            security: bss_security(privacy, &wpa, &rsn),
            signal: dbm_percent(proxy.get_property("Signal").await.unwrap_or(-100)),
            bssid: HardwareAddress(format_mac(&bssid)),
        })
    }

    async fn profiles(&self, interface: &SupplicantInterface) -> Vec<SupplicantProfile> {
        stream::iter(interface.network_paths.clone())
            .map(|path| self.read_profile(path))
            .buffer_unordered(16)
            .filter_map(async |result| result.ok().flatten())
            .collect()
            .await
    }

    async fn read_profile(
        &self,
        path: OwnedObjectPath,
    ) -> Result<Option<SupplicantProfile>, BackendFailure> {
        let proxy = Proxy::new(
            &self.connection,
            WPA_SERVICE,
            path.clone(),
            WPA_NETWORK_INTERFACE,
        )
        .await
        .map_err(|error| dbus_failure("inspect a wpa_supplicant profile", error))?;
        let properties: HashMap<String, OwnedValue> = proxy
            .get_property("Properties")
            .await
            .map_err(|error| dbus_failure("inspect a wpa_supplicant profile", error))?;
        let Some(ssid) = bytes_or_string(&properties, "ssid") else {
            return Ok(None);
        };
        let key_management = string_value(&properties, "key_mgmt").unwrap_or_default();
        Ok(Some(SupplicantProfile {
            path,
            ssid: Ssid(ssid),
            security: profile_security(&key_management),
            enabled: proxy.get_property("Enabled").await.unwrap_or(true),
        }))
    }

    async fn networkd_state(&self, interface: &InterfaceId) -> Option<(String, String)> {
        let manager = Proxy::new(
            &self.connection,
            NETWORKD_SERVICE,
            NETWORKD_PATH,
            NETWORKD_MANAGER,
        )
        .await
        .ok()?;
        let (_, path): (i32, OwnedObjectPath) = manager
            .call("GetLinkByName", &(interface.0.as_str(),))
            .await
            .ok()?;
        let link = Proxy::new(&self.connection, NETWORKD_SERVICE, path, NETWORKD_LINK)
            .await
            .ok()?;
        Some((
            link.get_property("OperationalState").await.ok()?,
            link.get_property("SetupState").await.ok()?,
        ))
    }

    async fn interface(&self, id: &InterfaceId) -> Result<SupplicantInterface, BackendFailure> {
        self.interfaces()
            .await?
            .into_iter()
            .find(|interface| &interface.id == id)
            .ok_or_else(|| not_found(format!("wpa_supplicant no longer controls {}", id.0)))
    }

    async fn connect(
        &self,
        command: &BackendCommand,
        id: &WifiNetworkId,
    ) -> Result<(), BackendFailure> {
        let interface = self.interface(&id.interface).await?;
        let profiles = self.profiles(&interface).await;
        let proxy = Proxy::new(
            &self.connection,
            WPA_SERVICE,
            &interface.path,
            WPA_INTERFACE,
        )
        .await
        .map_err(|error| dbus_failure("connect with wpa_supplicant", error))?;
        let profile = if let Some(profile) = profiles
            .into_iter()
            .find(|profile| profile.ssid == id.ssid && profile.security == id.security)
        {
            profile.path
        } else {
            if id.security == WifiSecurity::Enterprise {
                return Err(unsupported(
                    "create the 802.1X profile explicitly before selecting it",
                ));
            }
            if id.security != WifiSecurity::Open && command.credential.is_none() {
                return Err(missing_credential(id));
            }
            let settings = supplicant_settings(
                id,
                command.credential.as_ref().map(|secret| secret.expose()),
            );
            proxy
                .call("AddNetwork", &(settings,))
                .await
                .map_err(|error| dbus_failure("create a wpa_supplicant profile", error))?
        };
        proxy
            .call("SelectNetwork", &(profile,))
            .await
            .map_err(|error| dbus_failure("select a wpa_supplicant profile", error))
    }
}

#[async_trait]
impl RadioBackend for WpaNetworkdBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::WpaNetworkd
    }

    async fn probe(&self) -> ProbeResult {
        let mut wpa = probe_service(&self.connection, WPA_SERVICE, self.kind()).await;
        let networkd = probe_service(&self.connection, NETWORKD_SERVICE, self.kind()).await;
        if wpa.status == ProbeStatus::Available && networkd.status != ProbeStatus::Available {
            wpa.status = ProbeStatus::NotRunning;
            wpa.detail = Some("wpa_supplicant is running, but systemd-networkd is not available to report and configure IP state".into());
        }
        wpa
    }

    fn subscribe(&self) -> broadcast::Receiver<BackendEvent> {
        self.events.subscribe()
    }

    async fn snapshot(&self) -> Result<BackendEvent, BackendFailure> {
        self.snapshot_inner().await
    }

    async fn capabilities(&self) -> CapabilityMap {
        wpa_capabilities()
    }

    async fn execute(
        &self,
        command: BackendCommand,
    ) -> Result<OperationAcceptance, BackendFailure> {
        match (&command.action, &command.target) {
            (BackendAction::Scan, EntityId::WifiInterface(id)) => {
                let interface = self.interface(id).await?;
                let settings = HashMap::from([("Type", Value::new("active"))]);
                Proxy::new(&self.connection, WPA_SERVICE, interface.path, WPA_INTERFACE)
                    .await
                    .map_err(|error| dbus_failure("scan with wpa_supplicant", error))?
                    .call::<_, _, ()>("Scan", &(settings,))
                    .await
                    .map_err(|error| dbus_failure("scan with wpa_supplicant", error))?;
            }
            (BackendAction::Connect, EntityId::Wifi(id)) => self.connect(&command, id).await?,
            (BackendAction::Disconnect, EntityId::Wifi(id)) => {
                let interface = self.interface(&id.interface).await?;
                Proxy::new(&self.connection, WPA_SERVICE, interface.path, WPA_INTERFACE)
                    .await
                    .map_err(|error| dbus_failure("disconnect with wpa_supplicant", error))?
                    .call::<_, _, ()>("Disconnect", &())
                    .await
                    .map_err(|error| dbus_failure("disconnect with wpa_supplicant", error))?;
            }
            (BackendAction::Forget, EntityId::Wifi(id)) => {
                let interface = self.interface(&id.interface).await?;
                let Some(profile) = self
                    .profiles(&interface)
                    .await
                    .into_iter()
                    .find(|profile| profile.ssid == id.ssid && profile.security == id.security)
                else {
                    return Ok(acceptance(command.desired));
                };
                Proxy::new(&self.connection, WPA_SERVICE, interface.path, WPA_INTERFACE)
                    .await
                    .map_err(|error| dbus_failure("forget a wpa_supplicant profile", error))?
                    .call::<_, _, ()>("RemoveNetwork", &(profile.path,))
                    .await
                    .map_err(|error| dbus_failure("forget a wpa_supplicant profile", error))?;
            }
            (BackendAction::UpdateProfile(update), EntityId::Wifi(id))
                if update.auto_join.is_some() =>
            {
                let interface = self.interface(&id.interface).await?;
                let profile = self
                    .profiles(&interface)
                    .await
                    .into_iter()
                    .find(|profile| profile.ssid == id.ssid && profile.security == id.security)
                    .ok_or_else(|| {
                        not_found(format!("{} has no saved profile", id.ssid.display()))
                    })?;
                Proxy::new(
                    &self.connection,
                    WPA_SERVICE,
                    profile.path,
                    WPA_NETWORK_INTERFACE,
                )
                .await
                .map_err(|error| dbus_failure("update wpa_supplicant auto-join", error))?
                .set_property("Enabled", update.auto_join.unwrap())
                .await
                .map_err(|error| dbus_failure("update wpa_supplicant auto-join", error))?;
            }
            (BackendAction::SetPowered(_), EntityId::WifiInterface(_)) => {
                return Err(unsupported(
                    "wpa_supplicant has no radio-power D-Bus API; use rfkill or a higher-level network manager",
                ));
            }
            _ => return Err(unsupported("that action does not apply to this interface")),
        }
        Ok(acceptance(command.desired))
    }

    async fn cancel(&self, _operation_id: OperationId) -> Result<(), BackendFailure> {
        Ok(())
    }

    async fn diagnostics(&self) -> BackendDiagnostics {
        let wpa = probe_service(&self.connection, WPA_SERVICE, self.kind()).await;
        let networkd = probe_service(&self.connection, NETWORKD_SERVICE, self.kind()).await;
        BackendDiagnostics {
            backend: self.kind(),
            owner: wpa.owner,
            version: None,
            properties: BTreeMap::from([
                ("supplicant_service".into(), WPA_SERVICE.into()),
                ("networkd_service".into(), NETWORKD_SERVICE.into()),
                ("networkd_status".into(), format!("{:?}", networkd.status)),
                ("epoch".into(), self.clock.epoch().to_string()),
            ]),
            warnings: wpa.detail.into_iter().chain(networkd.detail).collect(),
        }
    }
}

fn bss_security(
    privacy: bool,
    wpa: &HashMap<String, OwnedValue>,
    rsn: &HashMap<String, OwnedValue>,
) -> WifiSecurity {
    let enterprise = [wpa, rsn].iter().any(|properties| {
        properties
            .get("KeyMgmt")
            .and_then(|value| value.try_clone().ok())
            .and_then(|value| Vec::<String>::try_from(value).ok())
            .is_some_and(|values| {
                values
                    .iter()
                    .any(|value| value.to_lowercase().contains("eap"))
            })
    });
    if enterprise {
        WifiSecurity::Enterprise
    } else if !wpa.is_empty() || !rsn.is_empty() {
        WifiSecurity::Personal
    } else if privacy {
        WifiSecurity::Wep
    } else {
        WifiSecurity::Open
    }
}

fn profile_security(key_management: &str) -> WifiSecurity {
    let key_management = key_management.to_lowercase();
    if key_management.contains("eap") || key_management.contains("ieee8021x") {
        WifiSecurity::Enterprise
    } else if key_management.contains("psk") || key_management.contains("sae") {
        WifiSecurity::Personal
    } else if key_management == "none" || key_management.is_empty() {
        WifiSecurity::Open
    } else {
        WifiSecurity::Unknown(key_management)
    }
}

fn map_supplicant_state(value: &str, networkd: Option<&(String, String)>) -> ConnectionState {
    match value {
        "authenticating" | "4way_handshake" | "group_handshake" => ConnectionState::Authenticating,
        "associating" | "associated" => ConnectionState::Associating,
        "completed"
            if networkd.is_some_and(|(operational, setup)| {
                matches!(operational.as_str(), "routable" | "degraded") && setup == "configured"
            }) =>
        {
            ConnectionState::Connected
        }
        "completed" => ConnectionState::ObtainingAddress,
        _ => ConnectionState::Disconnected,
    }
}

fn map_networkd_connectivity(networkd: Option<&(String, String)>) -> Connectivity {
    match networkd.map(|(operational, _)| operational.as_str()) {
        // networkd reports reachability of routes, not a verified Internet
        // probe. Claiming Internet here would be stronger than the API says.
        Some("routable") => Connectivity::Limited,
        Some("degraded") | Some("carrier") => Connectivity::Local,
        Some("no-carrier") | Some("off") => Connectivity::None,
        _ => Connectivity::Unknown,
    }
}

fn bytes_or_string(properties: &HashMap<String, OwnedValue>, name: &str) -> Option<Vec<u8>> {
    let value = properties.get(name)?;
    value
        .try_clone()
        .ok()
        .and_then(|value| Vec::<u8>::try_from(value).ok())
        .or_else(|| {
            <&str>::try_from(value)
                .ok()
                .map(|value| value.as_bytes().to_vec())
        })
}

fn string_value(properties: &HashMap<String, OwnedValue>, name: &str) -> Option<String> {
    properties
        .get(name)
        .and_then(|value| <&str>::try_from(value).ok())
        .map(str::to_owned)
}

fn supplicant_settings<'a>(
    id: &'a WifiNetworkId,
    credential: Option<&'a str>,
) -> HashMap<String, Value<'a>> {
    let mut settings = HashMap::from([("ssid".into(), Value::new(id.ssid.0.clone()))]);
    match id.security {
        WifiSecurity::Open => {
            settings.insert("key_mgmt".into(), Value::new("NONE"));
        }
        WifiSecurity::Wep => {
            settings.insert("key_mgmt".into(), Value::new("NONE"));
            if let Some(credential) = credential {
                settings.insert("wep_key0".into(), Value::new(credential));
            }
        }
        _ => {
            settings.insert("key_mgmt".into(), Value::new("WPA-PSK SAE"));
            if let Some(credential) = credential {
                settings.insert("psk".into(), Value::new(credential));
            }
        }
    }
    settings
}

fn format_mac(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

fn dbm_percent(dbm: i16) -> u8 {
    ((i32::from(dbm) + 100) * 2).clamp(0, 100) as u8
}

fn wpa_capabilities() -> CapabilityMap {
    let mut capabilities = [
        Capability::Scan,
        Capability::Enterprise,
        Capability::Forget,
        Capability::AutoJoin,
    ]
    .into_iter()
    .map(|capability| (capability, CapabilityState::Supported))
    .collect::<CapabilityMap>();
    capabilities.insert(Capability::RadioToggle, CapabilityState::Unsupported);
    capabilities
}

fn acceptance(desired: DesiredState) -> OperationAcceptance {
    OperationAcceptance {
        phase: OperationPhase::AwaitingConfirmation(
            "waiting for association and networkd address state".into(),
        ),
        deadline_ms: super::dbus::monotonic_ms()
            + Duration::from_secs(if desired == DesiredState::Connected {
                50
            } else {
                20
            })
            .as_millis() as u64,
    }
}

fn missing_credential(id: &WifiNetworkId) -> BackendFailure {
    BackendFailure {
        category: ErrorCategory::MissingSecrets,
        summary: format!("{} needs a Wi-Fi credential", id.ssid.display()),
        detail: "wpa_supplicant has no matching saved network block".into(),
        recovery: vec!["Retry and enter the network credential".into()],
        retryable: true,
        raw_code: Some("missing-credential".into()),
    }
}

fn stopped() -> BackendFailure {
    BackendFailure {
        category: ErrorCategory::ServiceUnavailable,
        summary: "wpa_supplicant backend stopped".into(),
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
        detail: "The selected interface disappeared".into(),
        recovery: vec!["Check wpa_supplicant interface configuration".into()],
        retryable: true,
        raw_code: None,
    }
}

fn unsupported(detail: &str) -> BackendFailure {
    BackendFailure {
        category: ErrorCategory::Unsupported,
        summary: "This wpa_supplicant operation is not supported".into(),
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
    fn completed_association_waits_for_networkd_configuration() {
        assert_eq!(
            map_supplicant_state("completed", Some(&("carrier".into(), "configuring".into()))),
            ConnectionState::ObtainingAddress
        );
        assert_eq!(
            map_supplicant_state("completed", Some(&("routable".into(), "configured".into()))),
            ConnectionState::Connected
        );
    }

    #[test]
    fn signal_normalization_clamps_extremes() {
        assert_eq!(dbm_percent(-120), 0);
        assert_eq!(dbm_percent(-75), 50);
        assert_eq!(dbm_percent(-30), 100);
    }
}
