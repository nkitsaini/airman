use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use tokio::sync::{broadcast, mpsc, Mutex};

use crate::{
    app::Intent,
    backend::{
        bluez::BluezBackend, connman::ConnManBackend, iwd::IwdBackend,
        network_manager::NetworkManagerBackend, wpa_networkd::WpaNetworkdBackend, BackendAction,
        BackendCommand, BackendDiagnostics, ProfileUpdate, RadioBackend, Secret,
    },
    cli::BackendChoice,
    config::Settings,
    domain::{
        AppEvent, AppState, BackendEvent, BackendHealth, BackendKind, BackendPayload, DesiredState,
        EntityId, ErrorCategory, Operation, OperationId, OperationPhase, UserFacingError,
        WifiNetworkId,
    },
};

pub struct Runtime {
    backends: BTreeMap<BackendKind, Arc<dyn RadioBackend>>,
    updates_tx: mpsc::Sender<AppEvent>,
    updates_rx: mpsc::Receiver<AppEvent>,
    next_operation: AtomicU64,
    wifi_selection: Option<Arc<Mutex<WifiSelection>>>,
    startup_diagnostics: Vec<BackendDiagnostics>,
}

#[derive(Default)]
struct WifiSelection {
    selected: Option<BackendKind>,
    usable: BTreeMap<BackendKind, bool>,
    latest: BTreeMap<BackendKind, BackendEvent>,
}

impl Runtime {
    pub async fn start(settings: &Settings) -> Self {
        let (updates_tx, updates_rx) = mpsc::channel(512);
        let mut runtime = Self {
            backends: BTreeMap::new(),
            updates_tx,
            updates_rx,
            next_operation: AtomicU64::new(1),
            wifi_selection: (settings.backend == BackendChoice::Auto)
                .then(|| Arc::new(Mutex::new(WifiSelection::default()))),
            startup_diagnostics: Vec::new(),
        };
        let connection = match zbus::Connection::system().await {
            Ok(connection) => connection,
            Err(error) => {
                let detail = format!("could not connect to the system D-Bus: {error}");
                runtime.startup_diagnostics = [
                    BackendKind::NetworkManager,
                    BackendKind::Iwd,
                    BackendKind::WpaNetworkd,
                    BackendKind::ConnMan,
                    BackendKind::Bluez,
                ]
                .into_iter()
                .map(|backend| BackendDiagnostics {
                    backend,
                    owner: None,
                    version: None,
                    properties: BTreeMap::new(),
                    warnings: vec![detail.clone()],
                })
                .collect();
                runtime.queue_health(
                    BackendKind::NetworkManager,
                    BackendHealth::Unavailable,
                    format!("could not connect to the system D-Bus: {error}"),
                );
                runtime.queue_health(
                    BackendKind::Bluez,
                    BackendHealth::Unavailable,
                    format!("could not connect to the system D-Bus: {error}"),
                );
                return runtime;
            }
        };

        match settings.backend {
            BackendChoice::Auto => {
                let network_manager =
                    NetworkManagerBackend::new(connection.clone(), settings.wifi_interface.clone())
                        .await;
                runtime.add_wifi_candidate(network_manager).await;
                let connman =
                    ConnManBackend::new(connection.clone(), settings.wifi_interface.clone()).await;
                runtime.add_wifi_candidate(connman).await;
                let iwd =
                    IwdBackend::new(connection.clone(), settings.wifi_interface.clone()).await;
                runtime.add_wifi_candidate(iwd).await;
                let wpa =
                    WpaNetworkdBackend::new(connection.clone(), settings.wifi_interface.clone())
                        .await;
                runtime.add_wifi_candidate(wpa).await;
            }
            BackendChoice::NetworkManager => {
                let backend =
                    NetworkManagerBackend::new(connection.clone(), settings.wifi_interface.clone())
                        .await;
                runtime.add_backend(backend).await;
            }
            BackendChoice::Iwd => {
                let backend =
                    IwdBackend::new(connection.clone(), settings.wifi_interface.clone()).await;
                runtime.add_backend(backend).await;
            }
            BackendChoice::WpaNetworkd => {
                let backend =
                    WpaNetworkdBackend::new(connection.clone(), settings.wifi_interface.clone())
                        .await;
                runtime.add_backend(backend).await;
            }
            BackendChoice::ConnMan => {
                let backend =
                    ConnManBackend::new(connection.clone(), settings.wifi_interface.clone()).await;
                runtime.add_backend(backend).await;
            }
        }

        let bluez = BluezBackend::new(connection, settings.bluetooth_adapter.clone()).await;
        runtime.add_backend(bluez).await;
        runtime
    }

