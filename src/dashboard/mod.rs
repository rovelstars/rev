use color_eyre::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap},
};
use tokio::net::UnixStream;

use crate::bus::protocol::{self, Message, MessageBody};

#[derive(Debug, Clone)]
struct ServiceEntry {
    name: String,
    description: String,
    running: bool,
    pid: Option<u32>,
    uptime: String,
    exec: String,
    restart_policy: String,
    memory: String,
    cpu: String,
    tasks: String,
    restart_count: u32,
    exit_code: Option<i32>,
    config_path: String,
    log_tail: Vec<String>,
}

enum View {
    List,
    Detail(usize),
    Help,
}

struct App {
    services: Vec<ServiceEntry>,
    table_state: TableState,
    view: View,
    status_msg: String,
    should_quit: bool,
}

impl App {
    fn new() -> Self {
        Self {
            services: Vec::new(),
            table_state: TableState::default().with_selected(Some(0)),
            view: View::List,
            status_msg: String::from("Press ? for help"),
            should_quit: false,
        }
    }

    async fn load_services(&mut self) {
        match fetch_services().await {
            Ok(svc_list) => {
                self.services = svc_list
                    .into_iter()
                    .map(|(name, info)| build_entry(name, info))
                    .collect();
                self.status_msg = format!("Loaded {} services", self.services.len());
            }
            Err(e) => {
                self.status_msg = format!("Cannot connect to rev: {}", e);
                self.load_services_from_disk();
            }
        }
    }

    fn load_services_from_disk(&mut self) {
        let dirs = crate::parser::service_dirs();
        self.services.clear();
        for dir in dirs {
            if !dir.exists() {
                continue;
            }
            for entry in walkdir::WalkDir::new(&dir) {
                let entry = match entry {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                let p = entry.path();
                if p.is_file() && p.extension().and_then(|s| s.to_str()) == Some("rsc") {
                    if let Ok(text) = std::fs::read_to_string(p) {
                        if let Ok(config) = toml::from_str::<crate::parser::ServiceConfig>(&text) {
                            let name = config.name.clone();
                            let log_tail = crate::logger::tail_log(&name, 10);
                            self.services.push(ServiceEntry {
                                description: config.description.clone().unwrap_or_default(),
                                name,
                                running: false,
                                pid: None,
                                uptime: "—".into(),
                                exec: config.exec_start.clone(),
                                restart_policy: format!("{:?}", config.restart_policy),
                                memory: "—".into(),
                                cpu: "—".into(),
                                tasks: "—".into(),
                                restart_count: 0,
                                exit_code: None,
                                config_path: p.display().to_string(),
                                log_tail,
                            });
                        }
                    }
                }
            }
        }
        if !self.services.is_empty() {
            self.status_msg = format!(
                "Loaded {} services from disk (init not running)",
                self.services.len()
            );
        }
    }

    fn selected_service(&self) -> Option<&ServiceEntry> {
        self.table_state.selected().and_then(|i| self.services.get(i))
    }

    fn next(&mut self) {
        if self.services.is_empty() {
            return;
        }
        let i = self.table_state.selected().unwrap_or(0);
        let next = if i >= self.services.len() - 1 { 0 } else { i + 1 };
        self.table_state.select(Some(next));
    }

    fn prev(&mut self) {
        if self.services.is_empty() {
            return;
        }
        let i = self.table_state.selected().unwrap_or(0);
        let prev = if i == 0 { self.services.len() - 1 } else { i - 1 };
        self.table_state.select(Some(prev));
    }

    async fn start_selected(&mut self) {
        if let Some(svc) = self.selected_service() {
            let name = svc.name.clone();
            match send_bus_command(MessageBody::StartService { service: name }).await {
                Ok(resp) => self.status_msg = resp,
                Err(e) => self.status_msg = format!("Error: {}", e),
            }
            self.load_services().await;
        }
    }

    async fn stop_selected(&mut self) {
        if let Some(svc) = self.selected_service() {
            let name = svc.name.clone();
            match send_bus_command(MessageBody::StopService { service: name }).await {
                Ok(resp) => self.status_msg = resp,
                Err(e) => self.status_msg = format!("Error: {}", e),
            }
            self.load_services().await;
        }
    }
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1}G", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1}M", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.0}K", bytes as f64 / 1024.0)
    } else {
        format!("{}B", bytes)
    }
}

