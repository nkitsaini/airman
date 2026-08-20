use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState,
        Tabs, Wrap,
    },
    Frame,
};

use crate::{
    app::{Application, Overlay, Pane},
    domain::{
        ActivityLevel, BackendHealth, BluetoothDevice, ConnectionState, Connectivity, EntityId,
        Operation, OperationPhase, WifiNetwork,
    },
};

fn selection_style() -> Style {
    Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
}

fn secondary_style() -> Style {
    // Preserve the terminal theme's own high-contrast foreground. Italics add
    // hierarchy without assuming whether the background is light or dark.
    Style::default().add_modifier(Modifier::ITALIC)
}

pub fn draw(frame: &mut Frame<'_>, app: &mut Application) {
    let area = frame.size();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(3),
            Constraint::Min(4),
            Constraint::Length(if app.reducer.state.current_error.is_some() {
                3
            } else {
                1
            }),
            Constraint::Length(1),
        ])
        .split(area);

    draw_header(frame, chunks[0], app);
    draw_tabs(frame, chunks[1], app);

    let content = if chunks[2].width >= 100 {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(65), Constraint::Percentage(35)])
            .split(chunks[2])
    } else {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(100), Constraint::Length(0)])
            .split(chunks[2])
    };
    draw_list(frame, content[0], app);
    if content[1].width > 0 {
        draw_details(frame, content[1], app);
    } else {
        app.set_detail_action_hit_areas(Vec::new());
    }
    draw_notification(frame, chunks[3], app);
    draw_footer(frame, chunks[4], app);

    match app.overlay {
        Some(Overlay::Help) => draw_help(frame, area),
        Some(Overlay::Activity) => draw_activity(frame, area, app),
        Some(Overlay::Palette) => draw_palette(frame, area, app),
        Some(Overlay::Search) => {}
        Some(Overlay::Credential) => draw_credential(frame, area, app),
        Some(Overlay::Diagnostics) => draw_diagnostics(frame, area, app),
        Some(Overlay::Error) => draw_error(frame, area, app),
        Some(Overlay::Confirm) => draw_confirmation(frame, area, app),
        Some(Overlay::WifiShare) => draw_wifi_share(frame, area, app),
        None => {}
    }
}

