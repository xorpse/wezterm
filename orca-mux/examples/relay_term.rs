use std::io::{Read, Write};
use std::process::Command;
use std::sync::Arc;

use anyhow::anyhow;
use orca_mux::relay::RelayConnection;
use serde_json::{Value, json};

fn main() -> anyhow::Result<()> {
    let target = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow!("usage: relay_term <ssh-target> [cwd]"))?;
    let cwd = std::env::args().nth(2);
    smol::block_on(run(target, cwd))
}

async fn run(target: String, cwd: Option<String>) -> anyhow::Result<()> {
    eprintln!("[relay_term] connecting to the relay daemon on {target} …\r");
    let connection = Arc::new(RelayConnection::open(&target).await?);

    connection
        .request(
            "pty.openClient",
            json!({
                "protocolVersion": 1,
                "clientInstanceId": "wezterm-relay-term",
                "requestedRole": "session-owner",
                "capabilities": { "outputFlowControl": { "versions": [1], "requestedWindowSu": 10000000 } },
            }),
        )
        .await?;

    let (cols, rows) = terminal_size();
    let mut spawn_params = json!({ "cols": cols, "rows": rows });
    if let Some(cwd) = cwd {
        spawn_params["cwd"] = json!(cwd);
    }
    let spawned = connection.request("pty.spawn", spawn_params).await?;
    let pty_id = spawned
        .get("id")
        .and_then(|value| value.as_str())
        .ok_or_else(|| anyhow!("pty.spawn returned no id: {spawned}"))?
        .to_owned();
    let attached = connection
        .request("pty.attach", json!({ "id": pty_id }))
        .await?;
    if let Some(replay) = attached.get("replay").and_then(|v| v.as_str()) {
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(replay.as_bytes());
        let _ = stdout.flush();
    }
    eprintln!("[relay_term] shell on {target} ({cols}x{rows}); Ctrl-] to quit\r");

    let raw = RawMode::enable();

    let output = {
        let connection = connection.clone();
        let pty_id = pty_id.clone();
        smol::spawn(async move {
            let mut stdout = std::io::stdout();
            loop {
                let notification = match connection.next_notification().await {
                    Ok(notification) => notification,
                    Err(_) => break,
                };
                if notification.params.get("id").and_then(|v| v.as_str()) != Some(&pty_id) {
                    continue;
                }
                match notification.method.as_str() {
                    "pty.data" => {
                        if let Some(data) = notification.params.get("data").and_then(|v| v.as_str())
                        {
                            let _ = stdout.write_all(data.as_bytes());
                            let _ = stdout.flush();
                        }
                        ack_credit(&connection, &pty_id, &notification.params).await;
                    }
                    "pty.exit" => break,
                    _ => {}
                }
            }
        })
    };

    let (input_tx, input_rx) = flume::unbounded::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buffer = [0u8; 4096];
        loop {
            match stdin.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if buffer[..read].contains(&0x1d) {
                        let _ = input_tx.send(Vec::new());
                        break;
                    }
                    if input_tx.send(buffer[..read].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    while let Ok(chunk) = input_rx.recv_async().await {
        if chunk.is_empty() {
            break;
        }
        let text = String::from_utf8_lossy(&chunk).into_owned();
        connection
            .notify("pty.data", json!({ "id": pty_id, "data": text }))
            .await?;
    }

    let _ = connection
        .notify("pty.shutdown", json!({ "id": pty_id }))
        .await;
    drop(raw);
    output.cancel().await;
    eprintln!("\r\n[relay_term] closed\r");
    Ok(())
}

async fn ack_credit(connection: &RelayConnection, pty_id: &str, params: &Value) {
    if let Some(end_su) = params.get("sourceEndSu").and_then(|v| v.as_u64()) {
        let _ = connection
            .notify(
                "pty.ackData",
                json!({
                    "id": pty_id,
                    "clientGeneration": params.get("clientGeneration"),
                    "ownerGeneration": params.get("ownerGeneration"),
                    "deliveryToken": params.get("deliveryToken"),
                    "creditedEndSu": end_su,
                }),
            )
            .await;
    }
}

fn terminal_size() -> (u16, u16) {
    if let Ok(output) = Command::new("stty").arg("size").output() {
        let text = String::from_utf8_lossy(&output.stdout);
        let mut parts = text.split_whitespace();
        if let (Some(rows), Some(cols)) = (parts.next(), parts.next()) {
            if let (Ok(rows), Ok(cols)) = (rows.parse(), cols.parse()) {
                return (cols, rows);
            }
        }
    }
    (80, 24)
}

struct RawMode {
    saved: Option<String>,
}

impl RawMode {
    fn enable() -> RawMode {
        let saved = Command::new("stty")
            .arg("-g")
            .stdin(std::process::Stdio::inherit())
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned());
        let _ = Command::new("stty")
            .arg("raw")
            .arg("-echo")
            .stdin(std::process::Stdio::inherit())
            .status();
        RawMode { saved }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        if let Some(saved) = &self.saved {
            let _ = Command::new("stty")
                .arg(saved)
                .stdin(std::process::Stdio::inherit())
                .status();
        }
    }
}
