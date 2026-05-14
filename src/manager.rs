use crate::config::Service;
use crate::tunnel::{TunnelEvent, TunnelHandle, TunnelId, make_id, spawn_tunnel};
use std::collections::{HashMap, VecDeque};
use tokio::sync::mpsc;

const LOG_CAP: usize = 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnState {
    Disconnected,
    Connecting,
    Connected { local_port: u16 },
    Disconnecting,
    Failed { reason: String },
}

impl ConnState {
    pub fn is_connected(&self) -> bool {
        matches!(self, ConnState::Connected { .. })
    }
    pub fn is_transient(&self) -> bool {
        matches!(self, ConnState::Connecting | ConnState::Disconnecting)
    }
}

pub struct Manager {
    pub events_tx: mpsc::UnboundedSender<TunnelEvent>,
    pub events_rx: mpsc::UnboundedReceiver<TunnelEvent>,
    pub states: HashMap<TunnelId, ConnState>,
    pub handles: HashMap<TunnelId, TunnelHandle>,
    pub logs: VecDeque<LogEntry>,
}

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub id: TunnelId,
    pub line: String,
}

impl Manager {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            events_tx: tx,
            events_rx: rx,
            states: HashMap::new(),
            handles: HashMap::new(),
            logs: VecDeque::new(),
        }
    }

    pub fn state_of(&self, id: &TunnelId) -> ConnState {
        self.states.get(id).cloned().unwrap_or(ConnState::Disconnected)
    }

    pub fn connect(&mut self, host_name: &str, ssh_alias: &str, svc: &Service) {
        let id = make_id(host_name, &svc.name);
        let cur = self.state_of(&id);
        if cur.is_connected() || cur.is_transient() {
            return;
        }
        self.states.insert(id.clone(), ConnState::Connecting);
        let handle = spawn_tunnel(
            id.clone(),
            ssh_alias.to_string(),
            svc.port,
            self.events_tx.clone(),
        );
        self.handles.insert(id, handle);
    }

    pub fn disconnect(&mut self, host_name: &str, svc_name: &str) {
        let id = make_id(host_name, svc_name);
        if let Some(h) = self.handles.get_mut(&id) {
            self.states.insert(id.clone(), ConnState::Disconnecting);
            h.shutdown();
        }
    }

    pub fn toggle_service(&mut self, host_name: &str, ssh_alias: &str, svc: &Service) {
        let id = make_id(host_name, &svc.name);
        match self.state_of(&id) {
            ConnState::Connected { .. } | ConnState::Connecting => {
                self.disconnect(host_name, &svc.name);
            }
            _ => self.connect(host_name, ssh_alias, svc),
        }
    }

    pub fn toggle_host(&mut self, host_name: &str, ssh_alias: &str, services: &[Service]) {
        let any_on = services.iter().any(|s| {
            let id = make_id(host_name, &s.name);
            matches!(
                self.state_of(&id),
                ConnState::Connected { .. } | ConnState::Connecting
            )
        });
        if any_on {
            for s in services {
                self.disconnect(host_name, &s.name);
            }
        } else {
            for s in services {
                self.connect(host_name, ssh_alias, s);
            }
        }
    }

    pub fn apply_event(&mut self, ev: TunnelEvent) {
        match ev {
            TunnelEvent::Connecting { id } => {
                self.states.insert(id, ConnState::Connecting);
            }
            TunnelEvent::Connected { id, local_port } => {
                self.states.insert(id, ConnState::Connected { local_port });
            }
            TunnelEvent::Failed { id, reason } => {
                self.push_log(&id, format!("failed: {reason}"));
                self.handles.remove(&id);
                self.states.insert(id, ConnState::Failed { reason });
            }
            TunnelEvent::Disconnected { id } => {
                self.handles.remove(&id);
                self.states.insert(id, ConnState::Disconnected);
            }
            TunnelEvent::Log { id, line } => {
                self.push_log(&id, line);
            }
        }
    }

    fn push_log(&mut self, id: &TunnelId, line: String) {
        if self.logs.len() >= LOG_CAP {
            self.logs.pop_front();
        }
        self.logs.push_back(LogEntry {
            id: id.clone(),
            line,
        });
    }

    pub fn shutdown_all(&mut self) {
        for (_, h) in self.handles.iter_mut() {
            h.shutdown();
        }
    }

    pub async fn await_all(&mut self) {
        let handles: Vec<_> = self.handles.drain().collect();
        for (_, mut h) in handles {
            h.wait().await;
        }
    }

    pub fn host_aggregate(&self, cfg_host: &str, services: &[Service]) -> HostAggState {
        if services.is_empty() {
            return HostAggState::Empty;
        }
        let mut connected = 0usize;
        let mut transient = false;
        let mut failed = false;
        for s in services {
            let st = self.state_of(&make_id(cfg_host, &s.name));
            match st {
                ConnState::Connected { .. } => connected += 1,
                ConnState::Connecting | ConnState::Disconnecting => transient = true,
                ConnState::Failed { .. } => failed = true,
                ConnState::Disconnected => {}
            }
        }
        if transient {
            HostAggState::Transient
        } else if connected == services.len() {
            HostAggState::AllOn
        } else if connected == 0 && failed {
            HostAggState::Failed
        } else if connected == 0 {
            HostAggState::AllOff
        } else {
            HostAggState::Mixed
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum HostAggState {
    Empty,
    AllOff,
    AllOn,
    Mixed,
    Transient,
    Failed,
}

