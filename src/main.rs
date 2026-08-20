use std::error::Error;

use clap::Parser;
use futures_util::StreamExt;
use airman::{
    app::{Application, Intent},
    backend::{monotonic_ms, Secret},
    cli::{Cli, Command},
    config::{self, Settings},
    discovery::DiscoveryCoordinator,
    domain::OperationId,
    logging,
    runtime::Runtime,
    terminal::{self, TerminalSession},
    tui,
};
use tokio::time::{self, Duration, MissedTickBehavior};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();
    let settings = Settings::load(&cli)?;
    let logging_guard = logging::init(&settings.log_level, settings.log_file.as_deref())?;

    tracing::info!(
        backend = ?settings.backend,
        wifi_interface = ?settings.wifi_interface,
        bluetooth_adapter = ?settings.bluetooth_adapter,
        auto_scan = settings.auto_scan,
        auto_discover = settings.auto_discover,
        log_path = %logging_guard.path.display(),
        "starting airman"
    );

    let mut runtime = Runtime::start(&settings).await;
    if let Some(Command::Diagnose { json }) = cli.command {
        print_diagnostics(&runtime, json, &logging_guard.path).await?;
        return Ok(());
    }

    terminal::install_panic_hook();
    let mut terminal_session = TerminalSession::enter()?;
    let mut application = Application::with_persistent_connection_history(
        config::state_directory().join("connection-history.json"),
    );
    let mut input = crossterm::event::EventStream::new();
    let mut animation = time::interval(Duration::from_millis(100));
    animation.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut housekeeping = time::interval(Duration::from_secs(1));
    housekeeping.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut dirty = true;
    let mut discovery = DiscoveryCoordinator::new(settings.auto_scan, settings.auto_discover);

    while !application.should_quit() {
        if dirty {
            terminal_session
                .terminal_mut()
                .draw(|frame| tui::draw(frame, &mut application))?;
            dirty = false;
        }

        tokio::select! {
            maybe_event = input.next() => {
                match maybe_event {
                    Some(Ok(event)) => {
                        let now_ms = monotonic_ms();
                        match &event {
                            crossterm::event::Event::FocusLost => {
                                discovery.set_terminal_focused(false, now_ms);
                            }
                            crossterm::event::Event::FocusGained => {
                                discovery.set_terminal_focused(true, now_ms);
                            }
                            _ => {
                                if let Some(intent) = application.handle_terminal_event(event) {
                                    if let Some(intent) = discovery.prepare_user_intent(
                                        intent,
                                        &application.reducer.state,
                                        now_ms,
                                    ) {
                                        dispatch_intent(
                                            &mut application,
                                            &runtime,
                                            &mut discovery,
                                            intent,
                                            now_ms,
                                        ).await;
                                    }
                                }
                            }
                        }
                        if !application.should_quit() {
                            reconcile_discovery(
                                &mut application,
                                &runtime,
                                &mut discovery,
                                now_ms,
                            ).await;
                        }
                        dirty = true;
                    }
                    Some(Err(error)) => {
                        tracing::error!(%error, "terminal input stream failed");
                        application.report_runtime_error(
                            "Terminal input failed",
                            error.to_string(),
                            monotonic_ms(),
                        );
                        dirty = true;
                    }
                    None => application.request_quit(),
                }
            }
            event = runtime.next_event() => {
                if let Some(event) = event {
                    let now_ms = monotonic_ms();
                    discovery.observe_event(&event, now_ms);
                    application.apply_event(event);
                    reconcile_discovery(
                        &mut application,
                        &runtime,
                        &mut discovery,
                        now_ms,
                    ).await;
                    dirty = true;
                }
            }
            _ = animation.tick(), if application.needs_animation() => {
                dirty = application.tick(monotonic_ms());
            }
            _ = housekeeping.tick() => {
                let now_ms = monotonic_ms();
                dirty |= application.tick(now_ms);
                reconcile_discovery(
                    &mut application,
                    &runtime,
                    &mut discovery,
                    now_ms,
                ).await;
            }
            result = tokio::signal::ctrl_c() => {
                if let Err(error) = result {
                    tracing::warn!(%error, "failed to listen for Ctrl-C");
                }
                application.request_quit();
            }
        }
    }

    tracing::info!("airman stopped");
    Ok(())
}