fn format_cpu(secs: f64) -> String {
    if secs >= 3600.0 {
        format!("{:.0}h {:.0}m", secs / 3600.0, (secs % 3600.0) / 60.0)
    } else if secs >= 60.0 {
        format!("{:.0}m {:.1}s", secs / 60.0, secs % 60.0)
    } else {
        format!("{:.3}s", secs)
    }
}

fn build_entry(name: String, info: crate::parser::ServiceInfo) -> ServiceEntry {
    let uptime = if let Some(ts) = info.up_timestamp {
        let dur = chrono::Utc::now() - ts;
        if dur.num_hours() > 0 {
            format!("{}h {}m", dur.num_hours(), dur.num_minutes() % 60)
        } else if dur.num_minutes() > 0 {
            format!("{}m {}s", dur.num_minutes(), dur.num_seconds() % 60)
        } else {
            format!("{}s", dur.num_seconds())
        }
    } else {
        "—".into()
    };
    let memory = info.memory_bytes.map(format_bytes).unwrap_or("—".into());
    let cpu = info.cpu_seconds.map(format_cpu).unwrap_or("—".into());
    let tasks = info.tasks.map(|t| t.to_string()).unwrap_or("—".into());
    let log_tail = crate::logger::tail_log(&name, 10);

    ServiceEntry {
        description: info.config.description.clone().unwrap_or_default(),
        name,
        running: info.is_running,
        pid: info.pid,
        uptime,
        exec: info.config.exec_start.clone(),
        restart_policy: format!("{:?}", info.config.restart_policy),
        memory,
        cpu,
        tasks,
        restart_count: info.restart_count,
        exit_code: info.last_exit_code,
        config_path: info.config_path.unwrap_or("—".into()),
        log_tail,
    }
}

async fn fetch_services() -> std::result::Result<Vec<(String, crate::parser::ServiceInfo)>, String> {
    let socket_path = crate::bus::socket_path();
    let stream = UnixStream::connect(&socket_path)
        .await
        .map_err(|e| format!("{}", e))?;
    let (mut reader, mut writer) = stream.into_split();

    let msg = Message {
        id: 1,
        sender: "rev-dashboard".to_string(),
        auth_token: None,
        body: MessageBody::ListServices,
    };
    protocol::send_message(&mut writer, &msg)
        .await
        .map_err(|e| format!("{}", e))?;

    let response = protocol::recv_message(&mut reader)
        .await
        .map_err(|e| format!("{}", e))?;

    match response.body {
        MessageBody::ServiceList { services } => Ok(services),
        MessageBody::Error { message } => Err(message),
        _ => Err("unexpected response".to_string()),
    }
}

async fn send_bus_command(body: MessageBody) -> std::result::Result<String, String> {
    let socket_path = crate::bus::socket_path();
    let stream = UnixStream::connect(&socket_path)
        .await
        .map_err(|e| format!("Cannot connect to rev: {}", e))?;
    let (mut reader, mut writer) = stream.into_split();

    let msg = Message {
        id: 1,
        sender: "rev-dashboard".to_string(),
        auth_token: None,
        body,
    };
    protocol::send_message(&mut writer, &msg)
        .await
        .map_err(|e| format!("Write failed: {}", e))?;

    let response = protocol::recv_message(&mut reader)
        .await
        .map_err(|e| format!("Read failed: {}", e))?;

    match response.body {
        MessageBody::Ok { message } => Ok(message),
        MessageBody::Error { message } => Err(message),
        _ => Ok("done".to_string()),
    }
}

pub async fn show() -> Result<()> {
    color_eyre::install()?;
    let terminal = ratatui::init();
    let result = run_tui(terminal).await;
    ratatui::restore();
    result
}

