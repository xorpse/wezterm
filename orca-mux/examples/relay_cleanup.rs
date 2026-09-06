use std::sync::Arc;

use anyhow::anyhow;
use serde_json::json;

use orca_mux::relay::RelayConnection;

fn main() -> anyhow::Result<()> {
    let target = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow!("usage: relay_cleanup <ssh-target> [--apply]"))?;
    let apply = std::env::args().any(|a| a == "--apply");
    smol::block_on(run(target, apply))
}

async fn run(target: String, apply: bool) -> anyhow::Result<()> {
    let connection = Arc::new(RelayConnection::open(&target).await?);
    connection
        .request(
            "pty.openClient",
            json!({
                "protocolVersion": 1,
                "clientInstanceId": "wezterm-relay-cleanup",
                "requestedRole": "subscriber",
            }),
        )
        .await?;
    let processes = connection.request("pty.listProcesses", json!({})).await?;
    let list = processes.as_array().cloned().unwrap_or_default();
    println!("{} ptys on {target}", list.len());
    for pty in &list {
        let id = pty.get("id").and_then(|v| v.as_str()).unwrap_or("?");
        let owned = pty.get("worktreeId").is_some()
            || pty.get("terminalHandle").is_some()
            || pty.get("agentSessionOwners").is_some();
        let title = pty.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let cwd = pty.get("cwd").and_then(|v| v.as_str()).unwrap_or("");
        let tag = if owned { "KEEP" } else { "orphan" };
        println!("  {id:8} [{tag}] {title}  {cwd}");
        if !owned && apply {
            let _ = connection
                .request("pty.shutdown", json!({ "id": id, "immediate": true }))
                .await;
        }
    }
    if apply {
        println!("shut down orphans (bare spawns with no worktree/terminal/agent)");
    } else {
        println!("dry run; pass --apply to shut down orphans");
    }
    smol::Timer::after(std::time::Duration::from_millis(500)).await;
    Ok(())
}