async fn reconcile_discovery(
    application: &mut Application,
    runtime: &Runtime,
    discovery: &mut DiscoveryCoordinator,
    now_ms: u64,
) {
    discovery.observe_state(&application.reducer.state, now_ms);
    let intents = discovery.reconcile(&application.reducer.state, application.pane, now_ms);
    for intent in intents {
        dispatch_intent(application, runtime, discovery, intent, now_ms).await;
    }
    application.set_discovery_mode(discovery.discovery_mode());
}

async fn dispatch_intent(
    application: &mut Application,
    runtime: &Runtime,
    discovery: &mut DiscoveryCoordinator,
    intent: Intent,
    now_ms: u64,
) {
    let attempt = discovery.attempt_for(&intent, &application.reducer.state);
    let operation = handle_intent(application, runtime, intent, now_ms).await;
    discovery.record_attempt(attempt, operation, now_ms);
}

async fn handle_intent(
    application: &mut Application,
    runtime: &Runtime,
    intent: Intent,
    now_ms: u64,
) -> Option<OperationId> {
    match intent {
        Intent::Quit => {
            application.request_quit();
            None
        }
        Intent::OpenDiagnostics => {
            let lines = runtime
                .diagnostics()
                .await
                .into_iter()
                .flat_map(|backend| {
                    let mut lines = vec![format!(
                        "{}: {}{}",
                        backend.backend,
                        backend.owner.as_deref().unwrap_or("not running"),
                        backend
                            .version
                            .as_ref()
                            .map_or_else(String::new, |version| format!(" ({version})"))
                    )];
                    lines.extend(
                        backend
                            .properties
                            .into_iter()
                            .map(|(name, value)| format!("  {name}: {value}")),
                    );
                    lines.extend(
                        backend
                            .warnings
                            .into_iter()
                            .map(|warning| format!("  warning: {warning}")),
                    );
                    lines.push(String::new());
                    lines
                })
                .collect();
            application.show_diagnostics(lines);
            None
        }
        Intent::ShowWifiSecret { id, qr } => {
            let open = application
                .reducer
                .state
                .wifi
                .networks
                .get(&id)
                .is_some_and(|network| network.id.security == airman::domain::WifiSecurity::Open);
            let secret = if open {
                Ok(Secret::new(String::new()))
            } else {
                runtime.wifi_secret(&id, &application.reducer.state).await
            };
            match secret {
                Ok(secret) => {
                    if let Err(error) = application.show_wifi_share(&id, secret, qr) {
                        application.report_runtime_error(
                            "Could not show Wi-Fi sharing details",
                            error,
                            now_ms,
                        );
                    }
                }
                Err(error) => application.report_user_error(error, now_ms),
            }
            None
        }
        intent => runtime.dispatch(intent, &application.reducer.state, now_ms),
    }
}

async fn print_diagnostics(
    runtime: &Runtime,
    json: bool,
    log_path: &std::path::Path,
) -> Result<(), Box<dyn Error>> {
    let diagnostics = runtime.diagnostics().await;
    if json {
        let backends = diagnostics
            .into_iter()
            .map(|backend| {
                serde_json::json!({
                    "backend": backend.backend.to_string(),
                    "available": backend.owner.is_some(),
                    "owner": backend.owner,
                    "version": backend.version,
                    "properties": backend.properties,
                    "warnings": backend.warnings,
                })
            })
            .collect::<Vec<_>>();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "log": log_path,
                "backends": backends,
            }))?
        );
    } else {
        println!("log: {}", log_path.display());
        for backend in diagnostics {
            println!();
            println!(
                "{}: {}",
                backend.backend,
                backend.owner.as_deref().unwrap_or("not running")
            );
            if let Some(version) = backend.version {
                println!("  version: {version}");
            }
            for (name, value) in backend.properties {
                println!("  {name}: {value}");
            }
            for warning in backend.warnings {
                println!("  warning: {warning}");
            }
        }
    }
    Ok(())
}