async fn run_tui(mut terminal: DefaultTerminal) -> Result<()> {
    let mut app = App::new();
    app.load_services().await;

    loop {
        terminal.draw(|f| render(&mut app, f))?;

        if event::poll(std::time::Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match &app.view {
                    View::List => match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
                        KeyCode::Char('j') | KeyCode::Down => app.next(),
                        KeyCode::Char('k') | KeyCode::Up => app.prev(),
                        KeyCode::Char('s') => app.start_selected().await,
                        KeyCode::Char('x') => app.stop_selected().await,
                        KeyCode::Char('r') => {
                            match send_bus_command(MessageBody::Rescan).await {
                                Ok(msg) => app.status_msg = msg,
                                Err(e) => app.status_msg = format!("Rescan failed: {}", e),
                            }
                            app.load_services().await;
                        }
                        KeyCode::Enter => {
                            if let Some(i) = app.table_state.selected() {
                                app.view = View::Detail(i);
                            }
                        }
                        KeyCode::Char('?') => app.view = View::Help,
                        _ => {}
                    },
                    View::Detail(_) => match key.code {
                        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Backspace => {
                            app.view = View::List;
                        }
                        KeyCode::Char('s') => {
                            app.view = View::List;
                            app.start_selected().await;
                        }
                        KeyCode::Char('x') => {
                            app.view = View::List;
                            app.stop_selected().await;
                        }
                        _ => {}
                    },
                    View::Help => match key.code {
                        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') => {
                            app.view = View::List;
                        }
                        _ => {}
                    },
                }
            }
        }

        if app.should_quit {
            break;
        }
    }

    Ok(())
}

fn render(app: &mut App, frame: &mut Frame) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(3),
        ])
        .split(frame.area());

    render_header(frame, chunks[0]);
    render_status_bar(app, frame, chunks[2]);

    match &app.view {
        View::List => render_service_list(app, frame, chunks[1]),
        View::Detail(i) => render_detail(app, *i, frame, chunks[1]),
        View::Help => render_help(frame, chunks[1]),
    }
}

fn render_header(frame: &mut Frame, area: Rect) {
    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            " Rev ",
            Style::default().fg(Color::Black).bg(Color::Cyan).bold(),
        ),
        Span::raw(" "),
        Span::styled("RunixOS Service Manager", Style::default().fg(Color::Gray)),
    ]))
    .block(
        Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    frame.render_widget(header, area);
}