fn draw_header(frame: &mut Frame<'_>, area: Rect, app: &Application) {
    let backend_text = if app.reducer.state.backends.is_empty() {
        Span::styled("probing services", Style::default().fg(Color::Yellow))
    } else {
        let ready = app
            .reducer
            .state
            .backends
            .values()
            .filter(|backend| backend.health == BackendHealth::Ready)
            .count();
        Span::styled(
            format!(
                "{ready}/{} services ready",
                app.reducer.state.backends.len()
            ),
            Style::default().fg(if ready == 0 {
                Color::Yellow
            } else {
                Color::Green
            }),
        )
    };
    let mut spans = vec![
        Span::styled("airman", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        backend_text,
        Span::styled(
            format!("  {} events", app.reducer.state.activity.len()),
            secondary_style(),
        ),
    ];
    // Sticky, low-noise indicator: an active BlueZ discovery shares the 2.4 GHz
    // band with Wi-Fi. It lives in the header chrome so it never competes with
    // transient notifications for the notification strip.
    if app.bluetooth_discovering() {
        spans.push(Span::styled(
            "  ⚠ BT discovery may slow 2.4 GHz Wi-Fi",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_tabs(frame: &mut Frame<'_>, area: Rect, app: &Application) {
    let selected = usize::from(app.pane == Pane::Bluetooth);
    let wifi_status = app
        .reducer
        .state
        .wifi
        .selected_interface
        .as_ref()
        .and_then(|id| app.reducer.state.wifi.interfaces.get(id))
        .map_or("", |interface| {
            if !interface.powered {
                " [off]"
            } else if interface.scanning {
                " [scanning]"
            } else {
                ""
            }
        });
    let bluetooth_status = app
        .reducer
        .state
        .bluetooth
        .selected_adapter
        .as_ref()
        .and_then(|id| app.reducer.state.bluetooth.adapters.get(id))
        .map_or("", |adapter| {
            if !adapter.powered {
                " [off]"
            } else if adapter.scanning {
                " [discovering]"
            } else {
                ""
            }
        });
    let tabs = Tabs::new([
        format!("1 Wi-Fi{wifi_status}"),
        format!("2 Bluetooth{bluetooth_status}"),
    ])
    .select(selected)
    .block(Block::default().borders(Borders::ALL))
    .highlight_style(selection_style());
    frame.render_widget(tabs, area);
}

fn draw_list(frame: &mut Frame<'_>, area: Rect, app: &mut Application) {
    let offset = app.list_offset();
    let first_visible_row = match app.pane {
        Pane::Wifi => {
            let ids = app.visible_wifi_ids();
            let rows = ids
                .iter()
                .map(|id| {
                    let operation = app
                        .reducer
                        .state
                        .active_operation(&EntityId::Wifi(id.clone()));
                    wifi_row(&app.reducer.state.wifi.networks[id], operation)
                })
                .collect::<Vec<_>>();
            let selected = app
                .reducer
                .state
                .wifi
                .selected
                .as_ref()
                .and_then(|selected| ids.iter().position(|id| id == selected));
            render_table(
                frame,
                area,
                TableSpec {
                    title: " Wi-Fi networks ",
                    headers: ["State", "Network", "Signal", "Saved", "Range"],
                    widths: [
                        Constraint::Length(14),
                        Constraint::Min(12),
                        Constraint::Length(7),
                        Constraint::Length(6),
                        Constraint::Length(12),
                    ],
                },
                rows,
                selected,
                offset,
            )
        }
        Pane::Bluetooth => {
            let ids = app.visible_bluetooth_ids();
            let rows = ids
                .iter()
                .map(|id| {
                    let operation = app
                        .reducer
                        .state
                        .active_operation(&EntityId::Bluetooth(id.clone()));
                    bluetooth_row(&app.reducer.state.bluetooth.devices[id], operation)
                })
                .collect::<Vec<_>>();
            let selected = app
                .reducer
                .state
                .bluetooth
                .selected
                .as_ref()
                .and_then(|selected| ids.iter().position(|id| id == selected));
            render_table(
                frame,
                area,
                TableSpec {
                    title: " Bluetooth devices ",
                    headers: ["State", "Device", "Address", "Paired", "Range"],
                    widths: [
                        Constraint::Length(14),
                        Constraint::Min(8),
                        Constraint::Length(17),
                        Constraint::Length(7),
                        Constraint::Length(13),
                    ],
                },
                rows,
                selected,
                offset,
            )
        }
    };
    app.set_rendered_list(
        Rect {
            x: area.x.saturating_add(1),
            y: area.y.saturating_add(2),
            width: area.width.saturating_sub(2),
            height: area.height.saturating_sub(3),
        },
        first_visible_row,
    );
}

struct TableSpec<const COLUMNS: usize> {
    title: &'static str,
    headers: [&'static str; COLUMNS],
    widths: [Constraint; COLUMNS],
}

fn render_table<const COLUMNS: usize>(
    frame: &mut Frame<'_>,
    area: Rect,
    spec: TableSpec<COLUMNS>,
    rows: Vec<Row<'static>>,
    selected: Option<usize>,
    offset: usize,
) -> usize {
    let empty = rows.is_empty();
    let rows = if empty {
        vec![Row::new([Cell::from(Line::styled(
            "No items yet. The service may still be initializing.",
            secondary_style(),
        ))])]
    } else {
        rows
    };
    let table = Table::new(rows, spec.widths)
        .header(Row::new(spec.headers).style(Style::default().add_modifier(Modifier::BOLD)))
        .column_spacing(1)
        .block(Block::default().title(spec.title).borders(Borders::ALL))
        .highlight_symbol("▸ ")
        .highlight_style(selection_style());
    let mut state = TableState::default().with_offset(offset);
    state.select(if empty { None } else { selected });
    frame.render_stateful_widget(table, area, &mut state);
    state.offset()
}

fn wifi_row(network: &WifiNetwork, operation: Option<&Operation>) -> Row<'static> {
    let (status, status_style) = status_label(network.state, operation);
    let out_of_range = !network.present;
    let range = if network.present {
        "in range"
    } else {
        "out of range"
    };
    let saved = if network.saved { "known" } else { "" };
    Row::new(vec![
        Cell::from(Span::styled(
            status,
            unavailable_cell_style(status_style, out_of_range),
        )),
        Cell::from(Span::styled(
            network.display_name.clone(),
            unavailable_cell_style(Style::default().add_modifier(Modifier::BOLD), out_of_range),
        )),
        Cell::from(Span::styled(
            format!("{}%", network.signal),
            unavailable_cell_style(signal_style(network.signal), out_of_range),
        )),
        Cell::from(Span::styled(
            saved,
            unavailable_cell_style(secondary_style(), out_of_range),
        )),
        Cell::from(Span::styled(
            range,
            unavailable_cell_style(secondary_style(), out_of_range),
        )),
    ])
}

fn bluetooth_row(device: &BluetoothDevice, operation: Option<&Operation>) -> Row<'static> {
    let (status, status_style) = status_label(device.state, operation);
    let paired = if device.paired { "yes" } else { "" };
    let out_of_range = device.presence == crate::domain::Presence::OutOfRange;
    let presence = match device.presence {
        crate::domain::Presence::Present => "in range",
        crate::domain::Presence::Unknown => "unknown",
        crate::domain::Presence::OutOfRange => "out of range",
    };
    Row::new(vec![
        Cell::from(Span::styled(
            status,
            unavailable_cell_style(status_style, out_of_range),
        )),
        Cell::from(Span::styled(
            device.name.clone(),
            unavailable_cell_style(Style::default().add_modifier(Modifier::BOLD), out_of_range),
        )),
        Cell::from(Span::styled(
            device.id.address.0.clone(),
            unavailable_cell_style(secondary_style(), out_of_range),
        )),
        Cell::from(Span::styled(
            paired,
            unavailable_cell_style(secondary_style(), out_of_range),
        )),
        Cell::from(Span::styled(
            presence,
            unavailable_cell_style(secondary_style(), out_of_range),
        )),
    ])
}

fn unavailable_cell_style(style: Style, out_of_range: bool) -> Style {
    if out_of_range {
        style.fg(Color::DarkGray).add_modifier(Modifier::DIM)
    } else {
        style
    }
}

fn status_label(state: ConnectionState, operation: Option<&Operation>) -> (String, Style) {
    if let Some(operation) = operation {
        let phase = match &operation.phase {
            OperationPhase::Queued => "queued",
            OperationPhase::Running(_) => "running",
            OperationPhase::AwaitingConfirmation(_) => "waiting",
            OperationPhase::Reconciling => "reconciling",
        };
        return (
            format!("{phase}→{:?}", operation.desired).to_lowercase(),
            Style::default().fg(Color::Yellow),
        );
    }
    let (label, style) = connection_label(state);
    (label.into(), style)
}

fn connection_label(state: ConnectionState) -> (&'static str, Style) {
    match state {
        ConnectionState::Connected => ("connected", Style::default().fg(Color::Green)),
        ConnectionState::Associating => ("associating", Style::default().fg(Color::Yellow)),
        ConnectionState::Authenticating => ("authenticating", Style::default().fg(Color::Yellow)),
        ConnectionState::ObtainingAddress => ("getting IP", Style::default().fg(Color::Yellow)),
        ConnectionState::Disconnecting => ("disconnecting", Style::default().fg(Color::Yellow)),
        ConnectionState::Failed => ("failed", Style::default().fg(Color::Red)),
        ConnectionState::Disconnected => ("", secondary_style()),
    }
}

fn signal_style(signal: u8) -> Style {
    Style::default().fg(match signal {
        70..=u8::MAX => Color::Green,
        40..=69 => Color::Yellow,
        _ => Color::Red,
    })
}

fn draw_details(frame: &mut Frame<'_>, area: Rect, app: &mut Application) {
    let lines = match app.pane {
        Pane::Wifi => app
            .reducer
            .state
            .wifi
            .selected
            .as_ref()
            .and_then(|id| app.reducer.state.wifi.networks.get(id))
            .map(|network| {
                let interface = app.reducer.state.wifi.interfaces.get(&network.id.interface);
                let mut lines = vec![
                    Line::from(network.display_name.clone()),
                    Line::from(format!("State: {:?}", network.state)),
                    Line::from(format!(
                        "Connectivity: {}",
                        connectivity_label(network.connectivity)
                    )),
                    Line::from(format!("Signal: {}%", network.signal)),
                    Line::from(format!("Security: {:?}", network.id.security)),
                    Line::from(format!("BSS count: {}", network.bss_count)),
                    Line::from(format!("Auto-join: {}", yes_no(network.auto_join))),
                    Line::from(format!("Interface: {}", network.id.interface.0)),
                    Line::from(format!(
                        "Active BSSID: {}",
                        network
                            .active_bssid
                            .as_ref()
                            .map_or("not reported", |address| address.0.as_str())
                    )),
                ];
                if let Some(interface) = interface {
                    lines.push(Line::from(format!("Backend: {}", interface.backend)));
                    lines.push(Line::from(format!(
                        "Radio powered: {}",
                        yes_no(interface.powered)
                    )));
                    match network.state {
                        ConnectionState::Connected if interface.addresses.is_empty() => {
                            lines.push(Line::from("IP address: not reported"));
                        }
                        ConnectionState::Connected => {
                            lines.extend(interface.addresses.iter().map(|address| {
                                let family = if address.address.contains(':') {
                                    "IPv6"
                                } else {
                                    "IPv4"
                                };
                                Line::from(format!(
                                    "{family}: {}/{}  mask {}",
                                    address.address, address.prefix_len, address.netmask
                                ))
                            }));
                        }
                        ConnectionState::ObtainingAddress => {
                            lines.push(Line::from("IP address: obtaining address"));
                        }
                        _ => lines.push(Line::from("IP address: network not connected")),
                    }
                }
                lines
            }),
        Pane::Bluetooth => app
            .reducer
            .state
            .bluetooth
            .selected
            .as_ref()
            .and_then(|id| app.reducer.state.bluetooth.devices.get(id))
            .map(|device| {
                vec![
                    Line::from(device.name.clone()),
                    Line::from(format!("State: {:?}", device.state)),
                    Line::from(format!(
                        "Range: {}",
                        match device.presence {
                            crate::domain::Presence::Present => "present",
                            crate::domain::Presence::Unknown => "unknown (scan to update)",
                            crate::domain::Presence::OutOfRange => "out of range",
                        }
                    )),
                    Line::from(format!("Address: {}", device.id.address.0)),
                    Line::from(format!("Adapter: {}", device.id.adapter.0)),
                    Line::from(format!("Paired: {}", yes_no(device.paired))),
                    Line::from(format!("Trusted: {}", yes_no(device.trusted))),
                    Line::from(format!("Blocked: {}", yes_no(device.blocked))),
                    Line::from(format!(
                        "Services ready: {}",
                        yes_no(device.services_resolved)
                    )),
                    Line::from(format!(
                        "RSSI: {}",
                        device
                            .rssi
                            .map_or_else(|| "unknown".into(), |value| format!("{value} dBm"))
                    )),
                    Line::from(format!(
                        "Battery: {}",
                        device
                            .battery_percent
                            .map_or_else(|| "unknown".into(), |value| format!("{value}%"))
                    )),
                ]
            }),
    }
    .unwrap_or_else(|| vec![Line::styled("Nothing selected", secondary_style())]);

    let actions = app.entry_actions();
    let block = Block::default().title(" Details ").borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if actions.is_empty() {
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
        app.set_detail_action_hit_areas(Vec::new());
        return;
    }

    let action_height = u16::try_from(actions.len() + 1)
        .unwrap_or(u16::MAX)
        .min(inner.height);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(action_height)])
        .split(inner);
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        sections[0],
    );
    let mut action_lines = vec![Line::styled(
        "Actions",
        Style::default().add_modifier(Modifier::BOLD),
    )];
    action_lines.extend(actions.iter().map(|action| {
        Line::styled(
            format!("  {}", app.entry_action_label(*action)),
            Style::default().add_modifier(Modifier::BOLD),
        )
    }));
    frame.render_widget(Paragraph::new(action_lines), sections[1]);
    app.set_detail_action_hit_areas(
        actions
            .into_iter()
            .enumerate()
            .filter_map(|(index, action)| {
                let y = sections[1].y.saturating_add(index as u16 + 1);
                (y < sections[1].bottom())
                    .then_some((Rect::new(sections[1].x, y, sections[1].width, 1), action))
            })
            .collect(),
    );
}

fn connectivity_label(connectivity: Connectivity) -> &'static str {
    match connectivity {
        Connectivity::Unknown => "unknown",
        Connectivity::None => "none",
        Connectivity::Local => "local only",
        Connectivity::Limited => "limited",
        Connectivity::CaptivePortal => "captive portal",
        Connectivity::Internet => "Internet",
    }
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

fn draw_notification(frame: &mut Frame<'_>, area: Rect, app: &Application) {
    if let Some(error) = &app.reducer.state.current_error {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    "Error: ",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::raw(error.summary.clone()),
                Span::styled("  e details · Esc dismisses", secondary_style()),
            ]))
            .block(Block::default().borders(Borders::TOP))
            .wrap(Wrap { trim: true }),
            area,
        );
    } else if let Some(activity) = app.reducer.state.activity.back() {
        frame.render_widget(
            Paragraph::new(activity.message.clone()).style(activity_style(activity.level)),
            area,
        );
    }
}

