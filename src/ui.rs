use crate::config::{self, Config, Host, Service};
use crate::manager::{ConnState, HostAggState, LogLevel, Manager};
use crate::tunnel::{TunnelEvent, TunnelId, make_id};
use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::{
    Frame, Terminal,
    backend::Backend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
};
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;
use tokio::time::interval;

pub struct App {
    pub cfg: Config,
    pub cfg_path: PathBuf,
    pub mgr: Manager,
    pub selection: Selection,
    pub expanded: HashSet<String>,
    pub modal: Modal,
    pub show_logs: bool,
    pub show_help: bool,
    pub status: Option<String>,
    pub should_quit: bool,
    pub pending_opens: HashSet<TunnelId>,
}

#[derive(Debug, Clone)]
pub struct Selection {
    pub host: Option<String>,
    pub svc: Option<String>,
}

impl Selection {
    fn empty() -> Self {
        Self { host: None, svc: None }
    }
}

pub enum Modal {
    None,
    Form(FormModal),
    Confirm(ConfirmModal),
    Error(String),
}

pub struct FormModal {
    pub title: String,
    pub fields: Vec<Field>,
    pub focus: usize,
    pub action: FormAction,
}

#[derive(Clone)]
pub enum FormAction {
    AddHost,
    EditHost { original: String },
    AddService { host: String },
    EditService { host: String, original_svc: String },
}

pub struct Field {
    pub label: String,
    pub value: String,
    pub placeholder: String,
}

pub struct ConfirmModal {
    pub message: String,
    pub action: ConfirmAction,
}

#[derive(Clone)]
pub enum ConfirmAction {
    DeleteHost(String),
    DeleteService { host: String, svc: String },
}

#[derive(Debug, Clone)]
enum Row {
    Host { name: String, ssh: String, svc_count: usize, expanded: bool },
    Service { host: String, svc_name: String },
}

impl App {
    pub fn new(cfg: Config, cfg_path: PathBuf) -> Self {
        let expanded: HashSet<String> = cfg.hosts.iter().map(|h| h.name.clone()).collect();
        let selection = match cfg.hosts.first() {
            Some(h) => Selection { host: Some(h.name.clone()), svc: None },
            None => Selection::empty(),
        };
        Self {
            cfg,
            cfg_path,
            mgr: Manager::new(),
            selection,
            expanded,
            modal: Modal::None,
            show_logs: false,
            show_help: false,
            status: None,
            should_quit: false,
            pending_opens: HashSet::new(),
        }
    }

    fn flat_rows(&self) -> Vec<Row> {
        let mut out = Vec::new();
        for h in &self.cfg.hosts {
            let expanded = self.expanded.contains(&h.name);
            out.push(Row::Host {
                name: h.name.clone(),
                ssh: h.ssh.clone(),
                svc_count: h.services.len(),
                expanded,
            });
            if expanded {
                for s in &h.services {
                    out.push(Row::Service {
                        host: h.name.clone(),
                        svc_name: s.name.clone(),
                    });
                }
            }
        }
        out
    }

    fn selected_index(&self, rows: &[Row]) -> Option<usize> {
        rows.iter().position(|r| match r {
            Row::Host { name, .. } => {
                self.selection.host.as_deref() == Some(name.as_str())
                    && self.selection.svc.is_none()
            }
            Row::Service { host, svc_name } => {
                self.selection.host.as_deref() == Some(host.as_str())
                    && self.selection.svc.as_deref() == Some(svc_name.as_str())
            }
        })
    }