fn render_status_bar(app: &App, frame: &mut Frame, area: Rect) {
    let running = app.services.iter().filter(|s| s.running).count();
    let total = app.services.len();

    let bar = Paragraph::new(Line::from(vec![
        Span::styled(
            format!(" {} running / {} total ", running, total),
            Style::default().fg(Color::Green),
        ),
        Span::styled(" │ ", Style::default().fg(Color::DarkGray)),
        Span::styled(&app.status_msg, Style::default().fg(Color::Yellow)),
    ]))
    .block(
        Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    frame.render_widget(bar, area);
}

fn render_service_list(app: &mut App, frame: &mut Frame, area: Rect) {
    let header = Row::new(vec![
        Cell::from("  ").style(Style::default().fg(Color::DarkGray).bold()),
        Cell::from("Service").style(Style::default().fg(Color::DarkGray).bold()),
        Cell::from("PID").style(Style::default().fg(Color::DarkGray).bold()),
        Cell::from("Uptime").style(Style::default().fg(Color::DarkGray).bold()),
    ])
    .height(1);

    let rows: Vec<Row> = app
        .services
        .iter()
        .map(|svc| {
            let status = if svc.running {
                Cell::from(" ● ").style(Style::default().fg(Color::Green))
            } else {
                Cell::from(" ○ ").style(Style::default().fg(Color::Red))
            };
            let pid = svc
                .pid
                .map(|p| p.to_string())
                .unwrap_or_else(|| "—".into());
            Row::new(vec![
                status,
                Cell::from(svc.name.clone()),
                Cell::from(pid),
                Cell::from(svc.uptime.clone()),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(5),
            Constraint::Min(30),
            Constraint::Length(8),
            Constraint::Length(12),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .title(" Services ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray)),
    )
    .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
    .highlight_symbol("▸ ");

    frame.render_stateful_widget(table, area, &mut app.table_state);
}

fn render_detail(app: &App, index: usize, frame: &mut Frame, area: Rect) {
    let svc = match app.services.get(index) {
        Some(s) => s,
        None => return,
    };

    let status_color = if svc.running { Color::Green } else { Color::Red };
    let status_icon = if svc.running { "●" } else { "○" };
    let status_text = if svc.running {
        "active (running)"
    } else {
        "inactive (dead)"
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(12), Constraint::Percentage(50)])
        .split(area);

    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                format!("{} ", status_icon),
                Style::default().fg(status_color).bold(),
            ),
            Span::styled(&svc.name, Style::default().bold()),
            if !svc.description.is_empty() {
                Span::styled(
                    format!(" - {}", svc.description),
                    Style::default().fg(Color::Gray),
                )
            } else {
                Span::raw("")
            },
        ]),
        Line::from(vec![
            Span::styled("   Loaded: ", Style::default().fg(Color::DarkGray)),
            Span::raw(&svc.config_path),
        ]),
        Line::from(vec![
            Span::styled("   Active: ", Style::default().fg(Color::DarkGray)),
            Span::styled(status_text, Style::default().fg(status_color)),
            if svc.running {
                Span::styled(
                    format!("; since {}", svc.uptime),
                    Style::default().fg(Color::Gray),
                )
            } else {
                Span::raw("")
            },
        ]),
    ];

    if let Some(pid) = svc.pid {
        lines.push(Line::from(vec![
            Span::styled(" Main PID: ", Style::default().fg(Color::DarkGray)),
            Span::raw(format!("{}", pid)),
        ]));
    }

    lines.push(Line::from(vec![
        Span::styled("    Tasks: ", Style::default().fg(Color::DarkGray)),
        Span::raw(&svc.tasks),
    ]));
    lines.push(Line::from(vec![
        Span::styled("   Memory: ", Style::default().fg(Color::DarkGray)),
        Span::raw(&svc.memory),
    ]));
    lines.push(Line::from(vec![
        Span::styled("      CPU: ", Style::default().fg(Color::DarkGray)),
        Span::raw(&svc.cpu),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  Restart: ", Style::default().fg(Color::DarkGray)),
        Span::raw(format!(
            "{} (policy: {})",
            svc.restart_count, svc.restart_policy
        )),
    ]));

    if let Some(code) = svc.exit_code {
        lines.push(Line::from(vec![
            Span::styled("     Exit: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{}", code),
                Style::default().fg(if code == 0 {
                    Color::Green
                } else {
                    Color::Red
                }),
            ),
        ]));
    }

    lines.push(Line::from(vec![
        Span::styled("     Exec: ", Style::default().fg(Color::DarkGray)),
        Span::raw(&svc.exec),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " [s] start  [x] stop  [Esc] back",
        Style::default().fg(Color::Yellow),
    )));

    let info = Paragraph::new(Text::from(lines))
        .block(
            Block::default()
                .title(format!(" {} ", svc.name))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(info, chunks[0]);

    // Log tail
    let log_lines: Vec<Line> = svc
        .log_tail
        .iter()
        .map(|l| {
            if l.len() > 25 && l.starts_with('[') {
                Line::from(vec![
                    Span::styled(&l[..25], Style::default().fg(Color::DarkGray)),
                    Span::raw(&l[25..]),
                ])
            } else {
                Line::from(Span::raw(l.as_str()))
            }
        })
        .collect();

    let logs = Paragraph::new(Text::from(log_lines))
        .block(
            Block::default()
                .title(" Logs ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(logs, chunks[1]);
}

fn render_help(frame: &mut Frame, area: Rect) {
    let text = Text::from(vec![
        Line::from(Span::styled(
            "Keybindings",
            Style::default().bold().fg(Color::Cyan),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("  j/↓  ", Style::default().fg(Color::Yellow)),
            Span::raw("Move down"),
        ]),
        Line::from(vec![
            Span::styled("  k/↑  ", Style::default().fg(Color::Yellow)),
            Span::raw("Move up"),
        ]),
        Line::from(vec![
            Span::styled("  Enter ", Style::default().fg(Color::Yellow)),
            Span::raw("View service details"),
        ]),
        Line::from(vec![
            Span::styled("  s     ", Style::default().fg(Color::Yellow)),
            Span::raw("Start selected service"),
        ]),
        Line::from(vec![
            Span::styled("  x     ", Style::default().fg(Color::Yellow)),
            Span::raw("Stop selected service"),
        ]),
        Line::from(vec![
            Span::styled("  r     ", Style::default().fg(Color::Yellow)),
            Span::raw("Rescan service files and refresh"),
        ]),
        Line::from(vec![
            Span::styled("  ?     ", Style::default().fg(Color::Yellow)),
            Span::raw("Toggle this help"),
        ]),
        Line::from(vec![
            Span::styled("  q/Esc ", Style::default().fg(Color::Yellow)),
            Span::raw("Quit"),
        ]),
    ]);

    let help = Paragraph::new(text).block(
        Block::default()
            .title(" Help ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
    );
    frame.render_widget(help, area);
}