    async fn add_backend<B>(&mut self, backend: Arc<B>)
    where
        B: RadioBackend + 'static,
    {
        let backend: Arc<dyn RadioBackend> = backend;
        let kind = backend.kind();
        let probe = backend.probe().await;
        let receiver = backend.subscribe();
        self.forward_backend_events(backend.clone(), receiver);
        self.backends.insert(kind, backend.clone());
        if probe.status != crate::backend::ProbeStatus::Available {
            self.queue_health(
                kind,
                BackendHealth::Unavailable,
                probe
                    .detail
                    .unwrap_or_else(|| format!("{kind} is not running")),
            );
            return;
        }

        // Subscribe before the initial snapshot. A signal that lands during the
        // snapshot will therefore be applied at a later revision rather than lost.
        match backend.snapshot().await {
            Ok(event) => {
                let _ = self.updates_tx.send(AppEvent::Backend(event)).await;
            }
            Err(error) => self.queue_health(kind, BackendHealth::Degraded, error.detail),
        }
    }

    async fn add_wifi_candidate<B>(&mut self, backend: Arc<B>)
    where
        B: RadioBackend + 'static,
    {
        let backend: Arc<dyn RadioBackend> = backend;
        let kind = backend.kind();
        let probe = backend.probe().await;
        let receiver = backend.subscribe();
        self.forward_wifi_candidate(backend.clone(), receiver);
        self.backends.insert(kind, backend.clone());

        if probe.status == crate::backend::ProbeStatus::Available {
            match backend.snapshot().await {
                Ok(event) => self.process_wifi_candidate(event).await,
                Err(error) => {
                    self.process_wifi_candidate(health_event(
                        kind,
                        BackendHealth::Degraded,
                        error.detail,
                    ))
                    .await;
                }
            }
        } else {
            self.process_wifi_candidate(health_event(
                kind,
                BackendHealth::Unavailable,
                probe
                    .detail
                    .unwrap_or_else(|| format!("{kind} is not running")),
            ))
            .await;
        }
    }

