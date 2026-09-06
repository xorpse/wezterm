use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use anyhow::anyhow;
use parking_lot::Mutex;
use serde_json::{Value, json};

use super::frame::{Frame, FrameType};
use super::transport::{self, RelayDaemon, RelayReader, RelayWriter};

const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);

pub struct Notification {
    pub method: String,
    pub params: Value,
}

type Routes = Arc<Mutex<HashMap<String, flume::Sender<Notification>>>>;

pub const DEFAULT_WINDOW_SU: u64 = 4 * 1024 * 1024;

#[derive(Clone)]
pub struct RelayConnection {
    outgoing: flume::Sender<Frame>,
    notifications: flume::Receiver<Notification>,
    routes: Routes,
    pending: Arc<Mutex<HashMap<u32, flume::Sender<Value>>>>,
    next_id: Arc<AtomicU32>,
    last_recv: Arc<AtomicU32>,
    closed: Arc<AtomicBool>,
}

impl RelayConnection {
    pub async fn open(target: &str) -> anyhow::Result<RelayConnection> {
        let daemon = transport::discover_daemon(target).await?;
        Self::open_daemon(target, &daemon).await
    }

    pub async fn open_daemon(
        target: &str,
        daemon: &RelayDaemon,
    ) -> anyhow::Result<RelayConnection> {
        let (reader, writer) = transport::connect(target, daemon).await?;
        let (outgoing_tx, outgoing_rx) = flume::unbounded::<Frame>();
        let (notif_tx, notif_rx) = flume::unbounded::<Notification>();
        let routes: Routes = Arc::new(Mutex::new(HashMap::new()));
        let pending = Arc::new(Mutex::new(HashMap::<u32, flume::Sender<Value>>::new()));
        let next_id = Arc::new(AtomicU32::new(1));
        let last_recv = Arc::new(AtomicU32::new(0));
        let closed = Arc::new(AtomicBool::new(false));

        smol::spawn(writer_loop(writer, outgoing_rx)).detach();
        smol::spawn(reader_loop(
            reader,
            pending.clone(),
            routes.clone(),
            notif_tx,
            last_recv.clone(),
            closed.clone(),
        ))
        .detach();
        smol::spawn(keepalive_loop(
            outgoing_tx.clone(),
            next_id.clone(),
            last_recv.clone(),
        ))
        .detach();

        Ok(RelayConnection {
            outgoing: outgoing_tx,
            notifications: notif_rx,
            routes,
            pending,
            next_id,
            last_recv,
            closed,
        })
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    pub fn route_pty(&self, id: &str) -> flume::Receiver<Notification> {
        let (tx, rx) = flume::unbounded::<Notification>();
        self.routes.lock().insert(id.to_owned(), tx);
        rx
    }

    pub fn unroute_pty(&self, id: &str) {
        self.routes.lock().remove(id);
    }

    pub async fn open_client(&self, role: &str, window_su: u64) -> anyhow::Result<Value> {
        self.request(
            "pty.openClient",
            json!({
                "protocolVersion": 1,
                "clientInstanceId": "wezterm-relay",
                "requestedRole": role,
                "capabilities": { "outputFlowControl": { "versions": [1], "requestedWindowSu": window_su } },
            }),
        )
        .await
    }

    pub async fn spawn_pty(
        &self,
        cols: u16,
        rows: u16,
        cwd: Option<&str>,
    ) -> anyhow::Result<String> {
        let mut params = json!({ "cols": cols, "rows": rows });
        if let Some(cwd) = cwd {
            params["cwd"] = json!(cwd);
        }
        let spawned = self.request("pty.spawn", params).await?;
        spawned
            .get("id")
            .and_then(|value| value.as_str())
            .map(|value| value.to_owned())
            .ok_or_else(|| anyhow!("pty.spawn returned no id"))
    }

    pub async fn attach_pty(&self, id: &str) -> anyhow::Result<Value> {
        self.request("pty.attach", json!({ "id": id, "requireReplay": true }))
            .await
    }

    pub async fn pty_size(&self, id: &str) -> anyhow::Result<Option<(u16, u16)>> {
        let size = self.request("pty.getSize", json!({ "id": id })).await?;
        let cols = size.get("cols").and_then(|value| value.as_u64());
        let rows = size.get("rows").and_then(|value| value.as_u64());
        Ok(match (cols, rows) {
            (Some(cols), Some(rows)) => Some((cols as u16, rows as u16)),
            _ => None,
        })
    }

    pub async fn list_processes(&self) -> anyhow::Result<Vec<Value>> {
        Ok(self
            .request("pty.listProcesses", json!({}))
            .await?
            .as_array()
            .cloned()
            .unwrap_or_default())
    }

    pub async fn input(&self, id: &str, data: &str) -> anyhow::Result<()> {
        self.notify("pty.data", json!({ "id": id, "data": data }))
            .await
    }

    pub async fn resize_pty(&self, id: &str, cols: u16, rows: u16) -> anyhow::Result<()> {
        self.notify(
            "pty.resize",
            json!({ "id": id, "cols": cols, "rows": rows }),
        )
        .await
    }

    pub async fn shutdown_pty(&self, id: &str) -> anyhow::Result<()> {
        self.request("pty.shutdown", json!({ "id": id, "immediate": false }))
            .await
            .map(|_| ())
    }

    pub async fn ack_data(&self, id: &str, frame: &Value) -> anyhow::Result<()> {
        let Some(end_su) = frame.get("sourceEndSu").and_then(|value| value.as_u64()) else {
            return Ok(());
        };
        self.notify(
            "pty.ackData",
            json!({
                "id": id,
                "clientGeneration": frame.get("clientGeneration"),
                "ownerGeneration": frame.get("ownerGeneration"),
                "deliveryToken": frame.get("deliveryToken"),
                "creditedEndSu": end_su,
            }),
        )
        .await
    }

    fn ack(&self) -> u32 {
        self.last_recv.load(Ordering::SeqCst)
    }

    fn alloc_id(&self) -> u32 {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }

    pub async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.alloc_id();
        let (tx, rx) = flume::bounded::<Value>(1);
        self.pending.lock().insert(id, tx);
        let payload = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))?;
        self.outgoing
            .send_async(Frame::new(FrameType::Regular, id, self.ack(), payload))
            .await
            .map_err(|_| anyhow!("relay connection is closed"))?;
        let response = rx
            .recv_async()
            .await
            .map_err(|_| anyhow!("relay connection closed before {method} responded"))?;
        if let Some(error) = response.get("error") {
            anyhow::bail!("relay {method} failed: {error}");
        }
        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }

    pub async fn notify(&self, method: &str, params: Value) -> anyhow::Result<()> {
        let id = self.alloc_id();
        let payload = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))?;
        self.outgoing
            .send_async(Frame::new(FrameType::Regular, id, self.ack(), payload))
            .await
            .map_err(|_| anyhow!("relay connection is closed"))?;
        Ok(())
    }

    pub async fn next_notification(&self) -> anyhow::Result<Notification> {
        self.notifications
            .recv_async()
            .await
            .map_err(|_| anyhow!("relay connection closed"))
    }
}

