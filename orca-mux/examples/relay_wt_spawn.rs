use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use orca_mux::relay::{DEFAULT_WINDOW_SU, RelayConnection};
use serde_json::json;

fn main() -> anyhow::Result<()> {
    let target = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow!("usage: relay_wt_spawn <ssh-target>"))?;
    smol::block_on(run(target))
}

async fn run(target: String) -> anyhow::Result<()> {
    let connection = Arc::new(RelayConnection::open(&target).await?);
    connection
        .open_client("subscriber", DEFAULT_WINDOW_SU)
        .await?;
    let spawned = connection
        .request(
            "pty.spawn",
            json!({ "cols": 80, "rows": 24, "worktreeId": "sync-test-worktree" }),
        )
        .await?;
    let id = spawned.get("id").and_then(|v| v.as_str()).unwrap_or("?");
    eprintln!("[wt_spawn] spawned worktree pty {id}; holding 15s so wezterm can poll it");
    connection.attach_pty(id).await?;
    smol::Timer::after(Duration::from_secs(15)).await;
    let _ = connection
        .request("pty.shutdown", json!({ "id": id, "immediate": true }))
        .await;
    eprintln!("[wt_spawn] shut down {id}");
    Ok(())
}