    fn forward_wifi_candidate(
        &self,
        backend: Arc<dyn RadioBackend>,
        mut receiver: broadcast::Receiver<BackendEvent>,
    ) {
        let updates = self.updates_tx.clone();
        let selection = self
            .wifi_selection
            .as_ref()
            .expect("auto selection exists")
            .clone();
        tokio::spawn(async move {
            loop {
                let event = match receiver.recv().await {
                    Ok(event) => event,
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(backend = %backend.kind(), skipped, "candidate event stream lagged; forcing snapshot");
                        match backend.snapshot().await {
                            Ok(event) => event,
                            Err(error) => {
                                tracing::error!(backend = %backend.kind(), %error, "candidate recovery snapshot failed");
                                continue;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                };
                let forward = update_wifi_selection(&selection, event).await;
                for event in forward {
                    if updates.send(AppEvent::Backend(event)).await.is_err() {
                        return;
                    }
                }
            }
        });
    }

    async fn process_wifi_candidate(&self, event: BackendEvent) {
        let Some(selection) = &self.wifi_selection else {
            return;
        };
        for event in update_wifi_selection(selection, event).await {
            let _ = self.updates_tx.send(AppEvent::Backend(event)).await;
        }
    }

    fn forward_backend_events(
        &self,
        backend: Arc<dyn RadioBackend>,
        mut receiver: broadcast::Receiver<BackendEvent>,
    ) {
        let updates = self.updates_tx.clone();
        tokio::spawn(async move {
            loop {
                match receiver.recv().await {
                    Ok(event) => {
                        if updates.send(AppEvent::Backend(event)).await.is_err() {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(backend = %backend.kind(), skipped, "backend event consumer lagged; forcing authoritative snapshot");
                        match backend.snapshot().await {
                            Ok(event) => {
                                if updates.send(AppEvent::Backend(event)).await.is_err() {
                                    return;
                                }
                            }
                            Err(error) => {
                                tracing::error!(backend = %backend.kind(), %error, "recovery snapshot failed")
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        });
    }

    pub async fn next_event(&mut self) -> Option<AppEvent> {
        self.updates_rx.recv().await
    }

    pub fn dispatch(&self, intent: Intent, state: &AppState, now_ms: u64) -> Option<OperationId> {
        let background = matches!(
            intent,
            Intent::AutomaticWifiScan
                | Intent::EnsureBluetoothDiscovery
                | Intent::ReleaseBluetoothDiscovery
        );
        if let Intent::Cancel(operation_id) = intent {
            if let Some(operation) = state.operations.get(&operation_id) {
                if matches!(
                    operation.desired,
                    DesiredState::Connected | DesiredState::Disconnected
                ) {
                    return self.dispatch(
                        Intent::SetConnection {
                            target: operation.target.clone(),
                            desired: if operation.desired == DesiredState::Connected {
                                DesiredState::Disconnected
                            } else {
                                DesiredState::Connected
                            },
                            credential: None,
                        },
                        state,
                        now_ms,
                    );
                } else if let Some(backend) = self.backends.get(&operation.backend).cloned() {
                    tokio::spawn(async move {
                        if let Err(error) = backend.cancel(operation_id).await {
                            tracing::warn!(%error, operation = operation_id.0, "backend could not cancel operation");
                        }
                    });
                }
            }
            return None;
        }

        let (backend_kind, target, desired, action, credential) = route_intent(intent, state)?;
        let id = OperationId(self.next_operation.fetch_add(1, Ordering::Relaxed));
        let backend_epoch = state
            .backends
            .get(&backend_kind)
            .map_or(1, |backend| backend.epoch);
        let operation = Operation {
            id,
            backend: backend_kind,
            target: target.clone(),
            desired,
            phase: OperationPhase::Queued,
            started_at_ms: now_ms,
            deadline_ms: now_ms + timeout_ms(desired),
            backend_epoch,
            background,
        };
        if let Err(error) = self
            .updates_tx
            .try_send(AppEvent::OperationStarted(operation))
        {
            tracing::error!(%error, operation = id.0, "runtime event queue is full; operation was not started");
            return None;
        }

        let Some(backend) = self.backends.get(&backend_kind).cloned() else {
            let _ = self.updates_tx.try_send(AppEvent::OperationFailed {
                id,
                error: unavailable_error(backend_kind, target),
                timestamp_ms: now_ms,
            });
            return Some(id);
        };
        let updates = self.updates_tx.clone();
        tokio::spawn(async move {
            let immediate_success = matches!(action, BackendAction::Scan | BackendAction::StopScan);
            let command = BackendCommand {
                operation_id: id,
                target: target.clone(),
                desired,
                action,
                credential,
                remember_credential: true,
            };
            match backend.execute(command).await {
                Ok(_acceptance) if immediate_success => {
                    let _ = updates
                        .send(AppEvent::OperationSucceeded {
                            id,
                            message: "scan request accepted".into(),
                            timestamp_ms: super::backend::dbus::monotonic_ms(),
                        })
                        .await;
                }
                Ok(acceptance) => {
                    let _ = updates
                        .send(AppEvent::OperationProgress {
                            id,
                            phase: acceptance.phase,
                            timestamp_ms: super::backend::dbus::monotonic_ms(),
                        })
                        .await;
                }
                Err(error) => {
                    let _ = updates
                        .send(AppEvent::OperationFailed {
                            id,
                            error: error.into_user_error(backend_kind, Some(target)),
                            timestamp_ms: super::backend::dbus::monotonic_ms(),
                        })
                        .await;
                }
            }
        });
        Some(id)
    }

    pub async fn diagnostics(&self) -> Vec<BackendDiagnostics> {
        let mut diagnostics = self.startup_diagnostics.clone();
        diagnostics.reserve(self.backends.len());
        for backend in self.backends.values() {
            diagnostics.push(backend.diagnostics().await);
        }
        diagnostics
    }

    pub async fn wifi_secret(
        &self,
        id: &WifiNetworkId,
        state: &AppState,
    ) -> Result<Secret, UserFacingError> {
        let target = EntityId::Wifi(id.clone());
        let backend_kind = backend_for_target(&target, state)
            .ok_or_else(|| unavailable_error(BackendKind::NetworkManager, target.clone()))?;
        let backend = self
            .backends
            .get(&backend_kind)
            .ok_or_else(|| unavailable_error(backend_kind, target.clone()))?;
        match tokio::time::timeout(std::time::Duration::from_secs(5), backend.wifi_secret(id)).await
        {
            Ok(Ok(secret)) => Ok(secret),
            Ok(Err(error)) => Err(error.into_user_error(backend_kind, Some(target))),
            Err(_) => Err(UserFacingError {
                category: ErrorCategory::Timeout,
                summary: "Reading the saved Wi-Fi password timed out".into(),
                detail: "The Wi-Fi service did not answer the secret request within five seconds"
                    .into(),
                recovery: vec!["Check the desktop secret agent and retry".into()],
                retryable: true,
                backend: Some(backend_kind),
                target: Some(target),
                raw_code: Some("secret-read-timeout".into()),
            }),
        }
    }

    fn queue_health(&self, backend: BackendKind, health: BackendHealth, detail: String) {
        let _ = self.updates_tx.try_send(AppEvent::Backend(BackendEvent {
            backend,
            epoch: 1,
            revision: 1,
            observed_at_ms: super::backend::dbus::monotonic_ms(),
            payload: BackendPayload::Health {
                health,
                detail: Some(detail),
            },
        }));
    }
}

fn route_intent(
    intent: Intent,
    state: &AppState,
) -> Option<(
    BackendKind,
    EntityId,
    DesiredState,
    BackendAction,
    Option<crate::backend::Secret>,
)> {
    match intent {
        Intent::SetConnection {
            target,
            desired,
            credential,
        } => {
            let backend = backend_for_target(&target, state)?;
            let action = if desired == DesiredState::Connected {
                BackendAction::Connect
            } else {
                BackendAction::Disconnect
            };
            Some((backend, target, desired, action, credential))
        }
        Intent::ScanWifi | Intent::AutomaticWifiScan => {
            let interface = state.wifi.selected_interface.as_ref()?;
            let info = state.wifi.interfaces.get(interface)?;
            Some((
                info.backend,
                EntityId::WifiInterface(interface.clone()),
                DesiredState::Scanning,
                BackendAction::Scan,
                None,
            ))
        }
        Intent::ToggleBluetoothDiscovery => {
            let adapter = state.bluetooth.selected_adapter.as_ref()?;
            let info = state.bluetooth.adapters.get(adapter)?;
            let (desired, action) = if info.scanning {
                (DesiredState::Idle, BackendAction::StopScan)
            } else {
                (DesiredState::Scanning, BackendAction::Scan)
            };
            Some((
                BackendKind::Bluez,
                EntityId::BluetoothAdapter(adapter.clone()),
                desired,
                action,
                None,
            ))
        }
        Intent::StartBluetoothDiscovery | Intent::EnsureBluetoothDiscovery => {
            let adapter = state.bluetooth.selected_adapter.as_ref()?;
            state.bluetooth.adapters.get(adapter)?;
            Some((
                BackendKind::Bluez,
                EntityId::BluetoothAdapter(adapter.clone()),
                DesiredState::Scanning,
                BackendAction::Scan,
                None,
            ))
        }
        Intent::StopBluetoothDiscovery | Intent::ReleaseBluetoothDiscovery => {
            let adapter = state.bluetooth.selected_adapter.as_ref()?;
            state.bluetooth.adapters.get(adapter)?;
            Some((
                BackendKind::Bluez,
                EntityId::BluetoothAdapter(adapter.clone()),
                DesiredState::Idle,
                BackendAction::StopScan,
                None,
            ))
        }
        Intent::ToggleWifiRadio => {
            let interface = state.wifi.selected_interface.as_ref()?;
            let info = state.wifi.interfaces.get(interface)?;
            let powered = !info.powered;
            Some((
                info.backend,
                EntityId::WifiInterface(interface.clone()),
                if powered {
                    DesiredState::Powered
                } else {
                    DesiredState::Unpowered
                },
                BackendAction::SetPowered(powered),
                None,
            ))
        }
        Intent::ToggleBluetoothRadio => {
            let adapter = state.bluetooth.selected_adapter.as_ref()?;
            let info = state.bluetooth.adapters.get(adapter)?;
            let powered = !info.powered;
            Some((
                BackendKind::Bluez,
                EntityId::BluetoothAdapter(adapter.clone()),
                if powered {
                    DesiredState::Powered
                } else {
                    DesiredState::Unpowered
                },
                BackendAction::SetPowered(powered),
                None,
            ))
        }
        Intent::Forget(target) => {
            let backend = backend_for_target(&target, state)?;
            Some((
                backend,
                target,
                DesiredState::Forgotten,
                BackendAction::Forget,
                None,
            ))
        }
        Intent::SetWifiAutoJoin { id, enabled } => {
            let target = EntityId::Wifi(id);
            let backend = backend_for_target(&target, state)?;
            Some((
                backend,
                target,
                if enabled {
                    DesiredState::AutoJoinEnabled
                } else {
                    DesiredState::AutoJoinDisabled
                },
                BackendAction::UpdateProfile(ProfileUpdate {
                    auto_join: Some(enabled),
                    ..ProfileUpdate::default()
                }),
                None,
            ))
        }
        Intent::PairBluetooth(id) => Some((
            BackendKind::Bluez,
            EntityId::Bluetooth(id),
            DesiredState::Paired,
            BackendAction::Pair,
            None,
        )),
        Intent::SetBluetoothTrusted { id, trusted } => Some((
            BackendKind::Bluez,
            EntityId::Bluetooth(id),
            if trusted {
                DesiredState::Trusted
            } else {
                DesiredState::Untrusted
            },
            BackendAction::SetTrusted(trusted),
            None,
        )),
        Intent::SetBluetoothBlocked { id, blocked } => Some((
            BackendKind::Bluez,
            EntityId::Bluetooth(id),
            if blocked {
                DesiredState::Blocked
            } else {
                DesiredState::Unblocked
            },
            BackendAction::SetBlocked(blocked),
            None,
        )),
        Intent::Quit
        | Intent::Cancel(_)
        | Intent::OpenDiagnostics
        | Intent::CycleBluetoothDiscoveryMode
        | Intent::ShowWifiSecret { .. } => None,
    }
}

fn backend_for_target(target: &EntityId, state: &AppState) -> Option<BackendKind> {
    match target {
        EntityId::Wifi(id) => state
            .wifi
            .interfaces
            .get(&id.interface)
            .map(|interface| interface.backend),
        EntityId::WifiInterface(id) => state
            .wifi
            .interfaces
            .get(id)
            .map(|interface| interface.backend),
        EntityId::Bluetooth(_) | EntityId::BluetoothAdapter(_) => Some(BackendKind::Bluez),
    }
}

fn timeout_ms(desired: DesiredState) -> u64 {
    match desired {
        DesiredState::Connected => 45_000,
        DesiredState::Scanning => 20_000,
        _ => 15_000,
    }
}

fn unavailable_error(backend: BackendKind, target: EntityId) -> UserFacingError {
    UserFacingError {
        category: ErrorCategory::ServiceUnavailable,
        summary: format!("{backend} is not available"),
        detail: "The selected backend has no active D-Bus owner".into(),
        recovery: vec![
            format!("Start {backend}, then retry"),
            "Open diagnostics for ownership details".into(),
        ],
        retryable: true,
        backend: Some(backend),
        target: Some(target),
        raw_code: Some("backend-unavailable".into()),
    }
}

async fn update_wifi_selection(
    selection: &Mutex<WifiSelection>,
    event: BackendEvent,
) -> Vec<BackendEvent> {
    let mut selection = selection.lock().await;
    let kind = event.backend;
    let is_health = matches!(event.payload, BackendPayload::Health { .. });
    match &event.payload {
        BackendPayload::WifiSnapshot(snapshot) => {
            selection
                .usable
                .insert(kind, !snapshot.interfaces.is_empty());
            selection.latest.insert(kind, event.clone());
        }
        BackendPayload::Health { health, .. } => match health {
            BackendHealth::Unavailable | BackendHealth::Initializing => {
                selection.usable.insert(kind, false);
            }
            BackendHealth::Ready => {
                selection.usable.insert(kind, true);
            }
            BackendHealth::Degraded | BackendHealth::Reconnecting => {}
        },
        BackendPayload::BluetoothSnapshot(_) => return Vec::new(),
    }

    let previous = selection.selected;
    selection.selected = wifi_priority()
        .into_iter()
        .find(|candidate| selection.usable.get(candidate).copied().unwrap_or(false));
    let selected = selection.selected;
    let switched = previous != selected;
    let mut forward = Vec::with_capacity(2);
    if is_health || selected == Some(kind) {
        forward.push(event);
    }
    if switched && selected != Some(kind) {
        if let Some(snapshot) = selected
            .and_then(|kind| selection.latest.get(&kind))
            .cloned()
        {
            forward.push(snapshot);
        }
    }
    forward
}

fn wifi_priority() -> [BackendKind; 4] {
    [
        BackendKind::NetworkManager,
        BackendKind::ConnMan,
        BackendKind::Iwd,
        BackendKind::WpaNetworkd,
    ]
}

fn health_event(kind: BackendKind, health: BackendHealth, detail: String) -> BackendEvent {
    BackendEvent {
        backend: kind,
        epoch: 1,
        revision: 1,
        observed_at_ms: crate::backend::dbus::monotonic_ms(),
        payload: BackendPayload::Health {
            health,
            detail: Some(detail),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        Connectivity, InterfaceId, Ssid, WifiInterface, WifiNetwork, WifiNetworkId, WifiSecurity,
    };

    #[test]
    fn connected_intent_routes_to_the_interface_owner() {
        let interface = InterfaceId("wlan0".into());
        let id = WifiNetworkId {
            interface: interface.clone(),
            ssid: Ssid(b"home".to_vec()),
            security: WifiSecurity::Personal,
        };
        let mut state = AppState::default();
        state.wifi.interfaces.insert(
            interface.clone(),
            WifiInterface {
                id: interface,
                backend: BackendKind::Iwd,
                powered: true,
                scanning: false,
                last_scan_ms: None,
                addresses: Vec::new(),
                capabilities: BTreeMap::new(),
            },
        );
        state.wifi.networks.insert(
            id.clone(),
            WifiNetwork {
                id: id.clone(),
                display_name: "home".into(),
                signal: 50,
                state: crate::domain::ConnectionState::Disconnected,
                connectivity: Connectivity::Unknown,
                saved: false,
                auto_join: false,
                bss_count: 1,
                active_bssid: None,
                present: true,
                last_seen_ms: 0,
            },
        );
        let routed = route_intent(
            Intent::SetConnection {
                target: EntityId::Wifi(id),
                desired: DesiredState::Connected,
                credential: None,
            },
            &state,
        )
        .unwrap();
        assert_eq!(routed.0, BackendKind::Iwd);
        assert_eq!(routed.3, BackendAction::Connect);
    }

    #[test]
    fn profile_intents_route_without_becoming_connection_operations() {
        let interface = InterfaceId("wlan0".into());
        let id = WifiNetworkId {
            interface: interface.clone(),
            ssid: Ssid(b"home".to_vec()),
            security: WifiSecurity::Personal,
        };
        let mut state = AppState::default();
        state.wifi.interfaces.insert(
            interface.clone(),
            WifiInterface {
                id: interface,
                backend: BackendKind::Iwd,
                powered: true,
                scanning: false,
                last_scan_ms: None,
                addresses: Vec::new(),
                capabilities: BTreeMap::new(),
            },
        );

        let auto_join = route_intent(
            Intent::SetWifiAutoJoin {
                id: id.clone(),
                enabled: true,
            },
            &state,
        )
        .unwrap();
        assert_eq!(auto_join.0, BackendKind::Iwd);
        assert_eq!(auto_join.2, DesiredState::AutoJoinEnabled);
        assert!(matches!(
            auto_join.3,
            BackendAction::UpdateProfile(ProfileUpdate {
                auto_join: Some(true),
                ..
            })
        ));

        let forget = route_intent(Intent::Forget(EntityId::Wifi(id)), &state).unwrap();
        assert_eq!(forget.2, DesiredState::Forgotten);
        assert_eq!(forget.3, BackendAction::Forget);
    }

    #[test]
    fn automatic_bluetooth_discovery_acquires_its_own_session() {
        let id = crate::domain::AdapterId("hci0".into());
        let mut state = AppState::default();
        state.bluetooth.selected_adapter = Some(id.clone());
        state.bluetooth.adapters.insert(
            id.clone(),
            crate::domain::BluetoothAdapter {
                id,
                powered: true,
                scanning: true,
                capabilities: BTreeMap::new(),
            },
        );

        let automatic = route_intent(Intent::StartBluetoothDiscovery, &state).unwrap();
        assert_eq!(automatic.2, DesiredState::Scanning);
        assert_eq!(automatic.3, BackendAction::Scan);

        let manual = route_intent(Intent::ToggleBluetoothDiscovery, &state).unwrap();
        assert_eq!(manual.2, DesiredState::Idle);
        assert_eq!(manual.3, BackendAction::StopScan);
    }

    #[test]
    fn bluetooth_property_intents_route_to_bluez() {
        let adapter = crate::domain::AdapterId("hci0".into());
        let id = crate::domain::BluetoothDeviceId {
            adapter,
            address: crate::domain::HardwareAddress("01:23:45:67:89:AB".into()),
        };
        let state = AppState::default();

        let paired = route_intent(Intent::PairBluetooth(id.clone()), &state).unwrap();
        assert_eq!(paired.0, BackendKind::Bluez);
        assert_eq!(paired.2, DesiredState::Paired);
        assert_eq!(paired.3, BackendAction::Pair);

        let trusted = route_intent(
            Intent::SetBluetoothTrusted {
                id: id.clone(),
                trusted: false,
            },
            &state,
        )
        .unwrap();
        assert_eq!(trusted.2, DesiredState::Untrusted);
        assert_eq!(trusted.3, BackendAction::SetTrusted(false));

        let blocked =
            route_intent(Intent::SetBluetoothBlocked { id, blocked: true }, &state).unwrap();
        assert_eq!(blocked.2, DesiredState::Blocked);
        assert_eq!(blocked.3, BackendAction::SetBlocked(true));
    }

    fn candidate_snapshot(kind: BackendKind, revision: u64, usable: bool) -> BackendEvent {
        BackendEvent {
            backend: kind,
            epoch: 1,
            revision,
            observed_at_ms: revision,
            payload: BackendPayload::WifiSnapshot(crate::domain::WifiSnapshot {
                interfaces: usable
                    .then(|| WifiInterface {
                        id: InterfaceId("wlan0".into()),
                        backend: kind,
                        powered: true,
                        scanning: false,
                        last_scan_ms: None,
                        addresses: Vec::new(),
                        capabilities: BTreeMap::new(),
                    })
                    .into_iter()
                    .collect(),
                networks: Vec::new(),
            }),
        }
    }

    #[tokio::test]
    async fn auto_selection_prefers_usable_manager_and_fails_over() {
        let selection = Mutex::new(WifiSelection::default());
        let iwd = candidate_snapshot(BackendKind::Iwd, 1, true);
        assert_eq!(
            update_wifi_selection(&selection, iwd.clone()).await,
            vec![iwd.clone()]
        );

        let unusable_nm = candidate_snapshot(BackendKind::NetworkManager, 1, false);
        assert!(update_wifi_selection(&selection, unusable_nm)
            .await
            .is_empty());

        let usable_nm = candidate_snapshot(BackendKind::NetworkManager, 2, true);
        assert_eq!(
            update_wifi_selection(&selection, usable_nm.clone()).await,
            vec![usable_nm]
        );

        let stopped_nm = health_event(
            BackendKind::NetworkManager,
            BackendHealth::Unavailable,
            "stopped".into(),
        );
        let failover = update_wifi_selection(&selection, stopped_nm.clone()).await;
        assert_eq!(failover, vec![stopped_nm, iwd]);
    }
}
