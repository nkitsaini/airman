use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use futures_util::{stream, StreamExt};
use tokio::sync::broadcast;
use uuid::Uuid;
use zbus::{proxy, Connection};
use zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

use super::{
    dbus::{dbus_failure, probe_service, spawn_signal_supervisor, ServiceClock, SnapshotFn},
    BackendAction, BackendCommand, BackendDiagnostics, BackendFailure, CapabilityMap,
    OperationAcceptance, ProbeResult, RadioBackend,
};
use crate::domain::{
    BackendEvent, BackendKind, BackendPayload, Capability, CapabilityState, ConnectionState,
    Connectivity, DesiredState, EntityId, ErrorCategory, HardwareAddress, InterfaceId, OperationId,
    OperationPhase, Ssid, WifiInterface, WifiNetwork, WifiNetworkId, WifiSecurity, WifiSnapshot,
};

const SERVICE: &str = "org.freedesktop.NetworkManager";
const NULL_PATH: &str = "/";
const DEVICE_TYPE_WIFI: u32 = 2;

#[proxy(
    interface = "org.freedesktop.NetworkManager",
    default_service = "org.freedesktop.NetworkManager",
    default_path = "/org/freedesktop/NetworkManager"
)]
trait NetworkManager {
    fn get_devices(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
    fn activate_connection(
        &self,
        connection: &ObjectPath<'_>,
        device: &ObjectPath<'_>,
        specific_object: &ObjectPath<'_>,
    ) -> zbus::Result<OwnedObjectPath>;
    fn add_and_activate_connection(
        &self,
        connection: &HashMap<String, HashMap<String, Value<'_>>>,
        device: &ObjectPath<'_>,
        specific_object: &ObjectPath<'_>,
    ) -> zbus::Result<(OwnedObjectPath, OwnedObjectPath)>;

    #[zbus(property)]
    fn wireless_enabled(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn set_wireless_enabled(&self, enabled: bool) -> zbus::Result<()>;
    #[zbus(property)]
    fn connectivity(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn version(&self) -> zbus::Result<String>;
}

#[proxy(
    interface = "org.freedesktop.NetworkManager.Device",
    default_service = "org.freedesktop.NetworkManager"
)]
trait Device {
    fn disconnect(&self) -> zbus::Result<()>;

    #[zbus(property)]
    fn interface(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn device_type(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn state(&self) -> zbus::Result<u32>;
}

#[proxy(
    interface = "org.freedesktop.NetworkManager.Device.Wireless",
    default_service = "org.freedesktop.NetworkManager"
)]
trait Wireless {
    fn request_scan(&self, options: &HashMap<String, Value<'_>>) -> zbus::Result<()>;
    fn get_all_access_points(&self) -> zbus::Result<Vec<OwnedObjectPath>>;

    #[zbus(property)]
    fn active_access_point(&self) -> zbus::Result<OwnedObjectPath>;
    #[zbus(property)]
    fn last_scan(&self) -> zbus::Result<i64>;
}

#[proxy(
    interface = "org.freedesktop.NetworkManager.AccessPoint",
    default_service = "org.freedesktop.NetworkManager"
)]
trait AccessPoint {
    #[zbus(property)]
    fn ssid(&self) -> zbus::Result<Vec<u8>>;
    #[zbus(property)]
    fn strength(&self) -> zbus::Result<u8>;
    #[zbus(property)]
    fn flags(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn wpa_flags(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn rsn_flags(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn hw_address(&self) -> zbus::Result<String>;
}

#[proxy(
    interface = "org.freedesktop.NetworkManager.Settings",
    default_service = "org.freedesktop.NetworkManager",
    default_path = "/org/freedesktop/NetworkManager/Settings"
)]
trait Settings {
    fn list_connections(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
}

#[proxy(
    interface = "org.freedesktop.NetworkManager.Settings.Connection",
    default_service = "org.freedesktop.NetworkManager"
)]
trait SettingsConnection {
    fn get_settings(&self) -> zbus::Result<HashMap<String, HashMap<String, OwnedValue>>>;
    fn get_secrets(
        &self,
        setting_name: &str,
    ) -> zbus::Result<HashMap<String, HashMap<String, OwnedValue>>>;
    fn update(&self, properties: HashMap<String, HashMap<String, OwnedValue>>) -> zbus::Result<()>;
    fn delete(&self) -> zbus::Result<()>;
}

