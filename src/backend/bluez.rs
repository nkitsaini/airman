use std::{
    collections::BTreeMap, collections::BTreeSet, collections::HashMap, sync::Arc, time::Duration,
};

use async_trait::async_trait;
use tokio::sync::broadcast;
use zbus::{Connection, Proxy};
use zvariant::{OwnedObjectPath, OwnedValue, Value};

use super::{
    dbus::{dbus_failure, probe_service, spawn_signal_supervisor, ServiceClock, SnapshotFn},
    BackendAction, BackendCommand, BackendDiagnostics, BackendFailure, CapabilityMap,
    OperationAcceptance, ProbeResult, RadioBackend,
};
use crate::domain::{
    AdapterId, BackendEvent, BackendKind, BackendPayload, BluetoothAdapter, BluetoothDevice,
    BluetoothDeviceId, BluetoothSnapshot, Capability, CapabilityState, ConnectionState,
    DesiredState, EntityId, ErrorCategory, HardwareAddress, OperationId, OperationPhase, Presence,
};

const SERVICE: &str = "org.bluez";
const ROOT_PATH: &str = "/";
const OBJECT_MANAGER: &str = "org.freedesktop.DBus.ObjectManager";
const ADAPTER_INTERFACE: &str = "org.bluez.Adapter1";
const DEVICE_INTERFACE: &str = "org.bluez.Device1";
const BATTERY_INTERFACE: &str = "org.bluez.Battery1";

type Properties = HashMap<String, OwnedValue>;
type Interfaces = HashMap<String, Properties>;
type ManagedObjects = HashMap<OwnedObjectPath, Interfaces>;

pub struct BluezBackend {
    connection: Connection,
    adapter_filter: Option<String>,
    clock: Arc<ServiceClock>,
    events: broadcast::Sender<BackendEvent>,
}

