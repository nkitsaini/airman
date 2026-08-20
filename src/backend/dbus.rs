use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, OnceLock,
    },
    time::{Duration, Instant},
};

use futures_util::{FutureExt, StreamExt};
use tokio::sync::broadcast;
use zbus::{message::Type, Connection, MatchRule, MessageStream};

use crate::{
    backend::{BackendFailure, ProbeResult, ProbeStatus},
    domain::{BackendEvent, BackendHealth, BackendKind, BackendPayload, ErrorCategory},
};

pub const SIGNAL_DEBOUNCE: Duration = Duration::from_millis(35);

pub fn monotonic_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START
        .get_or_init(Instant::now)
        .elapsed()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[derive(Debug)]
pub struct ServiceClock {
    kind: BackendKind,
    epoch: AtomicU64,
    revision: AtomicU64,
}

impl ServiceClock {
    pub fn new(kind: BackendKind) -> Self {
        Self {
            kind,
            epoch: AtomicU64::new(1),
            revision: AtomicU64::new(0),
        }
    }

    pub fn event(&self, payload: BackendPayload) -> BackendEvent {
        BackendEvent {
            backend: self.kind,
            epoch: self.epoch.load(Ordering::Acquire),
            revision: self.revision.fetch_add(1, Ordering::AcqRel) + 1,
            observed_at_ms: monotonic_ms(),
            payload,
        }
    }

    pub fn restart_event(&self, running: bool) -> BackendEvent {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        self.revision.store(0, Ordering::Release);
        self.event(BackendPayload::Health {
            health: if running {
                BackendHealth::Reconnecting
            } else {
                BackendHealth::Unavailable
            },
            detail: Some(if running {
                "service owner changed; rebuilding state".into()
            } else {
                "service stopped".into()
            }),
        })
    }

    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }
}

pub type SnapshotFuture =
    Pin<Box<dyn Future<Output = Result<BackendEvent, BackendFailure>> + Send>>;
pub type SnapshotFn = Arc<dyn Fn() -> SnapshotFuture + Send + Sync>;