    fn select_index(&mut self, rows: &[Row], idx: usize) {
        if let Some(r) = rows.get(idx) {
            match r {
                Row::Host { name, .. } => {
                    self.selection = Selection {
                        host: Some(name.clone()),
                        svc: None,
                    };
                }
                Row::Service { host, svc_name } => {
                    self.selection = Selection {
                        host: Some(host.clone()),
                        svc: Some(svc_name.clone()),
                    };
                }
            }
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let rows = self.flat_rows();
        if rows.is_empty() {
            return;
        }
        let cur = self.selected_index(&rows).unwrap_or(0) as isize;
        let len = rows.len() as isize;
        let new = (cur + delta).rem_euclid(len) as usize;
        self.select_index(&rows, new);
    }

    fn save_config(&mut self) {
        if let Err(e) = config::save(&self.cfg_path, &self.cfg) {
            self.status = Some(format!("save failed: {e}"));
        }
    }

    fn open_add_host(&mut self) {
        self.modal = Modal::Form(FormModal {
            title: "Add host".into(),
            fields: vec![
                Field { label: "name".into(), value: String::new(), placeholder: "vps".into() },
                Field { label: "ssh".into(), value: String::new(), placeholder: "ssh-config alias".into() },
            ],
            focus: 0,
            action: FormAction::AddHost,
        });
    }

    fn open_add_service(&mut self) {
        let host = match self.selection.host.clone() {
            Some(h) => h,
            None => {
                self.status = Some("no host selected — add a host first (Shift+A)".into());
                return;
            }
        };
        self.modal = Modal::Form(FormModal {
            title: format!("Add service to {host}"),
            fields: vec![
                Field { label: "name".into(), value: String::new(), placeholder: "matrix admin".into() },
                Field { label: "port".into(), value: String::new(), placeholder: "8126".into() },
                Field { label: "scheme".into(), value: "http".into(), placeholder: "http".into() },
                Field { label: "path".into(), value: "/".into(), placeholder: "/".into() },
            ],
            focus: 0,
            action: FormAction::AddService { host },
        });
    }

    fn open_edit(&mut self) {
        if let Some(svc_name) = self.selection.svc.clone() {
            let host_name = match self.selection.host.clone() {
                Some(h) => h,
                None => return,
            };
            let svc = match self
                .cfg
                .hosts
                .iter()
                .find(|h| h.name == host_name)
                .and_then(|h| h.services.iter().find(|s| s.name == svc_name))
            {
                Some(s) => s.clone(),
                None => return,
            };
            self.modal = Modal::Form(FormModal {
                title: format!("Edit {host_name}/{svc_name}"),
                fields: vec![
                    Field { label: "name".into(), value: svc.name.clone(), placeholder: String::new() },
                    Field { label: "port".into(), value: svc.port.to_string(), placeholder: String::new() },
                    Field { label: "scheme".into(), value: svc.scheme.clone(), placeholder: String::new() },
                    Field { label: "path".into(), value: svc.path.clone(), placeholder: String::new() },
                ],
                focus: 0,
                action: FormAction::EditService { host: host_name, original_svc: svc_name },
            });
        } else if let Some(host_name) = self.selection.host.clone() {
            let host = match self.cfg.hosts.iter().find(|h| h.name == host_name) {
                Some(h) => h.clone(),
                None => return,
            };
            self.modal = Modal::Form(FormModal {
                title: format!("Edit host {host_name}"),
                fields: vec![
                    Field { label: "name".into(), value: host.name.clone(), placeholder: String::new() },
                    Field { label: "ssh".into(), value: host.ssh.clone(), placeholder: String::new() },
                ],
                focus: 0,
                action: FormAction::EditHost { original: host_name },
            });
        }
    }

    fn open_delete(&mut self) {
        if let Some(svc) = self.selection.svc.clone() {
            let host = match self.selection.host.clone() {
                Some(h) => h,
                None => return,
            };
            self.modal = Modal::Confirm(ConfirmModal {
                message: format!("Delete service '{host}/{svc}'? (y/n)"),
                action: ConfirmAction::DeleteService { host, svc },
            });
        } else if let Some(host) = self.selection.host.clone() {
            let count = self.cfg.hosts.iter().find(|h| h.name == host).map(|h| h.services.len()).unwrap_or(0);
            self.modal = Modal::Confirm(ConfirmModal {
                message: format!("Delete host '{host}' and {count} service(s)? (y/n)"),
                action: ConfirmAction::DeleteHost(host),
            });
        }
    }

    fn submit_form(&mut self) {
        let form = match &self.modal {
            Modal::Form(f) => f,
            _ => return,
        };
        let action = form.action.clone();
        let values: Vec<String> = form.fields.iter().map(|f| f.value.trim().to_string()).collect();

        let result: Result<(), String> = (|| {
            match action {
                FormAction::AddHost => {
                    let name = values[0].clone();
                    let ssh = values[1].clone();
                    if name.is_empty() || ssh.is_empty() {
                        return Err("name and ssh required".into());
                    }
                    if self.cfg.hosts.iter().any(|h| h.name == name) {
                        return Err(format!("host '{name}' already exists"));
                    }
                    self.expanded.insert(name.clone());
                    self.cfg.hosts.push(Host {
                        name: name.clone(),
                        ssh,
                        services: vec![],
                    });
                    self.selection = Selection { host: Some(name), svc: None };
                }
                FormAction::EditHost { original } => {
                    let new_name = values[0].clone();
                    let new_ssh = values[1].clone();
                    if new_name.is_empty() || new_ssh.is_empty() {
                        return Err("name and ssh required".into());
                    }
                    if new_name != original && self.cfg.hosts.iter().any(|h| h.name == new_name) {
                        return Err(format!("host '{new_name}' already exists"));
                    }
                    let host = self.cfg.hosts.iter_mut().find(|h| h.name == original)
                        .ok_or_else(|| "host vanished".to_string())?;
                    if new_name != original {
                        self.expanded.remove(&original);
                        self.expanded.insert(new_name.clone());
                    }
                    host.name = new_name.clone();
                    host.ssh = new_ssh;
                    self.selection = Selection { host: Some(new_name), svc: None };
                }
                FormAction::AddService { host } => {
                    let name = values[0].clone();
                    let port_s = values[1].clone();
                    let scheme = if values[2].is_empty() { "http".into() } else { values[2].clone() };
                    let path = if values[3].is_empty() { "/".into() } else { values[3].clone() };
                    if name.is_empty() {
                        return Err("name required".into());
                    }
                    let port: u16 = port_s.parse().map_err(|_| "port must be 1..=65535".to_string())?;
                    if port == 0 {
                        return Err("port must be > 0".into());
                    }
                    let h = self.cfg.hosts.iter_mut().find(|h| h.name == host)
                        .ok_or_else(|| "host vanished".to_string())?;
                    if h.services.iter().any(|s| s.name == name) {
                        return Err(format!("service '{name}' already exists in {host}"));
                    }
                    h.services.push(Service { name: name.clone(), port, scheme, path });
                    self.selection = Selection { host: Some(host), svc: Some(name) };
                }
                FormAction::EditService { host, original_svc } => {
                    let new_name = values[0].clone();
                    let port_s = values[1].clone();
                    let scheme = if values[2].is_empty() { "http".into() } else { values[2].clone() };
                    let path = if values[3].is_empty() { "/".into() } else { values[3].clone() };
                    if new_name.is_empty() {
                        return Err("name required".into());
                    }
                    let port: u16 = port_s.parse().map_err(|_| "port must be 1..=65535".to_string())?;
                    if port == 0 {
                        return Err("port must be > 0".into());
                    }
                    let h = self.cfg.hosts.iter_mut().find(|h| h.name == host)
                        .ok_or_else(|| "host vanished".to_string())?;
                    if new_name != original_svc && h.services.iter().any(|s| s.name == new_name) {
                        return Err(format!("service '{new_name}' already exists in {host}"));
                    }
                    let svc = h.services.iter_mut().find(|s| s.name == original_svc)
                        .ok_or_else(|| "service vanished".to_string())?;
                    let was_connected = matches!(
                        self.mgr.state_of(&make_id(&host, &original_svc)),
                        ConnState::Connected { .. } | ConnState::Connecting
                    );
                    svc.name = new_name.clone();
                    svc.port = port;
                    svc.scheme = scheme;
                    svc.path = path;
                    if was_connected {
                        self.mgr.disconnect(&host, &original_svc);
                    }
                    self.selection = Selection { host: Some(host), svc: Some(new_name) };
                }
            }
            Ok(())
        })();

        match result {
            Ok(()) => {
                self.modal = Modal::None;
                self.save_config();
            }
            Err(e) => {
                self.modal = Modal::Error(e);
            }
        }
    }

    fn confirm_yes(&mut self) {
        let action = match &self.modal {
            Modal::Confirm(c) => c.action.clone(),
            _ => return,
        };
        match action {
            ConfirmAction::DeleteHost(name) => {
                if let Some(h) = self.cfg.hosts.iter().find(|h| h.name == name) {
                    for s in &h.services {
                        self.mgr.disconnect(&name, &s.name);
                    }
                }
                self.cfg.hosts.retain(|h| h.name != name);
                self.expanded.remove(&name);
                self.selection = self.cfg.hosts.first().map(|h| Selection {
                    host: Some(h.name.clone()),
                    svc: None,
                }).unwrap_or_else(Selection::empty);
            }
            ConfirmAction::DeleteService { host, svc } => {
                self.mgr.disconnect(&host, &svc);
                if let Some(h) = self.cfg.hosts.iter_mut().find(|h| h.name == host) {
                    h.services.retain(|s| s.name != svc);
                }
                self.selection = Selection { host: Some(host), svc: None };
            }
        }
        self.modal = Modal::None;
        self.save_config();
    }

    fn toggle_current(&mut self) {
        if let Some(svc_name) = self.selection.svc.clone() {
            let host_name = match self.selection.host.clone() {
                Some(h) => h,
                None => return,
            };
            let (ssh, svc) = match self
                .cfg
                .hosts
                .iter()
                .find(|h| h.name == host_name)
                .and_then(|h| h.services.iter().find(|s| s.name == svc_name).map(|s| (h.ssh.clone(), s.clone())))
            {
                Some(x) => x,
                None => return,
            };
            self.mgr.toggle_service(&host_name, &ssh, &svc);
        } else if let Some(host_name) = self.selection.host.clone() {
            let (ssh, services) = match self.cfg.hosts.iter().find(|h| h.name == host_name) {
                Some(h) => (h.ssh.clone(), h.services.clone()),
                None => return,
            };
            self.mgr.toggle_host(&host_name, &ssh, &services);
        }
    }

    fn enter_current(&mut self) {
        if self.selection.svc.is_some() {
            self.toggle_current();
        } else if let Some(host) = self.selection.host.clone() {
            if self.expanded.contains(&host) {
                self.expanded.remove(&host);
            } else {
                self.expanded.insert(host);
            }
        }
    }

    fn open_current(&mut self) {
        let svc_name = match self.selection.svc.clone() {
            Some(s) => s,
            None => {
                self.status = Some("select a service to open".into());
                return;
            }
        };
        let host_name = match self.selection.host.clone() {
            Some(h) => h,
            None => return,
        };
        let (ssh, svc) = match self
            .cfg
            .hosts
            .iter()
            .find(|h| h.name == host_name)
            .and_then(|h| h.services.iter().find(|s| s.name == svc_name).map(|s| (h.ssh.clone(), s.clone())))
        {
            Some(x) => x,
            None => return,
        };
        let id = make_id(&host_name, &svc_name);
        let state = self.mgr.state_of(&id);
        match state {
            ConnState::Connected { local_port } => {
                let url = svc.url(local_port);
                if let Err(e) = open::that_detached(&url) {
                    self.status = Some(format!("open failed: {e}"));
                } else {
                    self.status = Some(format!("opening {url}"));
                }
            }
            ConnState::Disconnecting => {
                self.status = Some("disconnecting — wait".into());
            }
            ConnState::Connecting => {
                self.pending_opens.insert(id);
                self.status = Some(format!("waiting for {host_name}/{svc_name}…"));
            }
            ConnState::Disconnected | ConnState::Failed { .. } => {
                self.pending_opens.insert(id);
                self.mgr.connect(&host_name, &ssh, &svc);
                self.status = Some(format!("connecting {host_name}/{svc_name} — will open when ready"));
            }
        }
    }

    fn handle_tunnel_event(&mut self, ev: TunnelEvent) {
        match &ev {
            TunnelEvent::Connected { id, local_port } => {
                if self.pending_opens.remove(id) {
                    if let Some(url) = self.url_for_id(id, *local_port) {
                        match open::that_detached(&url) {
                            Ok(()) => self.status = Some(format!("opening {url}")),
                            Err(e) => self.status = Some(format!("open failed: {e}")),
                        }
                    }
                }
            }
            TunnelEvent::Failed { id, reason } => {
                if self.pending_opens.remove(id) {
                    self.status = Some(format!("{id} failed: {reason}"));
                }
            }
            TunnelEvent::Disconnected { id } => {
                self.pending_opens.remove(id);
            }
            _ => {}
        }
        self.mgr.apply_event(ev);
    }

    fn url_for_id(&self, id: &TunnelId, local_port: u16) -> Option<String> {
        let (host, svc_name) = id.split_once('/')?;
        let svc = self.cfg.hosts.iter().find(|h| h.name == host)?
            .services.iter().find(|s| s.name == svc_name)?;
        Some(svc.url(local_port))
    }

    fn reload(&mut self) {
        match config::load(&self.cfg_path) {
            Ok(cfg) => {
                for h in &self.cfg.hosts {
                    for s in &h.services {
                        self.mgr.disconnect(&h.name, &s.name);
                    }
                }
                self.cfg = cfg;
                self.status = Some("config reloaded".into());
            }
            Err(e) => self.status = Some(format!("reload failed: {e}")),
        }
    }
}

pub async fn run<B: Backend>(term: &mut Terminal<B>, app: &mut App) -> Result<()> {
    let mut events = EventStream::new();
    let mut tick = interval(Duration::from_millis(150));

    loop {
        term.draw(|f| draw(f, app))?;
        if app.should_quit {
            break;
        }
        tokio::select! {
            _ = tick.tick() => {}
            maybe = events.next() => {
                match maybe {
                    Some(Ok(Event::Key(k))) if k.kind == KeyEventKind::Press => {
                        handle_key(app, k);
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break,
                }
            }
            Some(ev) = app.mgr.events_rx.recv() => {
                app.handle_tunnel_event(ev);
                loop {
                    match app.mgr.events_rx.try_recv() {
                        Ok(ev) => app.handle_tunnel_event(ev),
                        Err(_) => break,
                    }
                }
            }
        }
    }
    Ok(())
}

fn handle_key(app: &mut App, k: KeyEvent) {
    match &mut app.modal {
        Modal::Error(_) => {
            app.modal = Modal::None;
            return;
        }
        Modal::Confirm(_) => match k.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => app.confirm_yes(),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => app.modal = Modal::None,
            _ => {}
        },
        Modal::Form(form) => match k.code {
            KeyCode::Esc => app.modal = Modal::None,
            KeyCode::Enter => app.submit_form(),
            KeyCode::Tab | KeyCode::Down => {
                form.focus = (form.focus + 1) % form.fields.len();
            }
            KeyCode::BackTab | KeyCode::Up => {
                form.focus = (form.focus + form.fields.len() - 1) % form.fields.len();
            }
            KeyCode::Backspace => {
                form.fields[form.focus].value.pop();
            }
            KeyCode::Char(c) => {
                if k.modifiers.contains(KeyModifiers::CONTROL) && (c == 'u' || c == 'U') {
                    form.fields[form.focus].value.clear();
                } else {
                    form.fields[form.focus].value.push(c);
                }
            }
            _ => {}
        },
        Modal::None => {
            if app.show_help {
                app.show_help = false;
                return;
            }
            handle_tree_key(app, k);
        }
    }
}

fn handle_tree_key(app: &mut App, k: KeyEvent) {
    match k.code {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Char('?') => app.show_help = true,
        KeyCode::Char('L') => app.show_logs = !app.show_logs,
        KeyCode::Char('r') => app.reload(),
        KeyCode::Char('j') | KeyCode::Down => app.move_selection(1),
        KeyCode::Char('k') | KeyCode::Up => app.move_selection(-1),
        KeyCode::Char('h') | KeyCode::Left => {
            if app.selection.svc.is_some() {
                app.selection.svc = None;
            } else if let Some(host) = app.selection.host.clone() {
                app.expanded.remove(&host);
            }
        }
        KeyCode::Char('l') | KeyCode::Right => {
            if let Some(host) = app.selection.host.clone() {
                if app.selection.svc.is_none() {
                    app.expanded.insert(host);
                }
            }
        }
        KeyCode::Char('c') => app.toggle_current(),
        KeyCode::Enter => app.enter_current(),
        KeyCode::Char('o') => app.open_current(),
        KeyCode::Char('a') => {
            if app.cfg.hosts.is_empty() {
                app.open_add_host();
            } else {
                app.open_add_service();
            }
        }
        KeyCode::Char('A') => app.open_add_host(),
        KeyCode::Char('e') => app.open_edit(),
        KeyCode::Char('d') => app.open_delete(),
        _ => {}
    }
}

fn draw(f: &mut Frame, app: &mut App) {
    let size = f.area();
    let main_h = if app.show_logs { 60 } else { 100 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(main_h),
            if app.show_logs { Constraint::Min(5) } else { Constraint::Length(0) },
            Constraint::Length(1),
        ])
        .split(size);

