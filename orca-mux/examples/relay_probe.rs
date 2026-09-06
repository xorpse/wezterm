use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use serde_json::json;

use orca_mux::relay::RelayConnection;

fn main() -> anyhow::Result<()> {
    let target = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow!("usage: relay_probe <ssh-target>"))?;
    smol::block_on(run(target))
}

async fn run(target: String) -> anyhow::Result<()> {
    eprintln!("[probe] discovering + connecting to relay on {target}");
    let connection = Arc::new(RelayConnection::open(&target).await?);
    eprintln!("[probe] connected");

    let hello = connection
        .request(
            "pty.openClient",
            json!({
                "protocolVersion": 1,
                "clientInstanceId": "wezterm-relay-probe",
                "requestedRole": "session-owner",
                "capabilities": { "outputFlowControl": { "versions": [1], "requestedWindowSu": 10000000 } },
            }),
        )
        .await?;
    eprintln!("[probe] openClient -> {hello}");

    let spawned = connection
        .request("pty.spawn", json!({ "cols": 100, "rows": 30 }))
        .await?;
    eprintln!("[probe] spawn -> {spawned}");
    let pty_id = spawned
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("no pty id"))?
        .to_owned();

    let attached = connection
        .request("pty.attach", json!({ "id": pty_id }))
        .await?;
    eprintln!("[probe] attach -> {attached}");
    let activation = attached
        .get("sourceActivation")
        .cloned()
        .or_else(|| spawned.get("sourceActivation").cloned())
        .ok_or_else(|| anyhow!("no sourceActivation"))?;
    let delivery_token = activation
        .get("deliveryToken")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();
    let client_generation = activation
        .get("clientGeneration")
        .cloned()
        .unwrap_or(json!(0));
    let owner_generation = activation
        .get("ownerGeneration")
        .cloned()
        .unwrap_or(json!(0));

    let collected = Arc::new(parking_lot::Mutex::new(String::new()));
    let collector = {
        let connection = connection.clone();
        let pty_id = pty_id.clone();
        let collected = collected.clone();
        smol::spawn(async move {
            loop {
                match connection.next_notification().await {
                    Ok(n) if n.method == "pty.data" => {
                        if n.params.get("id").and_then(|v| v.as_str()) == Some(&pty_id) {
                            if let Some(data) = n.params.get("data").and_then(|v| v.as_str()) {
                                collected.lock().push_str(data);
                            }
                            if let Some(end_su) =
                                n.params.get("sourceEndSu").and_then(|v| v.as_u64())
                            {
                                let _ = &client_generation;
                                let _ = &owner_generation;
                                let _ = &delivery_token;
                                let _ = connection
                                    .notify(
                                        "pty.ackData",
                                        json!({
                                            "id": pty_id,
                                            "clientGeneration": n.params.get("clientGeneration"),
                                            "ownerGeneration": n.params.get("ownerGeneration"),
                                            "deliveryToken": n.params.get("deliveryToken"),
                                            "creditedEndSu": end_su,
                                        }),
                                    )
                                    .await;
                                eprintln!("[probe] ack creditedEndSu={end_su}");
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        })
    };

    smol::Timer::after(Duration::from_millis(500)).await;
    eprintln!("[probe] sending command");
    connection
        .notify(
            "pty.data",
            json!({ "id": pty_id, "data": "echo relay-works-$((3+4))\n" }),
        )
        .await?;

    smol::Timer::after(Duration::from_secs(3)).await;
    collector.cancel().await;
    let output = collected.lock().clone();
    eprintln!("[probe] ---- collected {} bytes ----", output.len());
    println!("{output}");
    if output.contains("relay-works-7") {
        eprintln!("[probe] SUCCESS: command executed on the remote pty");
    } else {
        eprintln!("[probe] no expected output seen");
    }
    Ok(())
}