fn activity_style(level: ActivityLevel) -> Style {
    match level {
        ActivityLevel::Info => Style::default(),
        ActivityLevel::Success => Style::default().fg(Color::Green),
        ActivityLevel::Warning => Style::default().fg(Color::Yellow),
        ActivityLevel::Error => Style::default().fg(Color::Red),
    }
}

fn draw_footer(frame: &mut Frame<'_>, area: Rect, app: &Application) {
    if app.overlay == Some(Overlay::Search) {
        let target = match app.pane {
            Pane::Wifi => "Search Wi-Fi networks",
            Pane::Bluetooth => "Search Bluetooth devices",
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(format!("{target}: "), secondary_style()),
                Span::raw(format!("/{}█", app.search)),
            ])),
            area,
        );
        return;
    }

    let search = if app.search.is_empty() {
        String::new()
    } else {
        format!("  filter: {}", app.search)
    };
    let range_toggle = if app.show_out_of_range {
        "o hide out-of-range"
    } else {
        "o show out-of-range"
    };
    frame.render_widget(
        Paragraph::new(format!(
            "Enter connect/disconnect  s scan  d discovery:{}  {range_toggle}  / search  Ctrl-P actions  l activity  ? help  q quit{search}",
            app.discovery_mode().short_label()
        ))
        .style(secondary_style()),
        area,
    );
}