impl BluezBackend {
    pub async fn new(connection: Connection, adapter_filter: Option<String>) -> Arc<Self> {
        let (events, _) = broadcast::channel(256);
        let backend = Arc::new(Self {
            connection,
            adapter_filter,
            clock: Arc::new(ServiceClock::new(BackendKind::Bluez)),
            events,
        });
        let weak = Arc::downgrade(&backend);
        let snapshot: SnapshotFn = Arc::new(move || {
            let weak = weak.clone();
            Box::pin(async move {
                let backend = weak.upgrade().ok_or_else(|| BackendFailure {
                    category: ErrorCategory::ServiceUnavailable,
                    summary: "BlueZ backend stopped".into(),
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

    async fn managed_objects(&self) -> Result<ManagedObjects, BackendFailure> {
        Proxy::new(&self.connection, SERVICE, ROOT_PATH, OBJECT_MANAGER)
            .await
            .map_err(|error| dbus_failure("inspect Bluetooth objects", error))?
            .call("GetManagedObjects", &())
            .await
            .map_err(|error| dbus_failure("inspect Bluetooth objects", error))
    }

    async fn snapshot_inner(&self) -> Result<BackendEvent, BackendFailure> {
        let objects = self.managed_objects().await?;
        let now = super::dbus::monotonic_ms();
        let audio_devices = compatible_audio_devices(&objects);
        let mut adapters = Vec::new();
        let mut adapter_paths = HashMap::<AdapterId, (OwnedObjectPath, bool)>::new();

        for (path, interfaces) in &objects {
            let Some(properties) = interfaces.get(ADAPTER_INTERFACE) else {
                continue;
            };
            let id = AdapterId(path.rsplit('/').next().unwrap_or_default().to_owned());
            if self
                .adapter_filter
                .as_ref()
                .is_some_and(|filter| filter != &id.0)
            {
                continue;
            }
            let discovering = bool_property(properties, "Discovering").unwrap_or(false);
            adapter_paths.insert(id.clone(), (path.clone(), discovering));
            adapters.push(BluetoothAdapter {
                id,
                powered: bool_property(properties, "Powered").unwrap_or(false),
                scanning: discovering,
                capabilities: bluetooth_capabilities(),
            });
        }

        let mut devices = Vec::new();
        for (path, interfaces) in &objects {
            let Some(properties) = interfaces.get(DEVICE_INTERFACE) else {
                continue;
            };
            let Some(adapter_path) = path_property(properties, "Adapter") else {
                continue;
            };
            let Some((adapter, discovering)) =
                adapter_paths.iter().find_map(|(id, (path, discovering))| {
                    (path == &adapter_path).then_some((id.clone(), *discovering))
                })
            else {
                continue;
            };
            let address = string_property(properties, "Address")
                .unwrap_or_else(|| address_from_path(path.as_str()));
            let connected = bool_property(properties, "Connected").unwrap_or(false);
            let services_resolved = bool_property(properties, "ServicesResolved").unwrap_or(false);
            let name = string_property(properties, "Alias")
                .or_else(|| string_property(properties, "Name"))
                .unwrap_or_else(|| address.clone());
            let rssi = i16_property(properties, "RSSI");
            let battery_percent = interfaces
                .get(BATTERY_INTERFACE)
                .and_then(|battery| u8_property(battery, "Percentage"));
            let presence = bluez_presence(connected, rssi, discovering);
            let audio_device = advertised_audio_address(properties)
                .map(|address| BluetoothDeviceId {
                    adapter: adapter.clone(),
                    address: HardwareAddress(address),
                })
                .filter(|id| id.address.0 != address && audio_devices.contains(id));
            devices.push(BluetoothDevice {
                id: BluetoothDeviceId {
                    adapter,
                    address: HardwareAddress(address),
                },
                audio_device,
                name,
                state: if connected {
                    ConnectionState::Connected
                } else {
                    ConnectionState::Disconnected
                },
                paired: bool_property(properties, "Paired").unwrap_or(false),
                trusted: bool_property(properties, "Trusted").unwrap_or(false),
                blocked: bool_property(properties, "Blocked").unwrap_or(false),
                services_resolved,
                rssi,
                battery_percent,
                presence,
                last_seen_ms: if presence == Presence::Present {
                    now
                } else {
                    0
                },
            });
        }

        Ok(self
            .clock
            .event(BackendPayload::BluetoothSnapshot(BluetoothSnapshot {
                adapters,
                devices,
            })))
    }

    async fn adapter_path(&self, id: &AdapterId) -> Result<OwnedObjectPath, BackendFailure> {
        self.managed_objects()
            .await?
            .into_iter()
            .find(|(path, interfaces)| {
                interfaces.contains_key(ADAPTER_INTERFACE)
                    && path.rsplit('/').next() == Some(id.0.as_str())
            })
            .map(|(path, _)| path)
            .ok_or_else(|| not_found(format!("Bluetooth adapter {} is unavailable", id.0)))
    }

    async fn device_path(&self, id: &BluetoothDeviceId) -> Result<OwnedObjectPath, BackendFailure> {
        self.managed_objects()
            .await?
            .into_iter()
            .find(|(_, interfaces)| {
                let Some(properties) = interfaces.get(DEVICE_INTERFACE) else {
                    return false;
                };
                string_property(properties, "Address").as_deref() == Some(id.address.0.as_str())
                    && path_property(properties, "Adapter")
                        .is_some_and(|path| path.rsplit('/').next() == Some(id.adapter.0.as_str()))
            })
            .map(|(path, _)| path)
            .ok_or_else(|| not_found(format!("Bluetooth device {} is unavailable", id.address.0)))
    }

    async fn device_call(
        &self,
        id: &BluetoothDeviceId,
        method: &str,
    ) -> Result<(), BackendFailure> {
        let path = self.device_path(id).await?;
        Proxy::new(&self.connection, SERVICE, path, DEVICE_INTERFACE)
            .await
            .map_err(|error| dbus_failure("control the Bluetooth device", error))?
            .call(method, &())
            .await
            .map_err(|error| dbus_failure("control the Bluetooth device", error))
    }
}

#[async_trait]
impl RadioBackend for BluezBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Bluez
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
        bluetooth_capabilities()
    }

    async fn execute(
        &self,
        command: BackendCommand,
    ) -> Result<OperationAcceptance, BackendFailure> {
        match (&command.action, &command.target) {
            (BackendAction::Connect, EntityId::Bluetooth(id)) => {
                self.device_call(id, "Connect").await?
            }
            (BackendAction::Disconnect, EntityId::Bluetooth(id)) => {
                self.device_call(id, "Disconnect").await?
            }
            (BackendAction::Pair, EntityId::Bluetooth(id)) => self.device_call(id, "Pair").await?,
            (BackendAction::SetTrusted(trusted), EntityId::Bluetooth(id)) => {
                let path = self.device_path(id).await?;
                Proxy::new(&self.connection, SERVICE, path, DEVICE_INTERFACE)
                    .await
                    .map_err(|error| dbus_failure("change Bluetooth trust", error))?
                    .set_property("Trusted", *trusted)
                    .await
                    .map_err(|error| dbus_failure("change Bluetooth trust", error))?;
            }
            (BackendAction::SetBlocked(blocked), EntityId::Bluetooth(id)) => {
                let path = self.device_path(id).await?;
                Proxy::new(&self.connection, SERVICE, path, DEVICE_INTERFACE)
                    .await
                    .map_err(|error| dbus_failure("change Bluetooth block state", error))?
                    .set_property("Blocked", *blocked)
                    .await
                    .map_err(|error| dbus_failure("change Bluetooth block state", error))?;
            }
            (BackendAction::Forget, EntityId::Bluetooth(id)) => {
                let adapter = self.adapter_path(&id.adapter).await?;
                let device = self.device_path(id).await?;
                Proxy::new(&self.connection, SERVICE, adapter, ADAPTER_INTERFACE)
                    .await
                    .map_err(|error| dbus_failure("forget the Bluetooth device", error))?
                    .call::<_, _, ()>("RemoveDevice", &(device,))
                    .await
                    .map_err(|error| dbus_failure("forget the Bluetooth device", error))?;
            }
            (BackendAction::Scan, EntityId::BluetoothAdapter(id)) => {
                let adapter = self.adapter_path(id).await?;
                let proxy = Proxy::new(&self.connection, SERVICE, adapter, ADAPTER_INTERFACE)
                    .await
                    .map_err(|error| dbus_failure("start Bluetooth discovery", error))?;
                let filter = HashMap::from([
                    ("Transport", Value::new("auto")),
                    ("RSSI", Value::new(-127_i16)),
                    ("DuplicateData", Value::new(false)),
                ]);
                if let Err(error) = proxy
                    .call::<_, _, ()>("SetDiscoveryFilter", &(filter,))
                    .await
                {
                    // Older controllers may reject one of these optional
                    // filters. Discovery itself remains useful without them.
                    tracing::debug!(%error, adapter = %id.0, "BlueZ discovery filter unavailable");
                }
                proxy
                    .call::<_, _, ()>("StartDiscovery", &())
                    .await
                    .map_err(|error| dbus_failure("start Bluetooth discovery", error))?;
            }
            (BackendAction::StopScan, EntityId::BluetoothAdapter(id)) => {
                let adapter = self.adapter_path(id).await?;
                let proxy = Proxy::new(&self.connection, SERVICE, adapter, ADAPTER_INTERFACE)
                    .await
                    .map_err(|error| dbus_failure("stop Bluetooth discovery", error))?;
                proxy
                    .call::<_, _, ()>("StopDiscovery", &())
                    .await
                    .map_err(|error| dbus_failure("stop Bluetooth discovery", error))?;
                let empty_filter = HashMap::<String, Value<'_>>::new();
                if let Err(error) = proxy
                    .call::<_, _, ()>("SetDiscoveryFilter", &(empty_filter,))
                    .await
                {
                    tracing::debug!(%error, adapter = %id.0, "could not clear BlueZ discovery filter");
                }
            }
            (BackendAction::SetPowered(powered), EntityId::BluetoothAdapter(id)) => {
                let adapter = self.adapter_path(id).await?;
                Proxy::new(&self.connection, SERVICE, adapter, ADAPTER_INTERFACE)
                    .await
                    .map_err(|error| dbus_failure("change the Bluetooth radio state", error))?
                    .set_property("Powered", *powered)
                    .await
                    .map_err(|error| dbus_failure("change the Bluetooth radio state", error))?;
            }
            _ => return Err(unsupported()),
        }
        Ok(OperationAcceptance {
            phase: OperationPhase::AwaitingConfirmation("waiting for BlueZ state".into()),
            deadline_ms: super::dbus::monotonic_ms()
                + Duration::from_secs(if command.desired == DesiredState::Connected {
                    45
                } else {
                    20
                })
                .as_millis() as u64,
        })
    }

    async fn cancel(&self, _operation_id: OperationId) -> Result<(), BackendFailure> {
        Ok(())
    }

    async fn diagnostics(&self) -> BackendDiagnostics {
        let probe = self.probe().await;
        let mut properties = BTreeMap::new();
        properties.insert("service".into(), SERVICE.into());
        properties.insert("epoch".into(), self.clock.epoch().to_string());
        if let Some(filter) = &self.adapter_filter {
            properties.insert("adapter_filter".into(), filter.clone());
        }
        BackendDiagnostics {
            backend: self.kind(),
            owner: probe.owner,
            version: None,
            properties,
            warnings: probe.detail.into_iter().collect(),
        }
    }
}

fn bluetooth_capabilities() -> CapabilityMap {
    [
        Capability::RadioToggle,
        Capability::Scan,
        Capability::Forget,
        Capability::Pairing,
        Capability::Trust,
        Capability::Block,
    ]
    .into_iter()
    .map(|capability| (capability, CapabilityState::Supported))
    .collect()
}

fn compatible_audio_devices(objects: &ManagedObjects) -> BTreeSet<BluetoothDeviceId> {
    objects
        .iter()
        .filter_map(|(path, interfaces)| {
            let properties = interfaces.get(DEVICE_INTERFACE)?;
            let uuids = Vec::<String>::try_from(properties.get("UUIDs")?.try_clone().ok()?).ok()?;
            // Require both an audio sink and the matching vendor control service.
            if ![
                "0000110b-0000-1000-8000-00805f9b34fb",
                "0000079a-d102-11e1-9b23-00025b00a5a5",
            ]
            .iter()
            .all(|expected| uuids.iter().any(|uuid| uuid.eq_ignore_ascii_case(expected)))
            {
                return None;
            }
            let adapter = path_property(properties, "Adapter")?;
            Some(BluetoothDeviceId {
                adapter: AdapterId(adapter.rsplit('/').next()?.to_owned()),
                address: HardwareAddress(
                    string_property(properties, "Address")
                        .unwrap_or_else(|| address_from_path(path.as_str()))
                        .to_uppercase(),
                ),
            })
        })
        .collect()
}

fn advertised_audio_address(properties: &Properties) -> Option<String> {
    let data =
        HashMap::<u16, OwnedValue>::try_from(properties.get("ManufacturerData")?.try_clone().ok()?)
            .ok()?;
    let payload = Vec::<u8>::try_from(data.get(&0xa408)?.try_clone().ok()?).ok()?;
    // Recognized manufacturer layout: a 19-byte payload with this
    // eight-byte prefix, followed by the classic address in little-endian
    // order. This is a deliberately narrow compatibility rule, not a generic
    // protocol decoder. Leave other formats visible until validated.
    if payload.len() != 19 || !payload.starts_with(&[0x12, 0x14, 0x05, 0x3f, 0, 3, 0x98, 1]) {
        return None;
    }
    let address = &payload[8..14];
    if address.iter().all(|byte| *byte == 0) || address.iter().all(|byte| *byte == 0xff) {
        return None;
    }
    Some(
        address
            .iter()
            .rev()
            .map(|byte| format!("{byte:02X}"))
            .collect::<Vec<_>>()
            .join(":"),
    )
}

/// Device identity comes from the `Address` property. The object path is the
/// fallback, and it has to produce the identical address: a device listed under
/// two spellings of the same address would appear twice, and the duplicate
/// would never match anything BlueZ reports later.
fn address_from_path(path: &str) -> String {
    path.rsplit('/')
        .next()
        .unwrap_or_default()
        .trim_start_matches("dev_")
        .replace('_', ":")
        .to_uppercase()
}

fn bluez_presence(connected: bool, rssi: Option<i16>, discovering: bool) -> Presence {
    if connected || (discovering && rssi.is_some()) {
        Presence::Present
    } else {
        // Device1 objects, particularly paired ones, outlive discovery. RSSI is
        // optional, so its absence cannot establish that the radio is absent.
        Presence::Unknown
    }
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

fn i16_property(properties: &Properties, name: &str) -> Option<i16> {
    properties
        .get(name)
        .and_then(|value| i16::try_from(value).ok())
}

fn u8_property(properties: &Properties, name: &str) -> Option<u8> {
    properties
        .get(name)
        .and_then(|value| u8::try_from(value).ok())
}

fn path_property(properties: &Properties, name: &str) -> Option<OwnedObjectPath> {
    properties
        .get(name)
        .and_then(|value| <&zvariant::ObjectPath<'_>>::try_from(value).ok())
        .and_then(|path| OwnedObjectPath::try_from(path.as_str()).ok())
}

fn not_found(summary: String) -> BackendFailure {
    BackendFailure {
        category: ErrorCategory::NotFound,
        summary,
        detail: "BlueZ removed the object before the operation began".into(),
        recovery: vec!["Run discovery and retry if the device is nearby".into()],
        retryable: true,
        raw_code: None,
    }
}

fn unsupported() -> BackendFailure {
    BackendFailure {
        category: ErrorCategory::Unsupported,
        summary: "This BlueZ operation is not supported".into(),
        detail: "The selected action does not apply to this Bluetooth object".into(),
        recovery: Vec::new(),
        retryable: false,
        raw_code: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AUXILIARY_ADVERTISEMENT: [u8; 19] = [
        0x12, 0x14, 0x05, 0x3f, 0, 3, 0x98, 1, 0xbc, 0xa7, 0x9c, 0x85, 0x0e, 0x88, 1, 0xb5, 0, 0, 2,
    ];

    fn advertisement(company: u16, payload: &[u8]) -> Properties {
        HashMap::from([(
            "ManufacturerData".into(),
            Value::from(HashMap::from([(company, Value::from(payload.to_vec()))]))
                .try_to_owned()
                .unwrap(),
        )])
    }

    #[test]
    fn auxiliary_advertisement_links_the_exact_audio_address() {
        assert_eq!(
            advertised_audio_address(&advertisement(0xa408, &AUXILIARY_ADVERTISEMENT)),
            Some("88:0E:85:9C:A7:BC".into())
        );
        // Another headset of the same model must link to its own address.
        let mut other = AUXILIARY_ADVERTISEMENT;
        other[8] = 0xbd;
        assert_eq!(
            advertised_audio_address(&advertisement(0xa408, &other)),
            Some("88:0E:85:9C:A7:BD".into())
        );
    }

    #[test]
    fn unknown_or_malformed_advertisements_do_not_create_links() {
        assert_eq!(advertised_audio_address(&Properties::new()), None);
        assert_eq!(
            advertised_audio_address(&advertisement(0x004c, &AUXILIARY_ADVERTISEMENT)),
            None
        );
        for length in 0..AUXILIARY_ADVERTISEMENT.len() {
            assert_eq!(
                advertised_audio_address(&advertisement(
                    0xa408,
                    &AUXILIARY_ADVERTISEMENT[..length]
                )),
                None
            );
        }
        let mut unknown = AUXILIARY_ADVERTISEMENT;
        unknown[0] = 0x13;
        assert_eq!(
            advertised_audio_address(&advertisement(0xa408, &unknown)),
            None
        );
        let mut extended = AUXILIARY_ADVERTISEMENT.to_vec();
        extended.push(0);
        assert_eq!(
            advertised_audio_address(&advertisement(0xa408, &extended)),
            None
        );
        for invalid in [0, 0xff] {
            let mut payload = AUXILIARY_ADVERTISEMENT;
            payload[8..14].fill(invalid);
            assert_eq!(
                advertised_audio_address(&advertisement(0xa408, &payload)),
                None
            );
        }
        let wrong_type = HashMap::from([("ManufacturerData".into(), OwnedValue::from(42u16))]);
        assert_eq!(advertised_audio_address(&wrong_type), None);
    }

    #[test]
    fn audio_counterparts_require_vendor_and_audio_services_and_keep_adapter_identity() {
        let audio = "0000110b-0000-1000-8000-00805f9b34fb";
        let vendor = "0000079a-d102-11e1-9b23-00025b00a5a5";
        for (services, recognized) in [
            (vec![], false),
            (vec![audio], false),
            (vec![vendor], false),
            (vec![audio, vendor], true),
        ] {
            let properties = HashMap::from([
                (
                    "Adapter".into(),
                    Value::from(OwnedObjectPath::try_from("/org/bluez/hci1").unwrap())
                        .try_to_owned()
                        .unwrap(),
                ),
                (
                    "UUIDs".into(),
                    Value::from(services).try_to_owned().unwrap(),
                ),
            ]);
            let objects = HashMap::from([(
                OwnedObjectPath::try_from("/org/bluez/hci1/dev_88_0E_85_9C_A7_BC").unwrap(),
                HashMap::from([(DEVICE_INTERFACE.into(), properties)]),
            )]);
            let ids = compatible_audio_devices(&objects);
            assert_eq!(ids.len(), usize::from(recognized));
            if recognized {
                assert!(ids.contains(&BluetoothDeviceId {
                    adapter: AdapterId("hci1".into()),
                    address: HardwareAddress("88:0E:85:9C:A7:BC".into()),
                }));
            }
        }
    }

    #[test]
    fn bluetooth_capabilities_include_pairing_and_trust() {
        let capabilities = bluetooth_capabilities();
        assert_eq!(
            capabilities[&Capability::Pairing],
            CapabilityState::Supported
        );
        assert_eq!(capabilities[&Capability::Trust], CapabilityState::Supported);
    }

    #[test]
    fn object_paths_resolve_to_the_same_address_bluez_reports() {
        assert_eq!(
            address_from_path("/org/bluez/hci0/dev_88_0E_85_9C_A7_BC"),
            "88:0E:85:9C:A7:BC"
        );
    }

    #[test]
    fn missing_rssi_does_not_claim_a_device_is_out_of_range() {
        assert_eq!(bluez_presence(false, None, true), Presence::Unknown);
        assert_eq!(bluez_presence(false, Some(-60), true), Presence::Present);
        assert_eq!(bluez_presence(false, Some(-60), false), Presence::Unknown);
        assert_eq!(bluez_presence(true, None, false), Presence::Present);
    }
}