#[derive(Debug, Clone)]
struct DeviceRecord {
    path: OwnedObjectPath,
    interface: InterfaceId,
    state: u32,
    powered: bool,
    active_ap: Option<OwnedObjectPath>,
    last_scan: Option<u64>,
}

#[derive(Debug, Clone)]
struct AccessPointRecord {
    path: OwnedObjectPath,
    ssid: Ssid,
    security: WifiSecurity,
    strength: u8,
    bssid: HardwareAddress,
}

#[derive(Debug, Clone)]
struct SavedProfile {
    path: OwnedObjectPath,
    ssid: Ssid,
    security: WifiSecurity,
    auto_join: bool,
}

pub struct NetworkManagerBackend {
    connection: Connection,
    interface_filter: Option<String>,
    clock: Arc<ServiceClock>,
    events: broadcast::Sender<BackendEvent>,
}

impl NetworkManagerBackend {
    pub async fn new(connection: Connection, interface_filter: Option<String>) -> Arc<Self> {
        let (events, _) = broadcast::channel(256);
        let backend = Arc::new(Self {
            connection,
            interface_filter,
            clock: Arc::new(ServiceClock::new(BackendKind::NetworkManager)),
            events,
        });
        let weak = Arc::downgrade(&backend);
        let snapshot: SnapshotFn = Arc::new(move || {
            let weak = weak.clone();
            Box::pin(async move {
                let backend = weak.upgrade().ok_or_else(|| BackendFailure {
                    category: ErrorCategory::ServiceUnavailable,
                    summary: "NetworkManager backend stopped".into(),
                    detail: "The backend was dropped while a snapshot was in progress".into(),
                    recovery: Vec::new(),
                    retryable: true,
                    raw_code: None,
                })?;
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

    async fn snapshot_inner(&self) -> Result<BackendEvent, BackendFailure> {
        let manager = NetworkManagerProxy::new(&self.connection)
            .await
            .map_err(|error| dbus_failure("read NetworkManager state", error))?;
        let powered = manager.wireless_enabled().await.unwrap_or(false);
        let connectivity = map_connectivity(manager.connectivity().await.unwrap_or(0));
        let device_paths = manager
            .get_devices()
            .await
            .map_err(|error| dbus_failure("enumerate Wi-Fi devices", error))?;

        let devices = stream::iter(device_paths)
            .map(|path| self.read_device(path, powered))
            .buffer_unordered(8)
            .filter_map(async |result| match result {
                Ok(Some(device)) => Some(device),
                Ok(None) => None,
                Err(error) => {
                    tracing::debug!(%error, "ignoring a device that changed during snapshot");
                    None
                }
            })
            .collect::<Vec<_>>()
            .await;

        let saved = self.saved_profiles().await.unwrap_or_else(|error| {
            tracing::warn!(%error, "could not read NetworkManager saved profiles");
            Vec::new()
        });
        let mut networks = BTreeMap::<WifiNetworkId, WifiNetwork>::new();
        let now = super::dbus::monotonic_ms();

        for device in &devices {
            let access_points = self.access_points(&device.path).await.unwrap_or_else(|error| {
                tracing::debug!(interface = %device.interface.0, %error, "access point list changed while reading");
                Vec::new()
            });
            for access_point in access_points {
                if access_point.ssid.0.is_empty() {
                    continue;
                }
                let id = WifiNetworkId {
                    interface: device.interface.clone(),
                    ssid: access_point.ssid.clone(),
                    security: access_point.security.clone(),
                };
                let is_active = device
                    .active_ap
                    .as_ref()
                    .is_some_and(|path| path == &access_point.path);
                let profile = saved
                    .iter()
                    .find(|profile| profile.ssid == id.ssid && profile.security == id.security);
                let entry = networks.entry(id.clone()).or_insert_with(|| WifiNetwork {
                    id: id.clone(),
                    display_name: id.ssid.display(),
                    signal: 0,
                    state: ConnectionState::Disconnected,
                    connectivity: Connectivity::Unknown,
                    saved: profile.is_some(),
                    auto_join: profile.is_some_and(|profile| profile.auto_join),
                    bss_count: 0,
                    active_bssid: None,
                    present: true,
                    last_seen_ms: now,
                });
                entry.bss_count += 1;
                if is_active || access_point.strength > entry.signal {
                    entry.signal = access_point.strength;
                }
                if is_active {
                    entry.state = map_device_state(device.state);
                    entry.connectivity = connectivity;
                    entry.active_bssid = Some(access_point.bssid);
                }
            }

            let missing_profiles = saved
                .iter()
                .filter(|profile| {
                    !networks.keys().any(|id| {
                        id.interface == device.interface
                            && id.ssid == profile.ssid
                            && id.security == profile.security
                    })
                })
                .cloned()
                .collect::<Vec<_>>();
            for profile in missing_profiles {
                let id = WifiNetworkId {
                    interface: device.interface.clone(),
                    ssid: profile.ssid.clone(),
                    security: profile.security.clone(),
                };
                networks.insert(
                    id.clone(),
                    WifiNetwork {
                        display_name: id.ssid.display(),
                        id,
                        signal: 0,
                        state: ConnectionState::Disconnected,
                        connectivity: Connectivity::None,
                        saved: true,
                        auto_join: profile.auto_join,
                        bss_count: 0,
                        active_bssid: None,
                        present: false,
                        last_seen_ms: now,
                    },
                );
            }
        }

        let capabilities = wifi_capabilities();
        Ok(self.clock.event(BackendPayload::WifiSnapshot(WifiSnapshot {
            interfaces: devices
                .into_iter()
                .map(|device| {
                    let addresses = super::system::interface_addresses(&device.interface.0);
                    WifiInterface {
                        id: device.interface,
                        backend: BackendKind::NetworkManager,
                        powered: device.powered,
                        scanning: false,
                        last_scan_ms: device.last_scan,
                        addresses,
                        capabilities: capabilities.clone(),
                    }
                })
                .collect(),
            networks: networks.into_values().collect(),
        })))
    }

    async fn read_device(
        &self,
        path: OwnedObjectPath,
        powered: bool,
    ) -> Result<Option<DeviceRecord>, BackendFailure> {
        let device = DeviceProxy::builder(&self.connection)
            .path(path.clone())
            .map_err(|error| dbus_failure("inspect a Wi-Fi device", error))?
            .build()
            .await
            .map_err(|error| dbus_failure("inspect a Wi-Fi device", error))?;
        if device.device_type().await.unwrap_or(0) != DEVICE_TYPE_WIFI {
            return Ok(None);
        }
        let interface = device
            .interface()
            .await
            .map_err(|error| dbus_failure("read a Wi-Fi interface name", error))?;
        if self
            .interface_filter
            .as_ref()
            .is_some_and(|selected| selected != &interface)
        {
            return Ok(None);
        }
        let wireless = WirelessProxy::builder(&self.connection)
            .path(path.clone())
            .map_err(|error| dbus_failure("inspect a Wi-Fi device", error))?
            .build()
            .await
            .map_err(|error| dbus_failure("inspect a Wi-Fi device", error))?;
        let active_ap = wireless
            .active_access_point()
            .await
            .ok()
            .filter(|path| path.as_str() != NULL_PATH);
        let last_scan = wireless
            .last_scan()
            .await
            .ok()
            .and_then(|value| u64::try_from(value).ok());
        Ok(Some(DeviceRecord {
            path,
            interface: InterfaceId(interface),
            state: device.state().await.unwrap_or(0),
            powered,
            active_ap,
            last_scan,
        }))
    }

    async fn access_points(
        &self,
        device_path: &OwnedObjectPath,
    ) -> Result<Vec<AccessPointRecord>, BackendFailure> {
        let wireless = WirelessProxy::builder(&self.connection)
            .path(device_path.clone())
            .map_err(|error| dbus_failure("enumerate access points", error))?
            .build()
            .await
            .map_err(|error| dbus_failure("enumerate access points", error))?;
        let paths = wireless
            .get_all_access_points()
            .await
            .map_err(|error| dbus_failure("enumerate access points", error))?;
        Ok(stream::iter(paths)
            .map(|path| self.read_access_point(path))
            .buffer_unordered(24)
            .filter_map(async |record| record.ok())
            .collect()
            .await)
    }

    async fn read_access_point(
        &self,
        path: OwnedObjectPath,
    ) -> Result<AccessPointRecord, BackendFailure> {
        let proxy = AccessPointProxy::builder(&self.connection)
            .path(path.clone())
            .map_err(|error| dbus_failure("inspect an access point", error))?
            .build()
            .await
            .map_err(|error| dbus_failure("inspect an access point", error))?;
        // Security is part of the network identity, so an unread flag cannot be
        // treated as zero: that would key the network as open, split it from
        // its saved profile, and leave the original key with nothing to match.
        let flags = proxy
            .flags()
            .await
            .map_err(|error| dbus_failure("read access point flags", error))?;
        let wpa = proxy
            .wpa_flags()
            .await
            .map_err(|error| dbus_failure("read access point WPA flags", error))?;
        let rsn = proxy
            .rsn_flags()
            .await
            .map_err(|error| dbus_failure("read access point RSN flags", error))?;
        Ok(AccessPointRecord {
            path,
            ssid: Ssid(
                proxy
                    .ssid()
                    .await
                    .map_err(|error| dbus_failure("read an access point SSID", error))?,
            ),
            security: map_ap_security(flags, wpa, rsn),
            strength: proxy.strength().await.unwrap_or(0),
            bssid: HardwareAddress(proxy.hw_address().await.unwrap_or_default()),
        })
    }

    async fn saved_profiles(&self) -> Result<Vec<SavedProfile>, BackendFailure> {
        let settings = SettingsProxy::new(&self.connection)
            .await
            .map_err(|error| dbus_failure("read saved Wi-Fi profiles", error))?;
        let paths = settings
            .list_connections()
            .await
            .map_err(|error| dbus_failure("read saved Wi-Fi profiles", error))?;
        Ok(stream::iter(paths)
            .map(|path| self.read_saved_profile(path))
            .buffer_unordered(16)
            .filter_map(async |profile| profile.ok().flatten())
            .collect()
            .await)
    }

    async fn read_saved_profile(
        &self,
        path: OwnedObjectPath,
    ) -> Result<Option<SavedProfile>, BackendFailure> {
        let proxy = SettingsConnectionProxy::builder(&self.connection)
            .path(path.clone())
            .map_err(|error| dbus_failure("inspect a saved Wi-Fi profile", error))?
            .build()
            .await
            .map_err(|error| dbus_failure("inspect a saved Wi-Fi profile", error))?;
        let settings = proxy
            .get_settings()
            .await
            .map_err(|error| dbus_failure("inspect a saved Wi-Fi profile", error))?;
        let Some(wifi) = settings.get("802-11-wireless") else {
            return Ok(None);
        };
        let Some(ssid) = wifi
            .get("ssid")
            .and_then(|value| value.try_clone().ok())
            .and_then(|value| Vec::<u8>::try_from(value).ok())
        else {
            return Ok(None);
        };
        let auto_join = settings
            .get("connection")
            .and_then(|connection| connection.get("autoconnect"))
            .and_then(|value| bool::try_from(value).ok())
            .unwrap_or(true);
        let security = if settings.contains_key("802-1x") {
            WifiSecurity::Enterprise
        } else if settings.contains_key("802-11-wireless-security") {
            WifiSecurity::Personal
        } else {
            WifiSecurity::Open
        };
        Ok(Some(SavedProfile {
            path,
            ssid: Ssid(ssid),
            security,
            auto_join,
        }))
    }

    async fn find_device(
        &self,
        interface: &InterfaceId,
    ) -> Result<OwnedObjectPath, BackendFailure> {
        let manager = NetworkManagerProxy::new(&self.connection)
            .await
            .map_err(|error| dbus_failure("find the Wi-Fi interface", error))?;
        for path in manager
            .get_devices()
            .await
            .map_err(|error| dbus_failure("find the Wi-Fi interface", error))?
        {
            let proxy = DeviceProxy::builder(&self.connection)
                .path(path.clone())
                .map_err(|error| dbus_failure("find the Wi-Fi interface", error))?
                .build()
                .await
                .map_err(|error| dbus_failure("find the Wi-Fi interface", error))?;
            if proxy.interface().await.ok().as_deref() == Some(interface.0.as_str()) {
                return Ok(path);
            }
        }
        Err(not_found(format!(
            "Wi-Fi interface {} is no longer available",
            interface.0
        )))
    }

    async fn find_access_point(
        &self,
        device: &OwnedObjectPath,
        id: &WifiNetworkId,
    ) -> Result<OwnedObjectPath, BackendFailure> {
        self.access_points(device)
            .await?
            .into_iter()
            .filter(|access_point| {
                access_point.ssid == id.ssid && access_point.security == id.security
            })
            .max_by_key(|access_point| access_point.strength)
            .map(|access_point| access_point.path)
            .ok_or_else(|| not_found(format!("{} is no longer in range", id.ssid.display())))
    }

    async fn connect(
        &self,
        command: &BackendCommand,
        id: &WifiNetworkId,
    ) -> Result<(), BackendFailure> {
        let device = self.find_device(&id.interface).await?;
        let access_point = self.find_access_point(&device, id).await?;
        let profiles = self.saved_profiles().await?;
        let manager = NetworkManagerProxy::new(&self.connection)
            .await
            .map_err(|error| dbus_failure("connect to Wi-Fi", error))?;
        if let Some(profile) = profiles
            .iter()
            .find(|profile| profile.ssid == id.ssid && profile.security == id.security)
        {
            manager
                .activate_connection(&profile.path, &device, &access_point)
                .await
                .map_err(|error| dbus_failure("connect to Wi-Fi", error))?;
            return Ok(());
        }
        if id.security != WifiSecurity::Open && command.credential.is_none() {
            return Err(BackendFailure {
                category: ErrorCategory::MissingSecrets,
                summary: format!("{} needs a Wi-Fi password", id.ssid.display()),
                detail: "No saved profile or credential is available for this network".into(),
                recovery: vec!["Retry and enter the network password".into()],
                retryable: true,
                raw_code: Some("missing-credential".into()),
            });
        }
        if id.security == WifiSecurity::Enterprise {
            return Err(BackendFailure {
                category: ErrorCategory::Unsupported,
                summary: format!("{} needs an enterprise Wi-Fi profile", id.ssid.display()),
                detail: "Creating a new 802.1X profile requires identity, EAP, and certificate choices; airman will not guess them".into(),
                recovery: vec!["Create or import the enterprise profile with NetworkManager, then activate it from airman".into()],
                retryable: false,
                raw_code: Some("enterprise-profile-required".into()),
            });
        }
        let settings = connection_settings(
            id,
            command.credential.as_ref().map(|secret| secret.expose()),
        );
        manager
            .add_and_activate_connection(&settings, &device, &access_point)
            .await
            .map_err(|error| dbus_failure("connect to Wi-Fi", error))?;
        Ok(())
    }

    async fn forget(&self, id: &WifiNetworkId) -> Result<(), BackendFailure> {
        let Some(profile) = self
            .saved_profiles()
            .await?
            .into_iter()
            .find(|profile| profile.ssid == id.ssid && profile.security == id.security)
        else {
            return Ok(());
        };
        SettingsConnectionProxy::builder(&self.connection)
            .path(profile.path)
            .map_err(|error| dbus_failure("forget the Wi-Fi profile", error))?
            .build()
            .await
            .map_err(|error| dbus_failure("forget the Wi-Fi profile", error))?
            .delete()
            .await
            .map_err(|error| dbus_failure("forget the Wi-Fi profile", error))
    }

    async fn set_auto_join(&self, id: &WifiNetworkId, enabled: bool) -> Result<(), BackendFailure> {
        let profile = self
            .saved_profiles()
            .await?
            .into_iter()
            .find(|profile| profile.ssid == id.ssid && profile.security == id.security)
            .ok_or_else(|| not_found(format!("{} has no saved profile", id.ssid.display())))?;
        let proxy = SettingsConnectionProxy::builder(&self.connection)
            .path(profile.path)
            .map_err(|error| dbus_failure("update Wi-Fi auto-join", error))?
            .build()
            .await
            .map_err(|error| dbus_failure("update Wi-Fi auto-join", error))?;
        let mut settings = proxy
            .get_settings()
            .await
            .map_err(|error| dbus_failure("read the Wi-Fi profile", error))?;
        settings
            .entry("connection".into())
            .or_default()
            .insert("autoconnect".into(), OwnedValue::from(enabled));
        proxy
            .update(settings)
            .await
            .map_err(|error| dbus_failure("update Wi-Fi auto-join", error))
    }

    async fn saved_secret(&self, id: &WifiNetworkId) -> Result<super::Secret, BackendFailure> {
        let profile = self
            .saved_profiles()
            .await?
            .into_iter()
            .find(|profile| profile.ssid == id.ssid && profile.security == id.security)
            .ok_or_else(|| not_found(format!("{} has no saved profile", id.ssid.display())))?;
        let proxy = SettingsConnectionProxy::builder(&self.connection)
            .path(profile.path)
            .map_err(|error| dbus_failure("read the saved Wi-Fi password", error))?
            .build()
            .await
            .map_err(|error| dbus_failure("read the saved Wi-Fi password", error))?;
        let secrets = proxy
            .get_secrets("802-11-wireless-security")
            .await
            .map_err(|error| dbus_failure("read the saved Wi-Fi password", error))?;
        let key = if id.security == WifiSecurity::Wep {
            "wep-key0"
        } else {
            "psk"
        };
        let password = secrets
            .get("802-11-wireless-security")
            .and_then(|security| security.get(key))
            .and_then(|value| <&str>::try_from(value).ok())
            .ok_or_else(|| BackendFailure {
                category: ErrorCategory::MissingSecrets,
                summary: format!("No saved password is available for {}", id.ssid.display()),
                detail: "NetworkManager returned the profile but not its secret. It may be agent-owned, not saved, or unavailable to this session".into(),
                recovery: vec!["Ensure a desktop secret agent is running in this login session, or reconnect and save the password".into()],
                retryable: true,
                raw_code: Some("saved-secret-unavailable".into()),
            })?;
        Ok(super::Secret::new(password.to_owned()))
    }
}

#[async_trait]
impl RadioBackend for NetworkManagerBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::NetworkManager
    }

    async fn probe(&self) -> ProbeResult {
        let mut result = probe_service(&self.connection, SERVICE, self.kind()).await;
        if result.status == super::ProbeStatus::Available {
            if let Ok(proxy) = NetworkManagerProxy::new(&self.connection).await {
                result.version = proxy.version().await.ok();
            }
        }
        result
    }

    fn subscribe(&self) -> broadcast::Receiver<BackendEvent> {
        self.events.subscribe()
    }

    async fn snapshot(&self) -> Result<BackendEvent, BackendFailure> {
        self.snapshot_inner().await
    }

    async fn capabilities(&self) -> CapabilityMap {
        wifi_capabilities()
    }

    async fn execute(
        &self,
        command: BackendCommand,
    ) -> Result<OperationAcceptance, BackendFailure> {
        let target = command.target.clone();
        match (&command.action, target) {
            (BackendAction::Scan, EntityId::WifiInterface(interface)) => {
                let device = self.find_device(&interface).await?;
                let proxy = WirelessProxy::builder(&self.connection)
                    .path(device)
                    .map_err(|error| dbus_failure("scan for Wi-Fi networks", error))?
                    .build()
                    .await
                    .map_err(|error| dbus_failure("scan for Wi-Fi networks", error))?;
                proxy
                    .request_scan(&HashMap::new())
                    .await
                    .map_err(|error| dbus_failure("scan for Wi-Fi networks", error))?;
            }
            (BackendAction::Connect, EntityId::Wifi(id)) => self.connect(&command, &id).await?,
            (BackendAction::Disconnect, EntityId::Wifi(id)) => {
                let device = self.find_device(&id.interface).await?;
                DeviceProxy::builder(&self.connection)
                    .path(device)
                    .map_err(|error| dbus_failure("disconnect Wi-Fi", error))?
                    .build()
                    .await
                    .map_err(|error| dbus_failure("disconnect Wi-Fi", error))?
                    .disconnect()
                    .await
                    .map_err(|error| dbus_failure("disconnect Wi-Fi", error))?;
            }
            (BackendAction::SetPowered(powered), EntityId::WifiInterface(_)) => {
                NetworkManagerProxy::new(&self.connection)
                    .await
                    .map_err(|error| dbus_failure("change the Wi-Fi radio state", error))?
                    .set_wireless_enabled(*powered)
                    .await
                    .map_err(|error| dbus_failure("change the Wi-Fi radio state", error))?;
            }
            (BackendAction::Forget, EntityId::Wifi(id)) => self.forget(&id).await?,
            (BackendAction::UpdateProfile(update), EntityId::Wifi(id))
                if update.auto_join.is_some() =>
            {
                self.set_auto_join(&id, update.auto_join.unwrap()).await?
            }
            _ => {
                return Err(unsupported(
                    "NetworkManager does not support that action for this item",
                ))
            }
        }
        Ok(OperationAcceptance {
            phase: OperationPhase::AwaitingConfirmation("waiting for NetworkManager state".into()),
            deadline_ms: super::dbus::monotonic_ms() + operation_timeout(command.desired),
        })
    }

    async fn cancel(&self, _operation_id: OperationId) -> Result<(), BackendFailure> {
        Ok(())
    }

    async fn wifi_secret(&self, id: &WifiNetworkId) -> Result<super::Secret, BackendFailure> {
        self.saved_secret(id).await
    }

    async fn diagnostics(&self) -> BackendDiagnostics {
        let probe = self.probe().await;
        let mut properties = BTreeMap::new();
        properties.insert("service".into(), SERVICE.into());
        properties.insert("epoch".into(), self.clock.epoch().to_string());
        if let Some(filter) = &self.interface_filter {
            properties.insert("interface_filter".into(), filter.clone());
        }
        BackendDiagnostics {
            backend: self.kind(),
            owner: probe.owner,
            version: probe.version,
            properties,
            warnings: probe.detail.into_iter().collect(),
        }
    }
}

fn wifi_capabilities() -> CapabilityMap {
    [
        Capability::RadioToggle,
        Capability::Scan,
        Capability::HiddenNetwork,
        Capability::Enterprise,
        Capability::Forget,
        Capability::SecretRetrieval,
        Capability::AutoJoin,
        Capability::Priority,
        Capability::PrivateMac,
        Capability::IpConfiguration,
        Capability::DnsConfiguration,
        Capability::ProxyConfiguration,
        Capability::Hotspot,
    ]
    .into_iter()
    .map(|capability| (capability, CapabilityState::Supported))
    .collect()
}

fn map_ap_security(flags: u32, wpa: u32, rsn: u32) -> WifiSecurity {
    const PRIVACY: u32 = 0x1;
    const KEY_MGMT_802_1X: u32 = 0x200;
    if wpa & KEY_MGMT_802_1X != 0 || rsn & KEY_MGMT_802_1X != 0 {
        WifiSecurity::Enterprise
    } else if wpa != 0 || rsn != 0 {
        WifiSecurity::Personal
    } else if flags & PRIVACY != 0 {
        WifiSecurity::Wep
    } else {
        WifiSecurity::Open
    }
}

fn map_connectivity(value: u32) -> Connectivity {
    match value {
        1 => Connectivity::None,
        2 => Connectivity::CaptivePortal,
        3 => Connectivity::Limited,
        4 => Connectivity::Internet,
        _ => Connectivity::Unknown,
    }
}

fn map_device_state(value: u32) -> ConnectionState {
    match value {
        40 => ConnectionState::Associating,
        50 | 60 => ConnectionState::Authenticating,
        70..=90 => ConnectionState::ObtainingAddress,
        100 => ConnectionState::Connected,
        110 => ConnectionState::Disconnecting,
        120 => ConnectionState::Failed,
        _ => ConnectionState::Disconnected,
    }
}

fn connection_settings<'a>(
    id: &'a WifiNetworkId,
    credential: Option<&'a str>,
) -> HashMap<String, HashMap<String, Value<'a>>> {
    let mut settings = HashMap::new();
    settings.insert(
        "connection".into(),
        HashMap::from([
            ("id".into(), Value::new(id.ssid.display())),
            ("type".into(), Value::new("802-11-wireless")),
            ("uuid".into(), Value::new(Uuid::new_v4().to_string())),
        ]),
    );
    settings.insert(
        "802-11-wireless".into(),
        HashMap::from([
            ("ssid".into(), Value::new(id.ssid.0.clone())),
            ("mode".into(), Value::new("infrastructure")),
        ]),
    );
    if let Some(credential) = credential {
        let security = if id.security == WifiSecurity::Wep {
            HashMap::from([
                ("key-mgmt".into(), Value::new("none")),
                ("wep-key0".into(), Value::new(credential)),
                ("wep-tx-keyidx".into(), Value::new(0_u32)),
            ])
        } else {
            HashMap::from([
                ("key-mgmt".into(), Value::new("wpa-psk")),
                ("psk".into(), Value::new(credential)),
            ])
        };
        settings.insert("802-11-wireless-security".into(), security);
    }
    settings.insert(
        "ipv4".into(),
        HashMap::from([("method".into(), Value::new("auto"))]),
    );
    settings.insert(
        "ipv6".into(),
        HashMap::from([("method".into(), Value::new("auto"))]),
    );
    settings
}

fn operation_timeout(desired: DesiredState) -> u64 {
    match desired {
        DesiredState::Connected => Duration::from_secs(45).as_millis() as u64,
        DesiredState::Scanning => Duration::from_secs(20).as_millis() as u64,
        _ => Duration::from_secs(15).as_millis() as u64,
    }
}

fn not_found(summary: String) -> BackendFailure {
    BackendFailure {
        category: ErrorCategory::NotFound,
        summary,
        detail: "The daemon no longer reports the selected object".into(),
        recovery: vec!["Refresh the list and retry".into()],
        retryable: true,
        raw_code: None,
    }
}

fn unsupported(detail: &str) -> BackendFailure {
    BackendFailure {
        category: ErrorCategory::Unsupported,
        summary: "This NetworkManager operation is not supported".into(),
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
    fn security_flags_distinguish_open_personal_and_enterprise() {
        assert_eq!(map_ap_security(0, 0, 0), WifiSecurity::Open);
        assert_eq!(map_ap_security(1, 0, 0), WifiSecurity::Wep);
        assert_eq!(map_ap_security(1, 0x100, 0), WifiSecurity::Personal);
        assert_eq!(map_ap_security(1, 0x200, 0), WifiSecurity::Enterprise);
    }

    #[test]
    fn network_manager_device_states_are_not_reported_as_connected_early() {
        assert_eq!(map_device_state(70), ConnectionState::ObtainingAddress);
        assert_eq!(map_device_state(100), ConnectionState::Connected);
        assert_eq!(map_device_state(120), ConnectionState::Failed);
    }
}
