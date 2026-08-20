use std::{collections::BTreeMap, fmt};

use async_trait::async_trait;
use tokio::sync::broadcast;
use zeroize::Zeroizing;

use crate::domain::{
    BackendEvent, BackendKind, Capability, CapabilityState, DesiredState, EntityId, ErrorCategory,
    OperationId, OperationPhase, UserFacingError, WifiNetworkId,
};

pub mod bluez;
pub mod connman;
pub(crate) mod dbus;
pub mod iwd;
pub mod network_manager;
pub(crate) mod system;
pub mod wpa_networkd;

pub use dbus::monotonic_ms;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeStatus {
    Available,
    NotRunning,
    NotInstalled,
    Ambiguous,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeResult {
    pub backend: BackendKind,
    pub status: ProbeStatus,
    pub owner: Option<String>,
    pub version: Option<String>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendAction {
    Scan,
    StopScan,
    Connect,
    Disconnect,
    SetPowered(bool),
    Forget,
    Pair,
    SetTrusted(bool),
    SetBlocked(bool),
    StartHotspot,
    StopHotspot,
    UpdateProfile(ProfileUpdate),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProfileUpdate {
    pub auto_join: Option<bool>,
    pub priority: Option<i32>,
    pub metered: Option<bool>,
    pub private_mac: Option<bool>,
    pub ipv4_method: Option<String>,
    pub ipv6_method: Option<String>,
    pub dns: Option<Vec<String>>,
    pub proxy: Option<String>,
    pub expected_revision: Option<u64>,
}

pub struct Secret(Zeroizing<String>);

impl Secret {
    pub fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
    }

    pub fn expose(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Secret([REDACTED])")
    }
}

#[derive(Debug)]
pub struct BackendCommand {
    pub operation_id: OperationId,
    pub target: EntityId,
    pub desired: DesiredState,
    pub action: BackendAction,
    pub credential: Option<Secret>,
    pub remember_credential: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationAcceptance {
    pub phase: OperationPhase,
    pub deadline_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendFailure {
    pub category: ErrorCategory,
    pub summary: String,
    pub detail: String,
    pub recovery: Vec<String>,
    pub retryable: bool,
    pub raw_code: Option<String>,
}

impl BackendFailure {
    pub fn into_user_error(
        self,
        backend: BackendKind,
        target: Option<EntityId>,
    ) -> UserFacingError {
        UserFacingError {
            category: self.category,
            summary: self.summary,
            detail: self.detail,
            recovery: self.recovery,
            retryable: self.retryable,
            backend: Some(backend),
            target,
            raw_code: self.raw_code,
        }
    }
}

impl fmt::Display for BackendFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.summary)
    }
}

impl std::error::Error for BackendFailure {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendDiagnostics {
    pub backend: BackendKind,
    pub owner: Option<String>,
    pub version: Option<String>,
    pub properties: BTreeMap<String, String>,
    pub warnings: Vec<String>,
}

pub type CapabilityMap = BTreeMap<Capability, CapabilityState>;

#[async_trait]
pub trait RadioBackend: Send + Sync {
    fn kind(&self) -> BackendKind;

    async fn probe(&self) -> ProbeResult;

    /// Subscribe before requesting the initial snapshot so no change is lost.
    fn subscribe(&self) -> broadcast::Receiver<BackendEvent>;

    async fn snapshot(&self) -> Result<BackendEvent, BackendFailure>;

    async fn capabilities(&self) -> CapabilityMap;

    /// A successful return means the daemon accepted the operation, not that
    /// the requested final state has been reached.
    async fn execute(&self, command: BackendCommand)
        -> Result<OperationAcceptance, BackendFailure>;

    async fn cancel(&self, operation_id: OperationId) -> Result<(), BackendFailure>;

    async fn wifi_secret(&self, _id: &WifiNetworkId) -> Result<Secret, BackendFailure> {
        Err(BackendFailure {
            category: ErrorCategory::Unsupported,
            summary: "Saved passwords are unavailable from this Wi-Fi service".into(),
            detail: "The selected backend does not expose stored credentials through its supported D-Bus API".into(),
            recovery: vec!["Use NetworkManager for retrievable system-stored secrets, or enter the password when connecting".into()],
            retryable: false,
            raw_code: Some("secret-retrieval-unsupported".into()),
        })
    }

    async fn diagnostics(&self) -> BackendDiagnostics;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_debug_output_is_always_redacted() {
        let secret = Secret::new("correct horse battery staple".into());
        assert_eq!(format!("{secret:?}"), "Secret([REDACTED])");
        assert!(!format!("{secret:?}").contains(secret.expose()));
    }
}