fn draw_help(frame: &mut Frame<'_>, area: Rect) {
    let popup = centered_rect(64, 70, area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from("Navigation"),
            Line::from("  ↑/↓ or j/k     move selection"),
            Line::from("  g/G             first/last item"),
            Line::from("  1/2 or Tab      switch radio"),
            Line::from(""),
            Line::from("Actions"),
            Line::from("  Enter           connect or disconnect"),
            Line::from("  s               scan / toggle discovery on-off"),
            Line::from("  d               discovery mode: auto / on / off"),
            Line::from("  o               show/hide out-of-range items"),
            Line::from("  a/p/r/f         auto-join / password / QR / forget"),
            Line::from("  p/t/b           pair / trust / block (Bluetooth)"),
            Line::from("  Ctrl-P          command palette"),
            Line::from("  F2 / Ctrl-R     reveal password while typing"),
            Line::from("  /               filter visible items"),
            Line::from("  l               activity journal"),
            Line::from("  q / Ctrl-C      quit"),
            Line::from(""),
            Line::styled("Esc, Enter, or q closes this window", secondary_style()),
        ])
        .block(Block::default().title(" Help ").borders(Borders::ALL))
        .wrap(Wrap { trim: false }),
        popup,
    );
}

fn draw_activity(frame: &mut Frame<'_>, area: Rect, app: &Application) {
    let popup = centered_rect(80, 80, area);
    frame.render_widget(Clear, popup);
    let available = usize::from(popup.height.saturating_sub(2));
    let skip = app.reducer.state.activity.len().saturating_sub(available);
    let lines = app
        .reducer
        .state
        .activity
        .iter()
        .skip(skip)
        .map(|entry| {
            Line::styled(
                format!("{:>8}ms  {}", entry.timestamp_ms, entry.message),
                activity_style(entry.level),
            )
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().title(" Activity ").borders(Borders::ALL))
            .wrap(Wrap { trim: false }),
        popup,
    );
}

