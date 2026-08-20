use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use super::{
    ActivityEntry, ActivityLevel, AppState, BackendEvent, BackendPayload, BackendState,
    BluetoothDevice, BluetoothDeviceId, BluetoothSnapshot, ErrorCategory, Operation, OperationId,
    OperationPhase, UserFacingError, WifiNetwork, WifiNetworkId, WifiSnapshot,
};

const MISSING_WIFI_RETENTION_MS: u64 = 30_000;
const MISSING_BLUETOOTH_RETENTION_MS: u64 = 60_000;

#[derive(Debug, Clone)]
pub enum AppEvent {
    Backend(BackendEvent),
    OperationStarted(Operation),
    OperationProgress {
        id: OperationId,
        phase: OperationPhase,
        timestamp_ms: u64,
    },
    OperationSucceeded {
        id: OperationId,
        message: String,
        timestamp_ms: u64,
    },
    OperationFailed {
        id: OperationId,
        error: UserFacingError,
        timestamp_ms: u64,
    },
    SelectWifi(Option<WifiNetworkId>),
    SelectBluetooth(Option<BluetoothDeviceId>),
    DismissError,
    Tick(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReduceOutcome {
    Changed,
    IgnoredStale,
    Unchanged,
}

/// Entries a daemon stops reporting are kept for a short grace period so a
/// single snapshot gap cannot make the list flicker. The grace period is
/// bounded for every entry, including saved networks and paired devices: every
/// backend reports those for as long as they exist, so one that stops arriving
/// has been removed, replaced, or re-keyed by the daemon. Keeping it until the
/// process exits would leave a phantom row that no rescan can clear.
#[derive(Debug, Default)]
pub struct Reducer {
    pub state: AppState,
    wifi_missing_since: BTreeMap<WifiNetworkId, u64>,
    bluetooth_missing_since: BTreeMap<BluetoothDeviceId, u64>,
}

impl Reducer {
    pub fn apply(&mut self, event: AppEvent) -> ReduceOutcome {
        match event {
            AppEvent::Backend(event) => self.apply_backend(event),
            AppEvent::OperationStarted(operation) => self.start_operation(operation),
            AppEvent::OperationProgress {
                id,
                phase,
                timestamp_ms,
            } => self.progress_operation(id, phase, timestamp_ms),
            AppEvent::OperationSucceeded {
                id,
                message,
                timestamp_ms,
            } => self.finish_operation(id, Ok(message), timestamp_ms),
            AppEvent::OperationFailed {
                id,
                error,
                timestamp_ms,
            } => self.finish_operation(id, Err(error), timestamp_ms),
            AppEvent::SelectWifi(selected) => {
                if self.state.wifi.selected == selected {
                    ReduceOutcome::Unchanged
                } else {
                    self.state.wifi.selected = selected;
                    ReduceOutcome::Changed
                }
            }
            AppEvent::SelectBluetooth(selected) => {
                if self.state.bluetooth.selected == selected {
                    ReduceOutcome::Unchanged
                } else {
                    self.state.bluetooth.selected = selected;
                    ReduceOutcome::Changed
                }
            }
            AppEvent::DismissError => {
                if self.state.current_error.take().is_some() {
                    ReduceOutcome::Changed
                } else {
                    ReduceOutcome::Unchanged
                }
            }
            AppEvent::Tick(now_ms) => self.expire_missing(now_ms),
        }
    }

    fn apply_backend(&mut self, event: BackendEvent) -> ReduceOutcome {
        if let Some(clock) = self.state.backends.get(&event.backend) {
            if event.epoch < clock.epoch
                || (event.epoch == clock.epoch && event.revision <= clock.revision)
            {
                return ReduceOutcome::IgnoredStale;
            }
        }

        let previous_epoch = self
            .state
            .backends
            .get(&event.backend)
            .map_or(event.epoch, |state| state.epoch);
        let default_health = self
            .state
            .backends
            .get(&event.backend)
            .map_or(super::BackendHealth::Ready, |state| state.health);

        let (health, detail) = match &event.payload {
            BackendPayload::Health { health, detail } => (*health, detail.clone()),
            _ => (default_health, None),
        };

        self.state.backends.insert(
            event.backend,
            BackendState {
                health,
                epoch: event.epoch,
                revision: event.revision,
                last_observed_ms: event.observed_at_ms,
                detail,
            },
        );

        if event.epoch > previous_epoch {
            self.cancel_operations_from_epoch(event.backend, previous_epoch, event.observed_at_ms);
        }

        match event.payload {
            BackendPayload::WifiSnapshot(snapshot) => {
                self.apply_wifi_snapshot(snapshot, event.observed_at_ms)
            }
            BackendPayload::BluetoothSnapshot(snapshot) => {
                self.apply_bluetooth_snapshot(snapshot, event.observed_at_ms)
            }
            BackendPayload::Health { .. } => {}
        }
        self.confirm_observed_operations(event.backend, event.observed_at_ms);
        ReduceOutcome::Changed
    }

    fn apply_wifi_snapshot(&mut self, snapshot: WifiSnapshot, now_ms: u64) {
        self.state.wifi.interfaces = snapshot
            .interfaces
            .into_iter()
            .map(|interface| (interface.id.clone(), interface))
            .collect();
        if self
            .state
            .wifi
            .selected_interface
            .as_ref()
            .is_none_or(|id| !self.state.wifi.interfaces.contains_key(id))
        {
            self.state.wifi.selected_interface = self.state.wifi.interfaces.keys().next().cloned();
        }

        let incoming: BTreeMap<_, _> = snapshot
            .networks
            .into_iter()
            .map(|network| (network.id.clone(), network))
            .collect();
        let reported = incoming.keys().cloned().collect::<BTreeSet<_>>();
        let reported_names = incoming
            .keys()
            .map(|id| (id.interface.clone(), id.ssid.clone()))
            .collect::<BTreeSet<_>>();
        let selected = self.state.wifi.selected.clone();
        let mut merged = incoming;

        for (id, previous) in &self.state.wifi.networks {
            if reported.contains(id) {
                continue;
            }
            let being_forgotten = self
                .state
                .active_operation(&super::EntityId::Wifi(id.clone()))
                .is_some_and(|operation| operation.desired == super::DesiredState::Forgotten);
            // The same name under a different security type is the same network
            // re-keyed by the daemon, not a second one that went out of range.
            let superseded = reported_names.contains(&(id.interface.clone(), id.ssid.clone()));
            let missing_since = self.wifi_missing_since.get(id).copied().unwrap_or(now_ms);
            if being_forgotten
                || superseded
                || !self.state.wifi.interfaces.contains_key(&id.interface)
                || now_ms.saturating_sub(missing_since) > MISSING_WIFI_RETENTION_MS
            {
                continue;
            }
            let mut missing = previous.clone();
            missing.present = false;
            merged.insert(id.clone(), missing);
        }

        let missing_since = merged
            .keys()
            .filter(|id| !reported.contains(*id))
            .map(|id| {
                let since = self.wifi_missing_since.get(id).copied().unwrap_or(now_ms);
                (id.clone(), since)
            })
            .collect();
        self.wifi_missing_since = missing_since;

        self.state.wifi.order = stable_wifi_order(&self.state.wifi.order, &merged);
        self.state.wifi.networks = merged;
        self.state.wifi.selected = preserve_or_first(selected, &self.state.wifi.order);
    }

    fn apply_bluetooth_snapshot(&mut self, snapshot: BluetoothSnapshot, now_ms: u64) {
        self.state.bluetooth.adapters = snapshot
            .adapters
            .into_iter()
            .map(|adapter| (adapter.id.clone(), adapter))
            .collect();
        if self
            .state
            .bluetooth
            .selected_adapter
            .as_ref()
            .is_none_or(|id| !self.state.bluetooth.adapters.contains_key(id))
        {
            self.state.bluetooth.selected_adapter =
                self.state.bluetooth.adapters.keys().next().cloned();
        }

        let incoming: BTreeMap<_, _> = snapshot
            .devices
            .into_iter()
            .map(|mut device| {
                if device.presence == super::Presence::Unknown {
                    if let Some(previous) = self.state.bluetooth.devices.get(&device.id) {
                        device.last_seen_ms = previous.last_seen_ms;
                    }
                }
                (device.id.clone(), device)
            })
            .collect();
        let reported = incoming.keys().cloned().collect::<BTreeSet<_>>();
        let reported_names = incoming
            .values()
            .map(|device| (device.id.adapter.clone(), device.name.clone()))
            .collect::<BTreeSet<_>>();
        let selected = self.state.bluetooth.selected.clone();
        let mut merged = incoming;

        for (id, previous) in &self.state.bluetooth.devices {
            if reported.contains(id) {
                continue;
            }
            let being_forgotten = self
                .state
                .active_operation(&super::EntityId::Bluetooth(id.clone()))
                .is_some_and(|operation| operation.desired == super::DesiredState::Forgotten);
            // BlueZ re-creates a device object when its address changes, for
            // example when a resolved identity replaces a random address. The
            // replacement carries the same name, so keeping the old object
            // would show the device twice, once permanently out of range.
            let superseded = reported_names.contains(&(id.adapter.clone(), previous.name.clone()));
            let missing_since = self
                .bluetooth_missing_since
                .get(id)
                .copied()
                .unwrap_or(now_ms);
            if being_forgotten
                || superseded
                || !self.state.bluetooth.adapters.contains_key(&id.adapter)
                || now_ms.saturating_sub(missing_since) > MISSING_BLUETOOTH_RETENTION_MS
            {
                continue;
            }
            let mut missing = previous.clone();
            missing.presence = super::Presence::OutOfRange;
            merged.insert(id.clone(), missing);
        }

        let missing_since = merged
            .keys()
            .filter(|id| !reported.contains(*id))
            .map(|id| {
                let since = self
                    .bluetooth_missing_since
                    .get(id)
                    .copied()
                    .unwrap_or(now_ms);
                (id.clone(), since)
            })
            .collect();
        self.bluetooth_missing_since = missing_since;

        self.state.bluetooth.order = stable_bluetooth_order(&self.state.bluetooth.order, &merged);
        self.state.bluetooth.devices = merged;
        self.state.bluetooth.selected = preserve_or_first(selected, &self.state.bluetooth.order);
    }

    fn start_operation(&mut self, operation: Operation) -> ReduceOutcome {
        if let Some(previous_id) = self
            .state
            .active_operation_by_target
            .insert(operation.target.clone(), operation.id)
        {
            self.state.operations.remove(&previous_id);
            self.state.push_activity(ActivityEntry {
                timestamp_ms: operation.started_at_ms,
                level: ActivityLevel::Warning,
                message: format!("operation {} was superseded", previous_id.0),
                operation: Some(previous_id),
            });
        }

        if !operation.background {
            self.state.push_activity(ActivityEntry {
                timestamp_ms: operation.started_at_ms,
                level: ActivityLevel::Info,
                message: format!("operation {} queued", operation.id.0),
                operation: Some(operation.id),
            });
        }
        self.state.operations.insert(operation.id, operation);
        ReduceOutcome::Changed
    }

    fn progress_operation(
        &mut self,
        id: OperationId,
        phase: OperationPhase,
        timestamp_ms: u64,
    ) -> ReduceOutcome {
        let Some(operation) = self.state.operations.get_mut(&id) else {
            return ReduceOutcome::IgnoredStale;
        };
        operation.phase = phase.clone();
        self.state.push_activity(ActivityEntry {
            timestamp_ms,
            level: ActivityLevel::Info,
            message: format_operation_phase(&phase),
            operation: Some(id),
        });
        ReduceOutcome::Changed
    }

    fn finish_operation(
        &mut self,
        id: OperationId,
        result: Result<String, UserFacingError>,
        timestamp_ms: u64,
    ) -> ReduceOutcome {
        let Some(operation) = self.state.operations.remove(&id) else {
            return ReduceOutcome::IgnoredStale;
        };
        if self.state.active_operation_by_target.get(&operation.target) != Some(&id) {
            return ReduceOutcome::IgnoredStale;
        }
        self.state
            .active_operation_by_target
            .remove(&operation.target);

        match result {
            Ok(message) => {
                if !operation.background {
                    self.state.push_activity(ActivityEntry {
                        timestamp_ms,
                        level: ActivityLevel::Success,
                        message,
                        operation: Some(id),
                    });
                }
            }
            Err(error) => {
                self.state.push_activity(ActivityEntry {
                    timestamp_ms,
                    level: if operation.background {
                        ActivityLevel::Warning
                    } else {
                        ActivityLevel::Error
                    },
                    message: error.summary.clone(),
                    operation: Some(id),
                });
                if !operation.background {
                    self.state.current_error = Some(error);
                }
            }
        }
        ReduceOutcome::Changed
    }

    fn cancel_operations_from_epoch(
        &mut self,
        backend: super::BackendKind,
        epoch: u64,
        timestamp_ms: u64,
    ) {
        let stale = self
            .state
            .operations
            .values()
            .filter(|operation| operation.backend == backend && operation.backend_epoch <= epoch)
            .map(|operation| operation.id)
            .collect::<Vec<_>>();
        for id in stale {
            if let Some(operation) = self.state.operations.remove(&id) {
                self.state
                    .active_operation_by_target
                    .remove(&operation.target);
                self.state.push_activity(ActivityEntry {
                    timestamp_ms,
                    level: ActivityLevel::Warning,
                    message: format!("{backend} restarted; operation {} will be reconciled", id.0),
                    operation: Some(id),
                });
            }
        }
    }

    fn expire_missing(&mut self, now_ms: u64) -> ReduceOutcome {
        let timed_out = self
            .state
            .operations
            .values()
            .filter(|operation| operation.deadline_ms <= now_ms)
            .map(|operation| operation.id)
            .collect::<Vec<_>>();
        let had_timeouts = !timed_out.is_empty();
        for id in timed_out {
            let Some(operation) = self.state.operations.get(&id).cloned() else {
                continue;
            };
            self.finish_operation(
                id,
                Err(UserFacingError {
                    category: ErrorCategory::Timeout,
                    summary: format!("{} did not confirm the requested state", operation.backend),
                    detail: "The daemon accepted the request but did not report the requested final state before the deadline".into(),
                    recovery: vec!["The displayed state was reconciled from the daemon; retry if it is still needed".into(), "Open the activity journal for operation timing".into()],
                    retryable: true,
                    backend: Some(operation.backend),
                    target: Some(operation.target),
                    raw_code: Some("confirmation-timeout".into()),
                }),
                now_ms,
            );
        }
        let wifi_before = self.state.wifi.networks.len();
        for id in expired(&self.wifi_missing_since, now_ms, MISSING_WIFI_RETENTION_MS) {
            self.state.wifi.networks.remove(&id);
            self.wifi_missing_since.remove(&id);
        }
        self.state
            .wifi
            .order
            .retain(|id| self.state.wifi.networks.contains_key(id));
        if self
            .state
            .wifi
            .selected
            .as_ref()
            .is_some_and(|id| !self.state.wifi.networks.contains_key(id))
        {
            self.state.wifi.selected = self.state.wifi.order.first().cloned();
        }

        let bt_before = self.state.bluetooth.devices.len();
        for id in expired(
            &self.bluetooth_missing_since,
            now_ms,
            MISSING_BLUETOOTH_RETENTION_MS,
        ) {
            self.state.bluetooth.devices.remove(&id);
            self.bluetooth_missing_since.remove(&id);
        }
        self.state
            .bluetooth
            .order
            .retain(|id| self.state.bluetooth.devices.contains_key(id));
        if self
            .state
            .bluetooth
            .selected
            .as_ref()
            .is_some_and(|id| !self.state.bluetooth.devices.contains_key(id))
        {
            self.state.bluetooth.selected = self.state.bluetooth.order.first().cloned();
        }

        if had_timeouts
            || wifi_before != self.state.wifi.networks.len()
            || bt_before != self.state.bluetooth.devices.len()
        {
            ReduceOutcome::Changed
        } else {
            ReduceOutcome::Unchanged
        }
    }

    fn confirm_observed_operations(&mut self, backend: super::BackendKind, timestamp_ms: u64) {
        let confirmed = self
            .state
            .operations
            .values()
            .filter(|operation| {
                operation.backend == backend && desired_state_is_observed(&self.state, operation)
            })
            .map(|operation| operation.id)
            .collect::<Vec<_>>();
        for id in confirmed {
            self.finish_operation(
                id,
                Ok("requested state confirmed by the radio service".into()),
                timestamp_ms,
            );
        }
    }
}

fn desired_state_is_observed(state: &AppState, operation: &Operation) -> bool {
    use super::{ConnectionState, DesiredState, EntityId};

    match (&operation.target, operation.desired) {
        (EntityId::Wifi(id), DesiredState::Connected) => state
            .wifi
            .networks
            .get(id)
            .is_some_and(|network| network.state == ConnectionState::Connected),
        (EntityId::Wifi(id), DesiredState::Disconnected) => state
            .wifi
            .networks
            .get(id)
            .is_none_or(|network| network.state == ConnectionState::Disconnected),
        (EntityId::Wifi(id), DesiredState::Forgotten) => state
            .wifi
            .networks
            .get(id)
            .is_none_or(|network| !network.saved),
        (EntityId::Wifi(id), DesiredState::AutoJoinEnabled) => state
            .wifi
            .networks
            .get(id)
            .is_some_and(|network| network.auto_join),
        (EntityId::Wifi(id), DesiredState::AutoJoinDisabled) => state
            .wifi
            .networks
            .get(id)
            .is_some_and(|network| !network.auto_join),
        (EntityId::Bluetooth(id), DesiredState::Connected) => state
            .bluetooth
            .devices
            .get(id)
            .is_some_and(|device| device.state == ConnectionState::Connected),
        (EntityId::Bluetooth(id), DesiredState::Disconnected) => state
            .bluetooth
            .devices
            .get(id)
            .is_none_or(|device| device.state == ConnectionState::Disconnected),
        (EntityId::Bluetooth(id), DesiredState::Forgotten) => state
            .bluetooth
            .devices
            .get(id)
            .is_none_or(|device| !device.paired && !device.trusted),
        (EntityId::Bluetooth(id), DesiredState::Paired) => state
            .bluetooth
            .devices
            .get(id)
            .is_some_and(|device| device.paired),
        (EntityId::Bluetooth(id), DesiredState::Trusted) => state
            .bluetooth
            .devices
            .get(id)
            .is_some_and(|device| device.trusted),
        (EntityId::Bluetooth(id), DesiredState::Untrusted) => state
            .bluetooth
            .devices
            .get(id)
            .is_some_and(|device| !device.trusted),
        (EntityId::Bluetooth(id), DesiredState::Blocked) => state
            .bluetooth
            .devices
            .get(id)
            .is_some_and(|device| device.blocked),
        (EntityId::Bluetooth(id), DesiredState::Unblocked) => state
            .bluetooth
            .devices
            .get(id)
            .is_some_and(|device| !device.blocked),
        (EntityId::WifiInterface(id), DesiredState::Powered) => state
            .wifi
            .interfaces
            .get(id)
            .is_some_and(|interface| interface.powered),
        (EntityId::WifiInterface(id), DesiredState::Unpowered) => state
            .wifi
            .interfaces
            .get(id)
            .is_some_and(|interface| !interface.powered),
        (EntityId::BluetoothAdapter(id), DesiredState::Powered) => state
            .bluetooth
            .adapters
            .get(id)
            .is_some_and(|adapter| adapter.powered),
        (EntityId::BluetoothAdapter(id), DesiredState::Unpowered) => state
            .bluetooth
            .adapters
            .get(id)
            .is_some_and(|adapter| !adapter.powered),
        (EntityId::BluetoothAdapter(id), DesiredState::Scanning) => state
            .bluetooth
            .adapters
            .get(id)
            .is_some_and(|adapter| adapter.scanning),
        (EntityId::BluetoothAdapter(id), DesiredState::Idle) => state
            .bluetooth
            .adapters
            .get(id)
            .is_some_and(|adapter| !adapter.scanning),
        _ => false,
    }
}

fn expired<K: Clone + Ord>(
    missing_since: &BTreeMap<K, u64>,
    now_ms: u64,
    retention_ms: u64,
) -> Vec<K> {
    missing_since
        .iter()
        .filter(|(_, since)| now_ms.saturating_sub(**since) > retention_ms)
        .map(|(id, _)| id.clone())
        .collect()
}

fn preserve_or_first<T: Clone + PartialEq>(selected: Option<T>, order: &[T]) -> Option<T> {
    selected
        .filter(|id| order.contains(id))
        .or_else(|| order.first().cloned())
}

fn stable_wifi_order(
    previous: &[WifiNetworkId],
    networks: &BTreeMap<WifiNetworkId, WifiNetwork>,
) -> Vec<WifiNetworkId> {
    stable_order(previous, networks, |left, right| {
        wifi_section(left)
            .cmp(&wifi_section(right))
            .then_with(|| right.saved.cmp(&left.saved))
    })
}

fn wifi_section(network: &WifiNetwork) -> u8 {
    if !network.present {
        return if network.saved { 4 } else { 5 };
    }
    match network.state {
        super::ConnectionState::Connected => 0,
        super::ConnectionState::Associating
        | super::ConnectionState::Authenticating
        | super::ConnectionState::ObtainingAddress
        | super::ConnectionState::Disconnecting => 1,
        _ if network.saved => 2,
        _ => 3,
    }
}

fn stable_bluetooth_order(
    previous: &[BluetoothDeviceId],
    devices: &BTreeMap<BluetoothDeviceId, BluetoothDevice>,
) -> Vec<BluetoothDeviceId> {
    stable_order(previous, devices, |left, right| {
        bluetooth_section(left).cmp(&bluetooth_section(right))
    })
}

fn bluetooth_section(device: &BluetoothDevice) -> u8 {
    match device.state {
        super::ConnectionState::Connected => 0,
        super::ConnectionState::Associating
        | super::ConnectionState::Authenticating
        | super::ConnectionState::ObtainingAddress
        | super::ConnectionState::Disconnecting => 1,
        _ => match (device.presence, device.paired || device.trusted) {
            (super::Presence::Present, true) => 2,
            (super::Presence::Present, false) => 3,
            (super::Presence::Unknown | super::Presence::OutOfRange, true) => 4,
            (super::Presence::Unknown | super::Presence::OutOfRange, false) => 5,
        },
    }
}

fn stable_order<K, V, F>(previous: &[K], values: &BTreeMap<K, V>, compare: F) -> Vec<K>
where
    K: Clone + Ord,
    F: Fn(&V, &V) -> Ordering,
{
    let previous_positions = previous
        .iter()
        .enumerate()
        .map(|(index, id)| (id, index))
        .collect::<BTreeMap<_, _>>();
    let mut order = values.keys().cloned().collect::<Vec<_>>();
    order.sort_by(|left, right| {
        compare(&values[left], &values[right]).then_with(|| {
            previous_positions
                .get(left)
                .copied()
                .unwrap_or(usize::MAX)
                .cmp(&previous_positions.get(right).copied().unwrap_or(usize::MAX))
                .then_with(|| left.cmp(right))
        })
    });
    order
}

fn format_operation_phase(phase: &OperationPhase) -> String {
    match phase {
        OperationPhase::Queued => "queued".to_owned(),
        OperationPhase::Running(message) | OperationPhase::AwaitingConfirmation(message) => {
            message.clone()
        }
        OperationPhase::Reconciling => "final state unknown; reconciling".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::domain::{
        AdapterId, BackendHealth, BackendKind, BluetoothAdapter, BluetoothDeviceId,
        ConnectionState, Connectivity, EntityId, ErrorCategory, HardwareAddress, InterfaceId,
        Presence, Ssid, WifiInterface, WifiNetworkId, WifiSecurity, ACTIVITY_CAPACITY,
    };

    fn wifi_id(name: &[u8]) -> WifiNetworkId {
        WifiNetworkId {
            interface: InterfaceId("wlan0".into()),
            ssid: Ssid(name.to_vec()),
            security: WifiSecurity::Personal,
        }
    }

    fn wifi_network(name: &[u8], state: ConnectionState, now: u64) -> WifiNetwork {
        let id = wifi_id(name);
        WifiNetwork {
            display_name: id.ssid.display(),
            id,
            signal: 50,
            state,
            connectivity: Connectivity::Unknown,
            saved: false,
            auto_join: false,
            bss_count: 1,
            active_bssid: None,
            present: true,
            last_seen_ms: now,
        }
    }

    fn wifi_event(revision: u64, networks: Vec<WifiNetwork>) -> AppEvent {
        AppEvent::Backend(BackendEvent {
            backend: BackendKind::NetworkManager,
            epoch: 1,
            revision,
            observed_at_ms: revision * 10,
            payload: BackendPayload::WifiSnapshot(WifiSnapshot {
                interfaces: vec![WifiInterface {
                    id: InterfaceId("wlan0".into()),
                    backend: BackendKind::NetworkManager,
                    powered: true,
                    scanning: false,
                    last_scan_ms: None,
                    addresses: Vec::new(),
                    capabilities: BTreeMap::new(),
                }],
                networks,
            }),
        })
    }

    fn bluetooth_id(address: &str) -> BluetoothDeviceId {
        BluetoothDeviceId {
            adapter: AdapterId("hci0".into()),
            address: HardwareAddress(address.into()),
        }
    }

    fn bluetooth_device(address: &str, paired: bool, now: u64) -> BluetoothDevice {
        BluetoothDevice {
            id: bluetooth_id(address),
            name: format!("Keyboard {address}"),
            state: ConnectionState::Disconnected,
            paired,
            trusted: paired,
            blocked: false,
            services_resolved: false,
            rssi: Some(-45),
            battery_percent: None,
            presence: Presence::Present,
            last_seen_ms: now,
        }
    }

    fn bluetooth_event(revision: u64, devices: Vec<BluetoothDevice>) -> AppEvent {
        AppEvent::Backend(BackendEvent {
            backend: BackendKind::Bluez,
            epoch: 1,
            revision,
            observed_at_ms: revision * 10,
            payload: BackendPayload::BluetoothSnapshot(BluetoothSnapshot {
                adapters: vec![BluetoothAdapter {
                    id: AdapterId("hci0".into()),
                    powered: true,
                    scanning: false,
                    capabilities: BTreeMap::new(),
                }],
                devices,
            }),
        })
    }

    #[test]
    fn focus_follows_connected_network_when_it_moves() {
        let mut reducer = Reducer::default();
        reducer.apply(wifi_event(
            1,
            vec![
                wifi_network(b"first", ConnectionState::Disconnected, 10),
                wifi_network(b"second", ConnectionState::Disconnected, 10),
            ],
        ));
        reducer.apply(AppEvent::SelectWifi(Some(wifi_id(b"second"))));
        reducer.apply(wifi_event(
            2,
            vec![
                wifi_network(b"first", ConnectionState::Disconnected, 20),
                wifi_network(b"second", ConnectionState::Connected, 20),
            ],
        ));

        assert_eq!(reducer.state.wifi.order[0], wifi_id(b"second"));
        assert_eq!(reducer.state.wifi.selected, Some(wifi_id(b"second")));
    }

    #[test]
    fn selected_missing_network_is_retained_without_stealing_focus() {
        let mut reducer = Reducer::default();
        reducer.apply(wifi_event(
            1,
            vec![wifi_network(b"unstable", ConnectionState::Disconnected, 10)],
        ));
        reducer.apply(wifi_event(2, vec![]));

        let network = &reducer.state.wifi.networks[&wifi_id(b"unstable")];
        assert!(!network.present);
        assert_eq!(reducer.state.wifi.selected, Some(wifi_id(b"unstable")));

        reducer.apply(AppEvent::Tick(30_021));
        assert!(reducer.state.wifi.networks.is_empty());
        assert_eq!(reducer.state.wifi.selected, None);
    }

    #[test]
    fn saved_wifi_remains_out_of_range_and_below_present_networks() {
        let mut reducer = Reducer::default();
        let mut saved = wifi_network(b"saved", ConnectionState::Disconnected, 10);
        saved.saved = true;
        reducer.apply(wifi_event(
            1,
            vec![
                saved.clone(),
                wifi_network(b"nearby", ConnectionState::Disconnected, 10),
            ],
        ));
        saved.present = false;
        saved.last_seen_ms = 20;
        reducer.apply(wifi_event(
            2,
            vec![
                saved,
                wifi_network(b"nearby", ConnectionState::Disconnected, 20),
            ],
        ));
        reducer.apply(AppEvent::Tick(120_000));

        assert!(!reducer.state.wifi.networks[&wifi_id(b"saved")].present);
        assert_eq!(
            reducer.state.wifi.order,
            vec![wifi_id(b"nearby"), wifi_id(b"saved")]
        );
    }

    #[test]
    fn saved_wifi_the_daemon_no_longer_reports_does_not_linger_forever() {
        let mut reducer = Reducer::default();
        let mut saved = wifi_network(b"saved", ConnectionState::Disconnected, 10);
        saved.saved = true;
        reducer.apply(wifi_event(1, vec![saved]));
        reducer.apply(wifi_event(
            2,
            vec![wifi_network(b"nearby", ConnectionState::Disconnected, 20)],
        ));
        assert!(!reducer.state.wifi.networks[&wifi_id(b"saved")].present);

        reducer.apply(AppEvent::Tick(60_000));
        assert!(!reducer.state.wifi.networks.contains_key(&wifi_id(b"saved")));
    }

    #[test]
    fn a_rekeyed_network_replaces_its_predecessor_instead_of_duplicating_it() {
        let mut reducer = Reducer::default();
        let mut saved = wifi_network(b"home", ConnectionState::Disconnected, 10);
        saved.saved = true;
        reducer.apply(wifi_event(1, vec![saved]));

        let mut rekeyed = wifi_network(b"home", ConnectionState::Disconnected, 20);
        rekeyed.id.security = WifiSecurity::Enterprise;
        let rekeyed_id = rekeyed.id.clone();
        reducer.apply(wifi_event(2, vec![rekeyed]));

        assert_eq!(reducer.state.wifi.order, vec![rekeyed_id]);
        assert!(reducer.state.wifi.networks[&reducer.state.wifi.order[0]].present);
    }

    #[test]
    fn all_recent_missing_networks_are_retained_then_expire() {
        let mut reducer = Reducer::default();
        reducer.apply(wifi_event(
            1,
            vec![
                wifi_network(b"nearby", ConnectionState::Disconnected, 10),
                wifi_network(b"transient", ConnectionState::Disconnected, 10),
            ],
        ));
        reducer.apply(AppEvent::SelectWifi(Some(wifi_id(b"nearby"))));
        reducer.apply(wifi_event(
            2,
            vec![wifi_network(b"nearby", ConnectionState::Disconnected, 20)],
        ));

        assert!(reducer
            .state
            .wifi
            .networks
            .contains_key(&wifi_id(b"transient")));
        reducer.apply(AppEvent::Tick(30_021));
        assert!(!reducer
            .state
            .wifi
            .networks
            .contains_key(&wifi_id(b"transient")));
        assert_eq!(reducer.state.wifi.selected, Some(wifi_id(b"nearby")));
    }

    #[test]
    fn paired_bluetooth_device_remains_out_of_range() {
        let mut reducer = Reducer::default();
        let paired_id = bluetooth_id("00:11:22:33:44:55");
        let nearby_id = bluetooth_id("00:11:22:33:44:66");
        reducer.apply(bluetooth_event(
            1,
            vec![
                bluetooth_device("00:11:22:33:44:55", true, 10),
                bluetooth_device("00:11:22:33:44:66", false, 10),
            ],
        ));
        reducer.apply(bluetooth_event(
            2,
            vec![bluetooth_device("00:11:22:33:44:66", false, 20)],
        ));
        reducer.apply(AppEvent::Tick(50_000));

        assert_eq!(
            reducer.state.bluetooth.devices[&paired_id].presence,
            Presence::OutOfRange
        );
        assert_eq!(
            reducer.state.bluetooth.order,
            vec![nearby_id, paired_id.clone()]
        );

        reducer.apply(AppEvent::Tick(120_000));
        assert!(!reducer.state.bluetooth.devices.contains_key(&paired_id));
    }

    #[test]
    fn a_device_that_returns_at_a_new_address_is_not_listed_twice() {
        let mut reducer = Reducer::default();
        let previous_id = bluetooth_id("7D:A9:2A:BA:04:FC");
        let mut renamed = bluetooth_device("7D:A9:2A:BA:04:FC", true, 10);
        renamed.name = "Headphones".into();
        reducer.apply(bluetooth_event(1, vec![renamed]));

        let resolved_id = bluetooth_id("88:0E:85:9C:A7:BC");
        let mut resolved = bluetooth_device("88:0E:85:9C:A7:BC", true, 20);
        resolved.name = "Headphones".into();
        reducer.apply(bluetooth_event(2, vec![resolved]));

        assert!(!reducer.state.bluetooth.devices.contains_key(&previous_id));
        assert_eq!(reducer.state.bluetooth.order, vec![resolved_id]);
    }

    #[test]
    fn bluetooth_order_prefers_connectable_then_remembered_devices() {
        let mut reducer = Reducer::default();
        let remembered_nearby = bluetooth_id("00:11:22:33:44:01");
        let new_nearby = bluetooth_id("00:11:22:33:44:02");
        let remembered_out_of_range = bluetooth_id("00:11:22:33:44:03");
        let new_unknown = bluetooth_id("00:11:22:33:44:04");
        let new_out_of_range = bluetooth_id("00:11:22:33:44:05");

        reducer.apply(bluetooth_event(
            1,
            vec![
                bluetooth_device("00:11:22:33:44:01", true, 10),
                bluetooth_device("00:11:22:33:44:03", true, 10),
            ],
        ));

        let mut unknown = bluetooth_device("00:11:22:33:44:04", false, 20);
        unknown.presence = Presence::Unknown;
        let mut out_of_range = bluetooth_device("00:11:22:33:44:05", false, 20);
        out_of_range.presence = Presence::OutOfRange;
        reducer.apply(bluetooth_event(
            2,
            vec![
                bluetooth_device("00:11:22:33:44:01", true, 20),
                bluetooth_device("00:11:22:33:44:02", false, 20),
                unknown,
                out_of_range,
            ],
        ));

        assert_eq!(
            reducer.state.bluetooth.order,
            vec![
                remembered_nearby,
                new_nearby,
                remembered_out_of_range,
                new_unknown,
                new_out_of_range,
            ]
        );
    }

    #[test]
    fn forget_operation_allows_a_saved_item_to_leave_the_model() {
        let mut reducer = Reducer::default();
        let mut saved = wifi_network(b"saved", ConnectionState::Disconnected, 10);
        saved.saved = true;
        reducer.apply(wifi_event(1, vec![saved]));
        reducer.apply(AppEvent::OperationStarted(Operation {
            id: OperationId(91),
            backend: BackendKind::NetworkManager,
            target: EntityId::Wifi(wifi_id(b"saved")),
            desired: super::super::DesiredState::Forgotten,
            phase: OperationPhase::AwaitingConfirmation("forgetting".into()),
            started_at_ms: 11,
            deadline_ms: 1_000,
            backend_epoch: 1,
            background: false,
        }));

        reducer.apply(wifi_event(2, vec![]));

        assert!(!reducer.state.wifi.networks.contains_key(&wifi_id(b"saved")));
        assert!(reducer.state.operations.is_empty());
    }

    #[test]
    fn auto_join_change_finishes_only_after_the_snapshot_confirms_it() {
        let mut reducer = Reducer::default();
        let mut saved = wifi_network(b"saved", ConnectionState::Disconnected, 10);
        saved.saved = true;
        reducer.apply(wifi_event(1, vec![saved.clone()]));
        reducer.apply(AppEvent::OperationStarted(Operation {
            id: OperationId(92),
            backend: BackendKind::NetworkManager,
            target: EntityId::Wifi(wifi_id(b"saved")),
            desired: super::super::DesiredState::AutoJoinEnabled,
            phase: OperationPhase::AwaitingConfirmation("updating".into()),
            started_at_ms: 11,
            deadline_ms: 1_000,
            backend_epoch: 1,
            background: false,
        }));
        assert_eq!(reducer.state.operations.len(), 1);

        saved.auto_join = true;
        saved.last_seen_ms = 20;
        reducer.apply(wifi_event(2, vec![saved]));
        assert!(reducer.state.operations.is_empty());
    }

    #[test]
    fn bluetooth_property_change_waits_for_observed_state() {
        let mut reducer = Reducer::default();
        let device = bluetooth_device("AA:BB:CC:DD:EE:FF", true, 10);
        let id = device.id.clone();
        reducer.apply(bluetooth_event(1, vec![device.clone()]));
        reducer.apply(AppEvent::OperationStarted(Operation {
            id: OperationId(93),
            backend: BackendKind::Bluez,
            target: EntityId::Bluetooth(id),
            desired: super::super::DesiredState::Blocked,
            phase: OperationPhase::AwaitingConfirmation("blocking".into()),
            started_at_ms: 11,
            deadline_ms: 1_000,
            backend_epoch: 1,
            background: false,
        }));

        reducer.apply(bluetooth_event(2, vec![device.clone()]));
        assert_eq!(reducer.state.operations.len(), 1);

        let mut blocked = device;
        blocked.blocked = true;
        reducer.apply(bluetooth_event(3, vec![blocked]));
        assert!(reducer.state.operations.is_empty());
    }

    #[test]
    fn stale_backend_event_cannot_replace_newer_state() {
        let mut reducer = Reducer::default();
        let mut strong = wifi_network(b"network", ConnectionState::Connected, 20);
        strong.signal = 90;
        reducer.apply(wifi_event(2, vec![strong]));

        let mut stale = wifi_network(b"network", ConnectionState::Disconnected, 10);
        stale.signal = 1;
        assert_eq!(
            reducer.apply(wifi_event(1, vec![stale])),
            ReduceOutcome::IgnoredStale
        );
        assert_eq!(reducer.state.wifi.networks[&wifi_id(b"network")].signal, 90);
    }

    #[test]
    fn superseded_operation_completion_is_ignored() {
        let mut reducer = Reducer::default();
        let target = EntityId::Wifi(wifi_id(b"network"));
        for id in [1, 2] {
            reducer.apply(AppEvent::OperationStarted(Operation {
                id: OperationId(id),
                backend: BackendKind::NetworkManager,
                target: target.clone(),
                desired: super::super::DesiredState::Connected,
                phase: OperationPhase::Queued,
                started_at_ms: id,
                deadline_ms: 100,
                backend_epoch: 1,
                background: false,
            }));
        }

        assert_eq!(
            reducer.apply(AppEvent::OperationSucceeded {
                id: OperationId(1),
                message: "old success".into(),
                timestamp_ms: 3,
            }),
            ReduceOutcome::IgnoredStale
        );
        assert_eq!(
            reducer.state.active_operation(&target).unwrap().id,
            OperationId(2)
        );
    }

    #[test]
    fn errors_are_persistent_and_activity_is_bounded() {
        let mut reducer = Reducer::default();
        let target = EntityId::Wifi(wifi_id(b"network"));
        for id in 0..=ACTIVITY_CAPACITY as u64 {
            reducer.apply(AppEvent::OperationStarted(Operation {
                id: OperationId(id),
                backend: BackendKind::NetworkManager,
                target: EntityId::Wifi(wifi_id(format!("network-{id}").as_bytes())),
                desired: super::super::DesiredState::Connected,
                phase: OperationPhase::Queued,
                started_at_ms: id,
                deadline_ms: id + 100,
                backend_epoch: 1,
                background: false,
            }));
        }
        assert_eq!(reducer.state.activity.len(), ACTIVITY_CAPACITY);

        let id = OperationId(9_999);
        reducer.apply(AppEvent::OperationStarted(Operation {
            id,
            backend: BackendKind::NetworkManager,
            target,
            desired: super::super::DesiredState::Connected,
            phase: OperationPhase::Queued,
            started_at_ms: 1,
            deadline_ms: 2,
            backend_epoch: 1,
            background: false,
        }));
        reducer.apply(AppEvent::OperationFailed {
            id,
            error: UserFacingError {
                category: ErrorCategory::Authentication,
                summary: "Password rejected".into(),
                detail: "The access point rejected the supplied secret".into(),
                recovery: vec!["Retry with a new password".into()],
                retryable: true,
                backend: Some(BackendKind::NetworkManager),
                target: None,
                raw_code: Some("no-secrets".into()),
            },
            timestamp_ms: 2,
        });
        assert_eq!(
            reducer.state.current_error.as_ref().unwrap().summary,
            "Password rejected"
        );
    }

    #[test]
    fn background_discovery_failure_warns_without_interrupting_the_user() {
        let mut reducer = Reducer::default();
        reducer.apply(AppEvent::OperationStarted(Operation {
            id: OperationId(10_000),
            backend: BackendKind::NetworkManager,
            target: EntityId::WifiInterface(InterfaceId("wlan0".into())),
            desired: super::super::DesiredState::Scanning,
            phase: OperationPhase::Queued,
            started_at_ms: 1,
            deadline_ms: 100,
            backend_epoch: 1,
            background: true,
        }));
        assert!(reducer.state.activity.is_empty());

        reducer.apply(AppEvent::OperationFailed {
            id: OperationId(10_000),
            error: UserFacingError {
                category: ErrorCategory::Busy,
                summary: "scan temporarily refused".into(),
                detail: "the daemon is busy".into(),
                recovery: Vec::new(),
                retryable: true,
                backend: Some(BackendKind::NetworkManager),
                target: None,
                raw_code: None,
            },
            timestamp_ms: 2,
        });

        assert!(reducer.state.current_error.is_none());
        assert_eq!(
            reducer.state.activity.back().unwrap().level,
            ActivityLevel::Warning
        );
    }

    #[test]
    fn raw_ssids_have_unambiguous_display_text() {
        assert_eq!(
            Ssid(vec![b'a', 0, b'\\', 0xff]).display(),
            "a\\x00\\\\\\xff"
        );
        assert_eq!(Ssid("café".as_bytes().to_vec()).display(), "café");
    }

    #[test]
    fn bluetooth_snapshot_selects_first_device() {
        let id = BluetoothDeviceId {
            adapter: AdapterId("hci0".into()),
            address: HardwareAddress("00:11:22:33:44:55".into()),
        };
        let mut reducer = Reducer::default();
        reducer.apply(AppEvent::Backend(BackendEvent {
            backend: BackendKind::Bluez,
            epoch: 1,
            revision: 1,
            observed_at_ms: 1,
            payload: BackendPayload::BluetoothSnapshot(BluetoothSnapshot {
                adapters: vec![BluetoothAdapter {
                    id: AdapterId("hci0".into()),
                    powered: true,
                    scanning: false,
                    capabilities: BTreeMap::new(),
                }],
                devices: vec![BluetoothDevice {
                    id: id.clone(),
                    name: "Keyboard".into(),
                    state: ConnectionState::Disconnected,
                    paired: true,
                    trusted: true,
                    blocked: false,
                    services_resolved: true,
                    rssi: None,
                    battery_percent: None,
                    presence: Presence::Present,
                    last_seen_ms: 1,
                }],
            }),
        }));
        assert_eq!(reducer.state.bluetooth.selected, Some(id));
    }

    #[test]
    fn newer_epoch_accepts_revision_reset() {
        let mut reducer = Reducer::default();
        reducer.apply(wifi_event(
            20,
            vec![wifi_network(b"old", ConnectionState::Connected, 20)],
        ));
        let outcome = reducer.apply(AppEvent::Backend(BackendEvent {
            backend: BackendKind::NetworkManager,
            epoch: 2,
            revision: 1,
            observed_at_ms: 30,
            payload: BackendPayload::Health {
                health: BackendHealth::Reconnecting,
                detail: Some("daemon owner changed".into()),
            },
        }));
        assert_eq!(outcome, ReduceOutcome::Changed);
        assert_eq!(
            reducer.state.backends[&BackendKind::NetworkManager].epoch,
            2
        );
    }

    #[test]
    fn accepted_operation_finishes_only_after_backend_confirmation() {
        let mut reducer = Reducer::default();
        let target = EntityId::Wifi(wifi_id(b"home"));
        reducer.apply(wifi_event(
            1,
            vec![wifi_network(b"home", ConnectionState::Disconnected, 10)],
        ));
        reducer.apply(AppEvent::OperationStarted(Operation {
            id: OperationId(41),
            backend: BackendKind::NetworkManager,
            target: target.clone(),
            desired: super::super::DesiredState::Connected,
            phase: OperationPhase::Queued,
            started_at_ms: 10,
            deadline_ms: 1_000,
            backend_epoch: 1,
            background: false,
        }));
        reducer.apply(AppEvent::OperationProgress {
            id: OperationId(41),
            phase: OperationPhase::AwaitingConfirmation("accepted".into()),
            timestamp_ms: 11,
        });
        assert!(reducer.state.active_operation(&target).is_some());

        reducer.apply(wifi_event(
            2,
            vec![wifi_network(b"home", ConnectionState::Connected, 20)],
        ));
        assert!(reducer.state.active_operation(&target).is_none());
        assert!(reducer
            .state
            .activity
            .back()
            .unwrap()
            .message
            .contains("confirmed"));
    }

    #[test]
    fn unconfirmed_operation_times_out_without_faking_target_state() {
        let mut reducer = Reducer::default();
        let target = EntityId::Wifi(wifi_id(b"home"));
        reducer.apply(wifi_event(
            1,
            vec![wifi_network(b"home", ConnectionState::Disconnected, 10)],
        ));
        reducer.apply(AppEvent::OperationStarted(Operation {
            id: OperationId(42),
            backend: BackendKind::NetworkManager,
            target: target.clone(),
            desired: super::super::DesiredState::Connected,
            phase: OperationPhase::Queued,
            started_at_ms: 10,
            deadline_ms: 20,
            backend_epoch: 1,
            background: false,
        }));
        reducer.apply(AppEvent::Tick(20));

        assert!(reducer.state.active_operation(&target).is_none());
        assert_eq!(
            reducer.state.wifi.networks[&wifi_id(b"home")].state,
            ConnectionState::Disconnected
        );
        assert_eq!(
            reducer.state.current_error.as_ref().unwrap().category,
            ErrorCategory::Timeout
        );
    }

    proptest! {
        #[test]
        fn event_order_never_allows_a_lower_revision_to_win(
            revisions in prop::collection::vec(1_u64..100, 1..100)
        ) {
            let mut reducer = Reducer::default();
            for revision in &revisions {
                let mut network = wifi_network(b"network", ConnectionState::Disconnected, *revision);
                network.signal = *revision as u8;
                reducer.apply(wifi_event(*revision, vec![network]));
            }
            let maximum = *revisions.iter().max().unwrap();
            prop_assert_eq!(
                reducer.state.wifi.networks[&wifi_id(b"network")].signal,
                maximum as u8
            );
        }
    }
}
