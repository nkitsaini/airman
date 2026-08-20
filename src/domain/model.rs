use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;

pub const ACTIVITY_CAPACITY: usize = 1_000;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InterfaceId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AdapterId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HardwareAddress(pub String);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ssid(pub Vec<u8>);

impl Ssid {
    pub fn display(&self) -> String {
        let mut output = String::new();
        let mut remaining = self.0.as_slice();
        while !remaining.is_empty() {
            match std::str::from_utf8(remaining) {
                Ok(valid) => {
                    push_valid_ssid(&mut output, valid);
                    break;
                }
                Err(error) => {
                    let valid_length = error.valid_up_to();
                    let valid = std::str::from_utf8(&remaining[..valid_length]).unwrap_or_default();
                    push_valid_ssid(&mut output, valid);
                    let invalid_length =
                        error.error_len().unwrap_or(remaining.len() - valid_length);
                    for byte in &remaining[valid_length..valid_length + invalid_length] {
                        output.push_str(&format!("\\x{byte:02x}"));
                    }
                    remaining = &remaining[valid_length + invalid_length..];
                }
            }
        }
        output
    }
}

fn push_valid_ssid(output: &mut String, value: &str) {
    for character in value.chars() {
        if character == '\\' {
            output.push_str("\\\\");
        } else if character.is_control() {
            let mut encoded = [0; 4];
            for byte in character.encode_utf8(&mut encoded).bytes() {
                output.push_str(&format!("\\x{byte:02x}"));
            }
        } else {
            output.push(character);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WifiSecurity {
    Open,
    Wep,
    Personal,
    Enterprise,
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WifiNetworkId {
    pub interface: InterfaceId,
    pub ssid: Ssid,
    pub security: WifiSecurity,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WifiBssId {
    pub network: WifiNetworkId,
    pub bssid: HardwareAddress,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BluetoothDeviceId {
    pub adapter: AdapterId,
    pub address: HardwareAddress,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EntityId {
    Wifi(WifiNetworkId),
    Bluetooth(BluetoothDeviceId),
    WifiInterface(InterfaceId),
    BluetoothAdapter(AdapterId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BackendKind {
    NetworkManager,
    Iwd,
    WpaNetworkd,
    ConnMan,
    Bluez,
    Simulator,
}

impl fmt::Display for BackendKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NetworkManager => "NetworkManager",
            Self::Iwd => "iwd",
            Self::WpaNetworkd => "wpa_supplicant + networkd",
            Self::ConnMan => "ConnMan",
            Self::Bluez => "BlueZ",
            Self::Simulator => "simulator",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendHealth {
    Initializing,
    Ready,
    Degraded,
    Reconnecting,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendState {
    pub health: BackendHealth,
    pub epoch: u64,
    pub revision: u64,
    pub last_observed_ms: u64,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityState {
    Supported,
    Unsupported,
    Unavailable(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Capability {
    RadioToggle,
    Scan,
    HiddenNetwork,
    Enterprise,
    Forget,
    SecretRetrieval,
    AutoJoin,
    Priority,
    PrivateMac,
    IpConfiguration,
    DnsConfiguration,
    ProxyConfiguration,
    Hotspot,
    Pairing,
    Trust,
    Block,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connectivity {
    Unknown,
    None,
    Local,
    Limited,
    CaptivePortal,
    Internet,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Disconnected,
    Associating,
    Authenticating,
    ObtainingAddress,
    Connected,
    Disconnecting,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    Present,
    Unknown,
    OutOfRange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WifiNetwork {
    pub id: WifiNetworkId,
    pub display_name: String,
    pub signal: u8,
    pub state: ConnectionState,
    pub connectivity: Connectivity,
    pub saved: bool,
    pub auto_join: bool,
    pub bss_count: usize,
    pub active_bssid: Option<HardwareAddress>,
    pub present: bool,
    pub last_seen_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WifiInterface {
    pub id: InterfaceId,
    pub backend: BackendKind,
    pub powered: bool,
    pub scanning: bool,
    pub last_scan_ms: Option<u64>,
    pub addresses: Vec<IpAddressInfo>,
    pub capabilities: BTreeMap<Capability, CapabilityState>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct IpAddressInfo {
    pub address: String,
    pub prefix_len: u8,
    pub netmask: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BluetoothDevice {
    pub id: BluetoothDeviceId,
    pub name: String,
    pub state: ConnectionState,
    pub paired: bool,
    pub trusted: bool,
    pub blocked: bool,
    pub services_resolved: bool,
    pub rssi: Option<i16>,
    pub battery_percent: Option<u8>,
    pub presence: Presence,
    pub last_seen_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BluetoothAdapter {
    pub id: AdapterId,
    pub powered: bool,
    pub scanning: bool,
    pub capabilities: BTreeMap<Capability, CapabilityState>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WifiSnapshot {
    pub interfaces: Vec<WifiInterface>,
    pub networks: Vec<WifiNetwork>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BluetoothSnapshot {
    pub adapters: Vec<BluetoothAdapter>,
    pub devices: Vec<BluetoothDevice>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendPayload {
    WifiSnapshot(WifiSnapshot),
    BluetoothSnapshot(BluetoothSnapshot),
    Health {
        health: BackendHealth,
        detail: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendEvent {
    pub backend: BackendKind,
    pub epoch: u64,
    pub revision: u64,
    pub observed_at_ms: u64,
    pub payload: BackendPayload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OperationId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesiredState {
    Connected,
    Disconnected,
    Powered,
    Unpowered,
    Scanning,
    Idle,
    Present,
    Forgotten,
    AutoJoinEnabled,
    AutoJoinDisabled,
    Paired,
    Trusted,
    Untrusted,
    Blocked,
    Unblocked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperationPhase {
    Queued,
    Running(String),
    AwaitingConfirmation(String),
    Reconciling,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Operation {
    pub id: OperationId,
    pub backend: BackendKind,
    pub target: EntityId,
    pub desired: DesiredState,
    pub phase: OperationPhase,
    pub started_at_ms: u64,
    pub deadline_ms: u64,
    pub backend_epoch: u64,
    pub background: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCategory {
    Authentication,
    MissingSecrets,
    NotFound,
    IpConfiguration,
    RadioBlocked,
    PermissionDenied,
    ServiceUnavailable,
    Busy,
    Unsupported,
    Hardware,
    Timeout,
    InconsistentResponse,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserFacingError {
    pub category: ErrorCategory,
    pub summary: String,
    pub detail: String,
    pub recovery: Vec<String>,
    pub retryable: bool,
    pub backend: Option<BackendKind>,
    pub target: Option<EntityId>,
    pub raw_code: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityLevel {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityEntry {
    pub timestamp_ms: u64,
    pub level: ActivityLevel,
    pub message: String,
    pub operation: Option<OperationId>,
}

#[derive(Debug, Clone, Default)]
pub struct WifiState {
    pub interfaces: BTreeMap<InterfaceId, WifiInterface>,
    pub selected_interface: Option<InterfaceId>,
    pub networks: BTreeMap<WifiNetworkId, WifiNetwork>,
    pub order: Vec<WifiNetworkId>,
    pub selected: Option<WifiNetworkId>,
}

#[derive(Debug, Clone, Default)]
pub struct BluetoothState {
    pub adapters: BTreeMap<AdapterId, BluetoothAdapter>,
    pub selected_adapter: Option<AdapterId>,
    pub devices: BTreeMap<BluetoothDeviceId, BluetoothDevice>,
    pub order: Vec<BluetoothDeviceId>,
    pub selected: Option<BluetoothDeviceId>,
}

#[derive(Debug, Clone, Default)]
pub struct AppState {
    pub wifi: WifiState,
    pub bluetooth: BluetoothState,
    pub backends: BTreeMap<BackendKind, BackendState>,
    pub operations: BTreeMap<OperationId, Operation>,
    pub active_operation_by_target: HashMap<EntityId, OperationId>,
    pub activity: VecDeque<ActivityEntry>,
    pub current_error: Option<UserFacingError>,
}

impl AppState {
    pub fn push_activity(&mut self, entry: ActivityEntry) {
        if self.activity.len() == ACTIVITY_CAPACITY {
            self.activity.pop_front();
        }
        self.activity.push_back(entry);
    }

    pub fn active_operation(&self, target: &EntityId) -> Option<&Operation> {
        self.active_operation_by_target
            .get(target)
            .and_then(|id| self.operations.get(id))
    }
}
