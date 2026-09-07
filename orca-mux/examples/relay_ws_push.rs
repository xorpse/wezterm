use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use orca_mux::relay::{DEFAULT_WINDOW_SU, RelayConnection};
use serde_json::json;

fn main() -> anyhow::Result<()> {
    let target = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow!("usage: relay_ws_push <ssh-target> <namespace>"))?;
    let namespace = std::env::args()
        .nth(2)
        .ok_or_else(|| anyhow!("need a namespace"))?;
    smol::block_on(run(target, namespace))
}

async fn run(target: String, namespace: String) -> anyhow::Result<()> {
    let connection = Arc::new(RelayConnection::open(&target).await?);
    connection
        .open_client("subscriber", DEFAULT_WINDOW_SU)
        .await?;

    let snapshot = connection
        .request("workspace.get", json!({ "namespace": namespace }))
        .await?;
    let revision = snapshot
        .get("revision")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let mut session = snapshot
        .get("session")
        .cloned()
        .unwrap_or_else(|| json!({}));
    eprintln!("[push] namespace {namespace} revision {revision}");

    // Learn the app's target id from an existing ptyId.
    let target_id = session
        .get("tabsByWorktreePath")
        .and_then(|v| v.as_object())
        .and_then(|m| m.values().next())
        .and_then(|arr| arr.as_array())
        .and_then(|arr| arr.first())
        .and_then(|t| t.get("ptyId"))
        .and_then(|p| p.as_str())
        .and_then(|p| p.strip_prefix("ssh:"))
        .and_then(|r| r.split_once("@@"))
        .map(|(t, _)| t.to_owned())
        .ok_or_else(|| anyhow!("could not learn target id from snapshot"))?;
    eprintln!("[push] app target id: {target_id}");

    let worktree = "/Users/slt/orca/workspaces/fugue-core/nblah";
    let pty_id = format!("ssh:{target_id}@@pty-8");
    let leaf = "wz-probe-leaf";
    let tab_id = "wz-probe-tab";
    let tab = json!({ "id": tab_id, "ptyId": pty_id, "title": "WEZTERM-PROBE" });

    session["tabsByWorktreePath"][worktree]
        .as_array_mut()
        .map(|arr| arr.push(tab.clone()));
    if !session["tabsByWorktreePath"][worktree].is_array() {
        session["tabsByWorktreePath"][worktree] = json!([tab]);
    }
    session["terminalLayoutsByTabId"][tab_id] = json!({
        "root": { "type": "leaf", "leafId": leaf },
        "activeLeafId": leaf,
        "ptyIdsByLeafId": { leaf: pty_id },
    });

    let result = connection
        .request(
            "workspace.patch",
            json!({
                "namespace": namespace,
                "baseRevision": revision,
                "patch": { "kind": "replace-session", "session": session },
                "clientId": "wezterm-probe",
            }),
        )
        .await?;
    eprintln!(
        "[push] patch result: {}",
        &result.to_string()[..result.to_string().len().min(200)]
    );
    eprintln!(
        "[push] >>> LOOK AT YOUR ORCA APP: did a 'WEZTERM-PROBE' terminal tab appear under nblah? <<<"
    );
    smol::Timer::after(Duration::from_secs(2)).await;
    Ok(())
}