    draw_tree(f, app, chunks[0]);
    if app.show_logs {
        draw_logs(f, app, chunks[1]);
    }
    draw_status(f, app, chunks[2]);

    match &app.modal {
        Modal::Form(form) => draw_form(f, form, size),
        Modal::Confirm(c) => draw_confirm(f, c, size),
        Modal::Error(e) => draw_error(f, e, size),
        Modal::None => {}
    }
    if app.show_help {
        draw_help(f, size);
    }
}

fn draw_tree(f: &mut Frame, app: &App, area: Rect) {
    let rows = app.flat_rows();
    let selected_idx = app.selected_index(&rows);
    let items: Vec<ListItem> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| render_row(app, r, Some(i) == selected_idx))
        .collect();
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("tunnels"));
    f.render_widget(list, area);
}

fn render_row<'a>(app: &App, row: &Row, selected: bool) -> ListItem<'a> {
    let base = if selected {
        Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    match row {
        Row::Host { name, ssh, svc_count, expanded } => {
            let host = app.cfg.hosts.iter().find(|h| &h.name == name);
            let agg = match host {
                Some(h) => app.mgr.host_aggregate(name, &h.services),
                None => HostAggState::Empty,
            };
            let (icon, color) = host_agg_icon(agg);
            let arrow = if *expanded { "▾" } else { "▸" };
            ListItem::new(Line::from(vec![
                Span::raw(format!("{arrow} ")),
                Span::styled(icon.to_string(), Style::default().fg(color)),
                Span::raw(format!("  {name}  ")),
                Span::styled(format!("[{ssh}]"), Style::default().fg(Color::DarkGray)),
                Span::styled(format!("  ({svc_count})"), Style::default().fg(Color::DarkGray)),
            ]))
            .style(base)
        }
        Row::Service { host, svc_name } => {
            let id = make_id(host, svc_name);
            let state = app.mgr.state_of(&id);
            let (icon, color, extra) = service_icon(&state);
            let svc = app.cfg.hosts.iter().find(|h| &h.name == host)
                .and_then(|h| h.services.iter().find(|s| &s.name == svc_name));
            let port_info = svc.map(|s| format!(" :{}", s.port)).unwrap_or_default();
            ListItem::new(Line::from(vec![
                Span::raw("    "),
                Span::styled(icon.to_string(), Style::default().fg(color)),
                Span::raw(format!("  {svc_name}")),
                Span::styled(port_info, Style::default().fg(Color::DarkGray)),
                Span::styled(extra, Style::default().fg(Color::Yellow)),
            ]))
            .style(base)
        }
    }
}

fn host_agg_icon(agg: HostAggState) -> (char, Color) {
    match agg {
        HostAggState::Empty => ('○', Color::DarkGray),
        HostAggState::AllOff => ('○', Color::Gray),
        HostAggState::AllOn => ('●', Color::Green),
        HostAggState::Mixed => ('◐', Color::Yellow),
        HostAggState::Transient => ('◌', Color::Cyan),
        HostAggState::Failed => ('✗', Color::Red),
    }
}

fn service_icon(state: &ConnState) -> (char, Color, String) {
    match state {
        ConnState::Disconnected => ('○', Color::Gray, String::new()),
        ConnState::Connecting => ('◌', Color::Cyan, " connecting…".into()),
        ConnState::Connected { local_port } => ('●', Color::Green, format!(" → 127.0.0.1:{local_port}")),
        ConnState::Disconnecting => ('◌', Color::Cyan, " disconnecting…".into()),
        ConnState::Failed { reason } => ('✗', Color::Red, format!(" {reason}")),
    }
}

fn draw_logs(f: &mut Frame, app: &App, area: Rect) {
    let lines: Vec<Line> = app
        .mgr
        .logs
        .iter()
        .rev()
        .take(area.height.saturating_sub(2) as usize)
        .map(|e| {
            let (tag, tag_color) = match e.level {
                LogLevel::Info => ("INFO", Color::Green),
                LogLevel::Warn => ("WARN", Color::Yellow),
                LogLevel::Error => ("ERR ", Color::Red),
            };
            let line_color = match e.level {
                LogLevel::Error => Color::Red,
                _ => Color::White,
            };
            Line::from(vec![
                Span::styled(e.timestamp.clone(), Style::default().fg(Color::DarkGray)),
                Span::raw(" "),
                Span::styled(tag.to_string(), Style::default().fg(tag_color)),
                Span::raw(" "),
                Span::styled(format!("{} ", e.id), Style::default().fg(Color::DarkGray)),
                Span::styled(e.line.clone(), Style::default().fg(line_color)),
            ])
        })
        .collect();
    let p = Paragraph::new(lines.into_iter().rev().collect::<Vec<_>>())
        .block(Block::default().borders(Borders::ALL).title("logs (L to hide)"))
        .wrap(Wrap { trim: false });
    f.render_widget(p, area);
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let txt = match &app.status {
        Some(s) => s.clone(),
        None => "q quit  ? help  a add  e edit  d delete  c toggle  Enter expand/toggle  o open  L logs  r reload".into(),
    };
    let p = Paragraph::new(Line::from(Span::styled(
        txt,
        Style::default().fg(Color::White).bg(Color::Blue),
    )));
    f.render_widget(p, area);
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    Rect { x, y, width: w.min(area.width), height: h.min(area.height) }
}

fn draw_form(f: &mut Frame, form: &FormModal, area: Rect) {
    let w = 60.min(area.width.saturating_sub(4));
    let h = (form.fields.len() as u16 * 2 + 4).min(area.height.saturating_sub(4));
    let r = centered(area, w, h);
    f.render_widget(Clear, r);
    let mut lines: Vec<Line> = Vec::new();
    for (i, field) in form.fields.iter().enumerate() {
        let focused = i == form.focus;
        let label_style = if focused {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        lines.push(Line::from(Span::styled(field.label.clone(), label_style)));
        let value_display = if field.value.is_empty() && !focused {
            Span::styled(field.placeholder.clone(), Style::default().fg(Color::DarkGray))
        } else if focused {
            Span::styled(format!("{}_", field.value), Style::default().fg(Color::White).bg(Color::DarkGray))
        } else {
            Span::raw(field.value.clone())
        };
        lines.push(Line::from(vec![Span::raw("  "), value_display]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Tab/↑↓ field  Enter submit  Esc cancel",
        Style::default().fg(Color::DarkGray),
    )));
    let block = Block::default().borders(Borders::ALL).title(form.title.clone());
    let p = Paragraph::new(lines).block(block);
    f.render_widget(p, r);
}

fn draw_confirm(f: &mut Frame, c: &ConfirmModal, area: Rect) {
    let w = 60.min(area.width.saturating_sub(4));
    let r = centered(area, w, 5);
    f.render_widget(Clear, r);
    let p = Paragraph::new(c.message.clone())
        .block(Block::default().borders(Borders::ALL).title("confirm"))
        .wrap(Wrap { trim: true });
    f.render_widget(p, r);
}

fn draw_error(f: &mut Frame, msg: &str, area: Rect) {
    let w = 60.min(area.width.saturating_sub(4));
    let r = centered(area, w, 6);
    f.render_widget(Clear, r);
    let p = Paragraph::new(vec![
        Line::from(Span::styled(msg.to_string(), Style::default().fg(Color::Red))),
        Line::from(""),
        Line::from(Span::styled("press any key", Style::default().fg(Color::DarkGray))),
    ])
    .block(Block::default().borders(Borders::ALL).title("error"))
    .wrap(Wrap { trim: false });
    f.render_widget(p, r);
}

fn draw_help(f: &mut Frame, area: Rect) {
    let w = 60.min(area.width.saturating_sub(4));
    let r = centered(area, w, 18);
    f.render_widget(Clear, r);
    let lines = vec![
        Line::from("Movement"),
        Line::from("  j/k or ↓/↑    move selection"),
        Line::from("  h/l or ←/→    collapse/expand host"),
        Line::from(""),
        Line::from("Tunnels"),
        Line::from("  c             toggle connection (host = cascade)"),
        Line::from("  Enter         host: expand/collapse  service: toggle conn"),
        Line::from("  o             open service in browser (connects if needed)"),
        Line::from(""),
        Line::from("Edit"),
        Line::from("  a             add (service to current host)"),
        Line::from("  A             add host"),
        Line::from("  e             edit selected"),
        Line::from("  d             delete selected"),
        Line::from(""),
        Line::from("Misc"),
        Line::from("  L  logs   r  reload   q  quit   ?  this help"),
    ];
    let p = Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("help"));
    f.render_widget(p, r);
}