fn draw_palette(frame: &mut Frame<'_>, area: Rect, app: &Application) {
    let popup = centered_rect(70, 60, area);
    frame.render_widget(Clear, popup);
    let inner = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1)])
        .split(popup);
    frame.render_widget(
        Paragraph::new(format!("> {}", app.palette_query)).block(
            Block::default()
                .title(" Command palette ")
                .borders(Borders::ALL),
        ),
        inner[0],
    );
    let actions = app.filtered_palette_actions();
    let items = actions
        .iter()
        .map(|action| ListItem::new(action.label()))
        .collect::<Vec<_>>();
    let mut state = ListState::default();
    if !items.is_empty() {
        state.select(Some(app.palette_selected.min(items.len() - 1)));
    }
    frame.render_stateful_widget(
        List::new(items)
            .block(Block::default().borders(Borders::LEFT | Borders::RIGHT | Borders::BOTTOM))
            .highlight_symbol("▸ ")
            .highlight_style(selection_style()),
        inner[1],
        &mut state,
    );
}

fn draw_credential(frame: &mut Frame<'_>, area: Rect, app: &Application) {
    let popup = centered_rect(64, 24, area);
    frame.render_widget(Clear, popup);
    let credential = if app.credential_revealed() {
        app.credential_text().to_owned()
    } else {
        "•".repeat(app.credential_length())
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from("Enter the network password:"),
            Line::from(""),
            Line::styled(
                if credential.is_empty() {
                    " ".to_owned()
                } else {
                    credential
                },
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Line::from(""),
            Line::styled(
                "F2/Ctrl-R reveals · Enter connects · Esc cancels · never logged",
                secondary_style(),
            ),
        ])
        .block(
            Block::default()
                .title(" Wi-Fi credential ")
                .borders(Borders::ALL),
        )
        .wrap(Wrap { trim: false }),
        popup,
    );
}