async fn writer_loop(mut writer: RelayWriter, outgoing: flume::Receiver<Frame>) {
    while let Ok(frame) = outgoing.recv_async().await {
        if writer.send(&frame).await.is_err() {
            break;
        }
    }
    writer.shutdown().await;
}

async fn reader_loop(
    mut reader: RelayReader,
    pending: Arc<Mutex<HashMap<u32, flume::Sender<Value>>>>,
    routes: Routes,
    notifications: flume::Sender<Notification>,
    last_recv: Arc<AtomicU32>,
    closed: Arc<AtomicBool>,
) {
    loop {
        let frame = match reader.recv().await {
            Ok(frame) => frame,
            Err(_) => break,
        };
        if frame.id != 0 {
            last_recv.fetch_max(frame.id, Ordering::SeqCst);
        }
        if frame.kind != FrameType::Regular {
            continue;
        }
        let value = match serde_json::from_slice::<Value>(&frame.payload) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let is_response = value.get("id").and_then(Value::as_u64).is_some()
            && (value.get("result").is_some() || value.get("error").is_some());
        if is_response {
            let id = value.get("id").and_then(Value::as_u64).unwrap_or(0) as u32;
            if let Some(tx) = pending.lock().remove(&id) {
                let _ = tx.send(value);
            }
        } else if let Some(method) = value.get("method").and_then(Value::as_str) {
            let notification = Notification {
                method: method.to_owned(),
                params: value.get("params").cloned().unwrap_or(Value::Null),
            };
            let routed = notification
                .params
                .get("id")
                .and_then(Value::as_str)
                .and_then(|id| routes.lock().get(id).cloned());
            match routed {
                Some(tx) => {
                    let _ = tx.send_async(notification).await;
                }
                None => {
                    if notifications.send_async(notification).await.is_err() {
                        break;
                    }
                }
            }
        }
    }
    closed.store(true, Ordering::SeqCst);
    for (_, tx) in routes.lock().drain() {
        drop(tx);
    }
}

async fn keepalive_loop(
    outgoing: flume::Sender<Frame>,
    next_id: Arc<AtomicU32>,
    last_recv: Arc<AtomicU32>,
) {
    loop {
        smol::Timer::after(KEEPALIVE_INTERVAL).await;
        let id = next_id.fetch_add(1, Ordering::SeqCst);
        let ack = last_recv.load(Ordering::SeqCst);
        if outgoing
            .send_async(Frame::new(FrameType::KeepAlive, id, ack, Vec::new()))
            .await
            .is_err()
        {
            break;
        }
    }
}
