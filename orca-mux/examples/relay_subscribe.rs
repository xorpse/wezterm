use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use orca_mux::relay::{DEFAULT_WINDOW_SU, RelayConnection};

fn main() -> anyhow::Result<()> {
    let target = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow!("usage: relay_subscribe <ssh-target> [pty-id]"))?;
    let pty = std::env::args().nth(2);
    smol::block_on(run(target, pty))
}

async fn run(target: String, pty: Option<String>) -> anyhow::Result<()> {
    let connection = Arc::new(RelayConnection::open(&target).await?);
    connection
        .open_client("subscriber", DEFAULT_WINDOW_SU)
        .await?;

    let pty_id = match pty {
        Some(id) => id,
        None => {
            let processes = connection.list_processes().await?;
            eprintln!("[subscribe] {} ptys:", processes.len());
            for p in &processes {
                eprintln!(
                    "  {} {} {}",
                    p.get("id").and_then(|v| v.as_str()).unwrap_or("?"),
                    p.get("title").and_then(|v| v.as_str()).unwrap_or(""),
                    p.get("cwd").and_then(|v| v.as_str()).unwrap_or("")
                );
            }
            let owned = processes.iter().find(|p| p.get("worktreeId").is_some());
            owned
                .or_else(|| processes.first())
                .and_then(|p| p.get("id").and_then(|v| v.as_str()))
                .ok_or_else(|| anyhow!("no ptys to subscribe to"))?
                .to_owned()
        }
    };
    eprintln!("[subscribe] subscribing to {pty_id} on {target}");

    let output = connection.route_pty(&pty_id);
    let attached = connection.attach_pty(&pty_id).await?;
    eprintln!(
        "[subscribe] attach keys: {:?}",
        attached.as_object().map(|o| o.keys().collect::<Vec<_>>())
    );
    if let Some(replay) = attached.get("replay").and_then(|v| v.as_str()) {
        eprintln!("[subscribe] replay: {} bytes of scrollback", replay.len());
        let tail = &replay[replay.len().saturating_sub(300)..];
        println!("{tail}");
    }
    eprintln!("---- co-driving an echo to prove shared streaming ----");
    connection
        .input(&pty_id, "echo SUBSCRIBE-TEST-$((5+5))\n")
        .await?;

    let deadline = Duration::from_secs(5);
    let mut live = 0usize;
    while let Ok(Ok(n)) = smol::future::or(async { Ok(output.recv_async().await) }, async {
        smol::Timer::after(deadline).await;
        Err(())
    })
    .await
    {
        if n.method == "pty.data" {
            if let Some(data) = n.params.get("data").and_then(|v| v.as_str()) {
                live += data.len();
                print!("{data}");
            }
            let _ = connection.ack_data(&pty_id, &n.params).await;
        }
    }
    eprintln!(
        "\n[subscribe] received {live} live bytes; done (app not displaced if its shell still works)"
    );
    Ok(())
}
