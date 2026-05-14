use anyhow::{Context, Result};
use std::net::TcpListener;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, timeout};

pub type TunnelId = String;

pub fn make_id(host: &str, service: &str) -> TunnelId {
    format!("{}/{}", host, service)
}

#[derive(Debug, Clone)]
pub enum TunnelEvent {
    Connecting { id: TunnelId },
    Connected { id: TunnelId, local_port: u16 },
    Failed { id: TunnelId, reason: String },
    Disconnected { id: TunnelId },
    Log { id: TunnelId, line: String },
}

pub struct TunnelHandle {
    cancel: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl TunnelHandle {
    pub fn shutdown(&mut self) {
        if let Some(tx) = self.cancel.take() {
            let _ = tx.send(());
        }
    }

    pub async fn wait(&mut self) {
        if let Some(t) = self.task.take() {
            let _ = t.await;
        }
    }
}

const READY_TIMEOUT: Duration = Duration::from_secs(8);
const READY_POLL: Duration = Duration::from_millis(50);

fn pick_local_port() -> Result<u16> {
    let l = TcpListener::bind("127.0.0.1:0").context("bind ephemeral port")?;
    let port = l.local_addr()?.port();
    drop(l);
    Ok(port)
}

pub fn spawn_tunnel(
    id: TunnelId,
    ssh_alias: String,
    remote_port: u16,
    events: mpsc::UnboundedSender<TunnelEvent>,
) -> TunnelHandle {
    let (cancel_tx, cancel_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        run_tunnel(id, ssh_alias, remote_port, events, cancel_rx).await;
    });
    TunnelHandle {
        cancel: Some(cancel_tx),
        task: Some(task),
    }
}

async fn run_tunnel(
    id: TunnelId,
    ssh_alias: String,
    remote_port: u16,
    events: mpsc::UnboundedSender<TunnelEvent>,
    mut cancel: oneshot::Receiver<()>,
) {
    let _ = events.send(TunnelEvent::Connecting { id: id.clone() });

    let local_port = match pick_local_port() {
        Ok(p) => p,
        Err(e) => {
            let _ = events.send(TunnelEvent::Failed {
                id,
                reason: format!("port alloc: {e}"),
            });
            return;
        }
    };

    let mut child = match spawn_ssh(&ssh_alias, local_port, remote_port) {
        Ok(c) => c,
        Err(e) => {
            let _ = events.send(TunnelEvent::Failed {
                id,
                reason: format!("spawn ssh: {e}"),
            });
            return;
        }
    };

    if let Some(stderr) = child.stderr.take() {
        let ev = events.clone();
        let id2 = id.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = ev.send(TunnelEvent::Log {
                    id: id2.clone(),
                    line,
                });
            }
        });
    }

    let ready = wait_ready(local_port);
    tokio::select! {
        _ = &mut cancel => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            let _ = events.send(TunnelEvent::Disconnected { id });
            return;
        }
        r = ready => {
            match r {
                Ok(()) => {
                    let _ = events.send(TunnelEvent::Connected { id: id.clone(), local_port });
                }
                Err(e) => {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    let _ = events.send(TunnelEvent::Failed { id, reason: e });
                    return;
                }
            }
        }
        status = child.wait() => {
            let msg = match status {
                Ok(s) => format!("ssh exited early: {s}"),
                Err(e) => format!("ssh wait: {e}"),
            };
            let _ = events.send(TunnelEvent::Failed { id, reason: msg });
            return;
        }
    }

    tokio::select! {
        _ = &mut cancel => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            let _ = events.send(TunnelEvent::Disconnected { id });
        }
        status = child.wait() => {
            let msg = match status {
                Ok(s) if s.success() => "ssh exited".to_string(),
                Ok(s) => format!("ssh exited: {s}"),
                Err(e) => format!("ssh wait: {e}"),
            };
            let _ = events.send(TunnelEvent::Failed { id, reason: msg });
        }
    }
}

fn spawn_ssh(alias: &str, local: u16, remote: u16) -> Result<Child> {
    let child = Command::new("ssh")
        .args([
            "-N",
            "-o",
            "BatchMode=yes",
            "-o",
            "ExitOnForwardFailure=yes",
            "-o",
            "ServerAliveInterval=30",
            "-o",
            "ServerAliveCountMax=3",
            "-L",
            &format!("127.0.0.1:{local}:127.0.0.1:{remote}"),
            alias,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("spawn ssh")?;
    Ok(child)
}

async fn wait_ready(port: u16) -> Result<(), String> {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        if Instant::now() >= deadline {
            return Err(format!("readiness timeout after {:?}", READY_TIMEOUT));
        }
        match timeout(Duration::from_millis(500), TcpStream::connect(("127.0.0.1", port))).await {
            Ok(Ok(_)) => return Ok(()),
            _ => sleep(READY_POLL).await,
        }
    }
}
