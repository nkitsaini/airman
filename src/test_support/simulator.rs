use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use tokio::sync::broadcast;

use crate::{
    backend::{
        BackendCommand, BackendDiagnostics, BackendFailure, CapabilityMap, OperationAcceptance,
        ProbeResult, ProbeStatus, RadioBackend,
    },
    domain::{BackendEvent, BackendKind, Capability, CapabilityState, OperationId},
};

#[derive(Debug, Clone)]
pub struct ScriptedEvent {
    pub at_ms: u64,
    pub event: BackendEvent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedCommand {
    pub operation_id: OperationId,
    pub target: crate::domain::EntityId,
    pub desired: crate::domain::DesiredState,
    pub action: crate::backend::BackendAction,
    pub had_credential: bool,
    pub remember_credential: bool,
}

struct SimulatorState {
    now_ms: u64,
    snapshot: BackendEvent,
    pending_events: VecDeque<ScriptedEvent>,
    command_results: HashMap<OperationId, Result<OperationAcceptance, BackendFailure>>,
    commands: Vec<RecordedCommand>,
    cancelled: HashSet<OperationId>,
    capabilities: CapabilityMap,
}

#[derive(Clone)]
pub struct SimulatorBackend {
    kind: BackendKind,
    state: Arc<Mutex<SimulatorState>>,
    events: broadcast::Sender<BackendEvent>,
}

impl SimulatorBackend {
    pub fn new(kind: BackendKind, snapshot: BackendEvent) -> Self {
        let (events, _) = broadcast::channel(128);
        Self {
            kind,
            state: Arc::new(Mutex::new(SimulatorState {
                now_ms: 0,
                snapshot,
                pending_events: VecDeque::new(),
                command_results: HashMap::new(),
                commands: Vec::new(),
                cancelled: HashSet::new(),
                capabilities: BTreeMap::new(),
            })),
            events,
        }
    }

    pub fn with_script(self, mut script: Vec<ScriptedEvent>) -> Self {
        script.sort_by_key(|event| event.at_ms);
        self.state
            .lock()
            .expect("simulator mutex poisoned")
            .pending_events = script.into();
        self
    }

    pub fn set_capability(&self, capability: Capability, state: CapabilityState) {
        self.state
            .lock()
            .expect("simulator mutex poisoned")
            .capabilities
            .insert(capability, state);
    }

    pub fn set_command_result(
        &self,
        operation: OperationId,
        result: Result<OperationAcceptance, BackendFailure>,
    ) {
        self.state
            .lock()
            .expect("simulator mutex poisoned")
            .command_results
            .insert(operation, result);
    }

    pub fn advance_to(&self, now_ms: u64) -> usize {
        let ready = {
            let mut state = self.state.lock().expect("simulator mutex poisoned");
            assert!(now_ms >= state.now_ms, "virtual time cannot move backwards");
            state.now_ms = now_ms;
            let mut ready = Vec::new();
            while state
                .pending_events
                .front()
                .is_some_and(|event| event.at_ms <= now_ms)
            {
                let scripted = state
                    .pending_events
                    .pop_front()
                    .expect("front was checked above");
                state.snapshot = scripted.event.clone();
                ready.push(scripted.event);
            }
            ready
        };

        let count = ready.len();
        for event in ready {
            let _ = self.events.send(event);
        }
        count
    }

    pub fn recorded_commands(&self) -> Vec<RecordedCommand> {
        self.state
            .lock()
            .expect("simulator mutex poisoned")
            .commands
            .clone()
    }

    pub fn was_cancelled(&self, operation: OperationId) -> bool {
        self.state
            .lock()
            .expect("simulator mutex poisoned")
            .cancelled
            .contains(&operation)
    }
}

#[async_trait]
impl RadioBackend for SimulatorBackend {
    fn kind(&self) -> BackendKind {
        self.kind
    }

    async fn probe(&self) -> ProbeResult {
        ProbeResult {
            backend: self.kind,
            status: ProbeStatus::Available,
            owner: Some("simulator".into()),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            detail: None,
        }
    }

    fn subscribe(&self) -> broadcast::Receiver<BackendEvent> {
        self.events.subscribe()
    }