fn draw_confirmation(frame: &mut Frame<'_>, area: Rect, app: &Application) {
    let Some(target) = app.confirmation_target() else {
        return;
    };
    let label = match target {
        EntityId::Wifi(id) => format!("Wi-Fi network {}", id.ssid.display()),
        EntityId::Bluetooth(id) => format!("Bluetooth device {}", id.address.0),
        _ => "selected radio item".into(),
    };
    let popup = centered_rect(64, 24, area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(format!("Forget {label}?")),
            Line::from(""),
            Line::from("This removes the saved profile or pairing."),
            Line::from("Press y or Enter to confirm; n or Esc cancels."),
        ])
        .block(
            Block::default()
                .title(" Confirm forget ")
                .borders(Borders::ALL),
        )
        .wrap(Wrap { trim: false }),
        popup,
    );
}

fn draw_wifi_share(frame: &mut Frame<'_>, area: Rect, app: &Application) {
    let Some((network, password, qr)) = app.wifi_share() else {
        return;
    };
    let popup = centered_rect(74, if qr.is_some() { 88 } else { 28 }, area);
    frame.render_widget(Clear, popup);
    let mut lines = vec![
        Line::from(format!("Network: {network}")),
        Line::from(format!(
            "Password: {}",
            if password.is_empty() {
                "(none)"
            } else {
                password
            }
        )),
    ];
    if let Some(qr) = qr {
        lines.push(Line::from(""));
        lines.extend(qr.lines().map(|line| {
            Line::styled(
                line.to_owned(),
                Style::default().fg(Color::Black).bg(Color::White),
            )
        }));
    }
    lines.push(Line::from(""));
    lines.push(Line::styled(
        "Enter, q, or Esc closes and clears these credentials",
        secondary_style(),
    ));
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(" Wi-Fi sharing ")
                    .borders(Borders::ALL),
            )
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: false }),
        popup,
    );
}