pub fn spawn_signal_supervisor(
    connection: Connection,
    service: &'static str,
    clock: Arc<ServiceClock>,
    events: broadcast::Sender<BackendEvent>,
    snapshot: SnapshotFn,
) {
    tokio::spawn(async move {
        loop {
            if let Err(error) =
                supervise_once(&connection, service, &clock, &events, snapshot.clone()).await
            {
                tracing::warn!(service, %error, "D-Bus signal supervisor restarting");
                let _ = events.send(clock.event(BackendPayload::Health {
                    health: BackendHealth::Degraded,
                    detail: Some(format!("signal subscription failed: {error}")),
                }));
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    });
}

async fn supervise_once(
    connection: &Connection,
    service: &'static str,
    clock: &ServiceClock,
    events: &broadcast::Sender<BackendEvent>,
    snapshot: SnapshotFn,
) -> zbus::Result<()> {
    let owner_rule = MatchRule::builder()
        .msg_type(Type::Signal)
        .sender("org.freedesktop.DBus")?
        .interface("org.freedesktop.DBus")?
        .member("NameOwnerChanged")?
        .add_arg(service)?
        .build();
    let mut owner_signals = MessageStream::for_match_rule(owner_rule, connection, Some(8)).await?;

    let bus = zbus::fdo::DBusProxy::new(connection).await?;
    let name = zbus::names::BusName::try_from(service)?;
    if !bus.name_has_owner(name).await? {
        loop {
            let Some(signal) = owner_signals.next().await else {
                return Ok(());
            };
            let signal = signal?;
            let (_, _, new_owner): (String, String, String) = signal.body().deserialize()?;
            if !new_owner.is_empty() {
                let _ = events.send(clock.restart_event(true));
                emit_snapshot(events, snapshot.clone()).await;
                break;
            }
        }
    }

    let service_rule = MatchRule::builder()
        .msg_type(Type::Signal)
        .sender(service)?
        .build();
    let mut service_signals =
        MessageStream::for_match_rule(service_rule, connection, Some(256)).await?;

    loop {
        tokio::select! {
            signal = service_signals.next() => {
                let Some(signal) = signal else { return Ok(()); };
                signal?;
                tokio::time::sleep(SIGNAL_DEBOUNCE).await;
                while service_signals.next().now_or_never().flatten().is_some() {}
                emit_snapshot(events, snapshot.clone()).await;
            }
            signal = owner_signals.next() => {
                let Some(signal) = signal else { return Ok(()); };
                let signal = signal?;
                let (_, _, new_owner): (String, String, String) = signal.body().deserialize()?;
                let _ = events.send(clock.restart_event(!new_owner.is_empty()));
                if !new_owner.is_empty() {
                    emit_snapshot(events, snapshot.clone()).await;
                } else {
                    return Ok(());
                }
            }
        }
    }
}

async fn emit_snapshot(events: &broadcast::Sender<BackendEvent>, snapshot: SnapshotFn) {
    match snapshot().await {
        Ok(event) => {
            let _ = events.send(event);
        }
        Err(error) => {
            tracing::warn!(%error, "could not rebuild backend snapshot");
        }
    }
}

pub async fn probe_service(
    connection: &Connection,
    service: &'static str,
    backend: BackendKind,
) -> ProbeResult {
    let proxy = match zbus::fdo::DBusProxy::new(connection).await {
        Ok(proxy) => proxy,
        Err(error) => {
            return ProbeResult {
                backend,
                status: ProbeStatus::NotRunning,
                owner: None,
                version: None,
                detail: Some(error.to_string()),
            }
        }
    };
    let name = match zbus::names::BusName::try_from(service) {
        Ok(name) => name,
        Err(error) => {
            return ProbeResult {
                backend,
                status: ProbeStatus::NotInstalled,
                owner: None,
                version: None,
                detail: Some(error.to_string()),
            }
        }
    };
    match proxy.name_has_owner(name.clone()).await {
        Ok(true) => ProbeResult {
            backend,
            status: ProbeStatus::Available,
            owner: proxy
                .get_name_owner(name)
                .await
                .ok()
                .map(|owner| owner.to_string()),
            version: None,
            detail: None,
        },
        Ok(false) => ProbeResult {
            backend,
            status: ProbeStatus::NotRunning,
            owner: None,
            version: None,
            detail: Some(format!("{service} has no D-Bus owner")),
        },
        Err(error) => ProbeResult {
            backend,
            status: ProbeStatus::NotRunning,
            owner: None,
            version: None,
            detail: Some(error.to_string()),
        },
    }
}

pub fn dbus_failure(context: &str, error: impl std::fmt::Display) -> BackendFailure {
    let detail = error.to_string();
    let lower = detail.to_lowercase();
    let (category, summary, recovery, retryable) = if lower.contains("accessdenied")
        || lower.contains("not authorized")
        || lower.contains("permission")
    {
        (
            ErrorCategory::PermissionDenied,
            format!("Permission denied while trying to {context}"),
            vec!["Check the active polkit agent and local radio-control policy".into()],
            false,
        )
    } else if lower.contains("nosecrets") || lower.contains("no secrets") {
        (
            ErrorCategory::MissingSecrets,
            format!("A credential is required to {context}"),
            vec!["Retry and provide the requested credential".into()],
            true,
        )
    } else if lower.contains("authentication") || lower.contains("invalid password") {
        (
            ErrorCategory::Authentication,
            format!("Authentication failed while trying to {context}"),
            vec!["Verify the credential and retry".into()],
            true,
        )
    } else if lower.contains("unknownobject") || lower.contains("not found") {
        (
            ErrorCategory::NotFound,
            format!("The selected radio item disappeared before it could {context}"),
            vec!["Refresh the list and retry if the item is still nearby".into()],
            true,
        )
    } else if lower.contains("inprogress") || lower.contains("busy") {
        (
            ErrorCategory::Busy,
            format!("The radio service is busy and could not {context}"),
            vec!["Wait for the current operation to finish, then retry".into()],
            true,
        )
    } else {
        (
            ErrorCategory::ServiceUnavailable,
            format!("The radio service could not {context}"),
            vec!["Open diagnostics to inspect daemon ownership and recent events".into()],
            true,
        )
    };
    BackendFailure {
        category,
        summary,
        detail,
        recovery,
        retryable,
        raw_code: None,
    }
}