    async fn snapshot(&self) -> Result<BackendEvent, BackendFailure> {
        Ok(self
            .state
            .lock()
            .expect("simulator mutex poisoned")
            .snapshot
            .clone())
    }

    async fn capabilities(&self) -> CapabilityMap {
        self.state
            .lock()
            .expect("simulator mutex poisoned")
            .capabilities
            .clone()
    }

    async fn execute(
        &self,
        command: BackendCommand,
    ) -> Result<OperationAcceptance, BackendFailure> {
        let mut state = self.state.lock().expect("simulator mutex poisoned");
        let result = state
            .command_results
            .remove(&command.operation_id)
            .unwrap_or_else(|| {
                Ok(OperationAcceptance {
                    phase: crate::domain::OperationPhase::Running("accepted by simulator".into()),
                    deadline_ms: state.now_ms + 30_000,
                })
            });
        state.commands.push(RecordedCommand {
            operation_id: command.operation_id,
            target: command.target,
            desired: command.desired,
            action: command.action,
            had_credential: command.credential.is_some(),
            remember_credential: command.remember_credential,
        });
        result
    }

    async fn cancel(&self, operation_id: OperationId) -> Result<(), BackendFailure> {
        self.state
            .lock()
            .expect("simulator mutex poisoned")
            .cancelled
            .insert(operation_id);
        Ok(())
    }

    async fn diagnostics(&self) -> BackendDiagnostics {
        let state = self.state.lock().expect("simulator mutex poisoned");
        BackendDiagnostics {
            backend: self.kind,
            owner: Some("simulator".into()),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            properties: BTreeMap::from([
                ("clock_ms".into(), state.now_ms.to_string()),
                (
                    "queued_events".into(),
                    state.pending_events.len().to_string(),
                ),
                ("commands".into(), state.commands.len().to_string()),
            ]),
            warnings: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::{
        backend::{BackendAction, Secret},
        domain::{
            BackendPayload, BluetoothSnapshot, DesiredState, EntityId, InterfaceId, OperationPhase,
        },
    };

    fn event(revision: u64) -> BackendEvent {
        BackendEvent {
            backend: BackendKind::Simulator,
            epoch: 1,
            revision,
            observed_at_ms: revision * 10,
            payload: BackendPayload::BluetoothSnapshot(BluetoothSnapshot::default()),
        }
    }

    #[tokio::test]
    async fn subscription_before_snapshot_does_not_miss_events() {
        let backend = SimulatorBackend::new(BackendKind::Simulator, event(1)).with_script(vec![
            ScriptedEvent {
                at_ms: 10,
                event: event(2),
            },
        ]);
        let mut subscription = backend.subscribe();
        assert_eq!(backend.advance_to(10), 1);
        let snapshot = backend.snapshot().await.unwrap();
        let update = subscription.recv().await.unwrap();
        assert_eq!(snapshot.revision, 2);
        assert_eq!(update.revision, 2);
    }

    #[tokio::test]
    async fn commands_record_only_presence_of_credentials() {
        let backend = SimulatorBackend::new(BackendKind::Simulator, event(1));
        let operation_id = OperationId(7);
        let acceptance = backend
            .execute(BackendCommand {
                operation_id,
                target: EntityId::WifiInterface(InterfaceId("wlan0".into())),
                desired: DesiredState::Connected,
                action: BackendAction::Connect,
                credential: Some(Secret::new("secret value".into())),
                remember_credential: true,
            })
            .await
            .unwrap();
        assert_eq!(
            acceptance.phase,
            OperationPhase::Running("accepted by simulator".into())
        );
        let recorded = backend.recorded_commands();
        assert!(recorded[0].had_credential);
        assert!(!format!("{recorded:?}").contains("secret value"));
    }

    #[tokio::test]
    async fn cancellation_and_diagnostics_use_virtual_state() {
        let backend = SimulatorBackend::new(BackendKind::Simulator, event(1));
        backend.set_capability(Capability::Scan, CapabilityState::Supported);
        backend.cancel(OperationId(4)).await.unwrap();
        assert!(backend.was_cancelled(OperationId(4)));
        assert_eq!(
            backend.capabilities().await,
            BTreeMap::from([(Capability::Scan, CapabilityState::Supported)])
        );
        assert_eq!(backend.diagnostics().await.properties["clock_ms"], "0");
    }
}