fn draw_diagnostics(frame: &mut Frame<'_>, area: Rect, app: &Application) {
    let popup = centered_rect(84, 84, area);
    frame.render_widget(Clear, popup);
    let lines = app
        .diagnostics
        .iter()
        .map(|line| Line::from(line.clone()))
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(" Diagnostics ")
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: false }),
        popup,
    );
}

fn draw_error(frame: &mut Frame<'_>, area: Rect, app: &Application) {
    let Some(error) = &app.reducer.state.current_error else {
        return;
    };
    let popup = centered_rect(78, 64, area);
    frame.render_widget(Clear, popup);
    let mut lines = vec![
        Line::styled(
            error.summary.clone(),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        Line::from(""),
        Line::from(error.detail.clone()),
    ];
    if !error.recovery.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::styled(
            "What to try",
            Style::default().add_modifier(Modifier::BOLD),
        ));
        lines.extend(
            error
                .recovery
                .iter()
                .map(|step| Line::from(format!("  • {step}"))),
        );
    }
    if let Some(code) = &error.raw_code {
        lines.push(Line::from(""));
        lines.push(Line::styled(
            format!("Service code: {code}"),
            secondary_style(),
        ));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(" Error details ")
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: false }),
        popup,
    );
}

fn centered_rect(width_percent: u16, height_percent: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height_percent) / 2),
            Constraint::Percentage(height_percent),
            Constraint::Percentage((100 - height_percent) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_percent) / 2),
            Constraint::Percentage(width_percent),
            Constraint::Percentage((100 - width_percent) / 2),
        ])
        .split(vertical[1])[1]
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use ratatui::{backend::TestBackend, Terminal};

    use super::*;
    use crate::app::Application;
    use crate::domain::{
        AppEvent, BackendEvent, BackendKind, BackendPayload, Capability, CapabilityState,
        Connectivity, InterfaceId, IpAddressInfo, Ssid, WifiInterface, WifiNetwork, WifiNetworkId,
        WifiSecurity, WifiSnapshot,
    };

    fn render(width: u16, height: u16, app: &mut Application) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer();
        let mut output = String::new();
        for y in 0..height {
            for x in 0..width {
                output.push_str(buffer.get(x, y).symbol());
            }
            output.push('\n');
        }
        output
    }

    #[test]
    fn empty_state_explains_initialization() {
        let screen = render(100, 24, &mut Application::new());
        assert!(screen.contains("probing services"));
        assert!(screen.contains("No items yet"));
        assert!(screen.contains("State"));
        assert!(screen.contains("Network"));
        assert!(screen.contains("Signal"));
        assert!(screen.contains("Saved"));
        assert!(screen.contains("Range"));
        assert!(screen.contains("Ctrl-P actions"));
    }

    #[test]
    fn minimal_terminal_does_not_panic() {
        let screen = render(20, 6, &mut Application::new());
        assert!(!screen.is_empty());
    }

    #[test]
    fn wide_layout_shows_network_facts_and_entry_actions() {
        let mut app = Application::new();
        let interface = InterfaceId("wlan0".into());
        let id = WifiNetworkId {
            interface: interface.clone(),
            ssid: Ssid(b"Home".to_vec()),
            security: WifiSecurity::Personal,
        };
        app.reducer.apply(AppEvent::Backend(BackendEvent {
            backend: BackendKind::NetworkManager,
            epoch: 1,
            revision: 1,
            observed_at_ms: 1,
            payload: BackendPayload::WifiSnapshot(WifiSnapshot {
                interfaces: vec![WifiInterface {
                    id: interface,
                    backend: BackendKind::NetworkManager,
                    powered: true,
                    scanning: false,
                    last_scan_ms: Some(1),
                    addresses: vec![IpAddressInfo {
                        address: "192.0.2.8".into(),
                        prefix_len: 24,
                        netmask: "255.255.255.0".into(),
                    }],
                    capabilities: BTreeMap::from([
                        (Capability::AutoJoin, CapabilityState::Supported),
                        (Capability::Forget, CapabilityState::Supported),
                        (Capability::SecretRetrieval, CapabilityState::Supported),
                    ]),
                }],
                networks: vec![WifiNetwork {
                    id,
                    display_name: "Home".into(),
                    signal: 80,
                    state: ConnectionState::Connected,
                    connectivity: Connectivity::Internet,
                    saved: true,
                    auto_join: true,
                    bss_count: 1,
                    active_bssid: None,
                    present: true,
                    last_seen_ms: 1,
                }],
            }),
        }));

        let screen = render(140, 35, &mut app);
        assert!(screen.contains("IPv4: 192.0.2.8/24"));
        assert!(screen.contains("mask 255.255.255.0"));
        assert!(screen.contains("[Enter] Disconnect"));
        assert!(screen.contains("[a] Disable auto-join"));
        assert!(screen.contains("[p] Show saved password"));
        assert!(screen.contains("[r] Show Wi-Fi QR code"));
        assert!(screen.contains("[f] Forget"));
    }

    #[test]
    fn help_and_activity_overlays_render() {
        let mut app = Application::new();
        app.overlay = Some(Overlay::Help);
        assert!(render(100, 30, &mut app).contains("connect or disconnect"));
        app.overlay = Some(Overlay::Activity);
        assert!(render(100, 30, &mut app).contains("airman started"));
    }

    #[test]
    fn palette_renders_filtered_actions() {
        let mut app = Application::new();
        app.overlay = Some(Overlay::Palette);
        app.palette_query = "diagnostics".into();
        let screen = render(100, 30, &mut app);
        assert!(screen.contains("Open diagnostics"));
        assert!(!screen.contains("Toggle Wi-Fi radio"));
    }

    #[test]
    fn bluetooth_search_uses_an_inline_device_specific_prompt() {
        let mut app = Application::new();
        app.pane = Pane::Bluetooth;
        app.overlay = Some(Overlay::Search);

        let screen = render(100, 30, &mut app);
        let footer = screen.lines().last().unwrap();
        assert!(footer.contains("Search Bluetooth devices: /"));
        assert!(!screen.contains(" Filter "));
    }

    #[test]
    fn error_details_include_recovery_steps() {
        let mut app = Application::new();
        app.report_runtime_error("No service", "The system bus is unavailable", 1);
        app.reducer.state.current_error.as_mut().unwrap().recovery =
            vec!["Start the D-Bus service".into()];
        app.overlay = Some(Overlay::Error);
        let screen = render(100, 30, &mut app);
        assert!(screen.contains("The system bus is unavailable"));
        assert!(screen.contains("Start the D-Bus service"));
    }

    #[test]
    fn selection_and_secondary_text_preserve_terminal_contrast() {
        let selected = selection_style();
        assert!(selected.add_modifier.contains(Modifier::REVERSED));
        assert_eq!(selected.bg, None);
        assert_eq!(secondary_style().fg, None);
    }

    #[test]
    fn out_of_range_bluetooth_style_is_greyed_out() {
        let base = Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD);

        let greyed = unavailable_cell_style(base, true);

        assert_eq!(greyed.fg, Some(Color::DarkGray));
        assert!(greyed.add_modifier.contains(Modifier::DIM));
        assert!(greyed.add_modifier.contains(Modifier::BOLD));
        assert_eq!(unavailable_cell_style(base, false), base);
    }
}
