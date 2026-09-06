use anyhow::anyhow;

use orca_mux::LocalRuntime;

fn main() -> anyhow::Result<()> {
    smol::block_on(run())
}

async fn run() -> anyhow::Result<()> {
    let runtime = LocalRuntime::discover().ok_or_else(|| anyhow!("no local orca runtime found"))?;
    eprintln!("[probe] local runtime {}", runtime.runtime_id());

    let targets = runtime.ssh_targets().await?;
    eprintln!("[probe] ssh targets: {}", targets);

    match runtime.worktree_ps().await {
        Ok(worktrees) => {
            eprintln!("[probe] worktree.ps -> {} worktrees:", worktrees.len());
            for wt in &worktrees {
                eprintln!(
                    "    {} [{}] {} live={}",
                    wt.display_name, wt.branch, wt.path, wt.live_terminal_count
                );
            }
        }
        Err(err) => eprintln!("[probe] worktree.ps FAILED: {err:#}"),
    }
    match runtime.detect_agents().await {
        Ok(agents) => eprintln!("[probe] detectAgents -> {agents:?}"),
        Err(err) => eprintln!("[probe] detectAgents FAILED: {err:#}"),
    }

    let session = runtime.list_all().await?;
    if let Some(snapshots) = session.get("snapshots").and_then(|v| v.as_array()) {
        eprintln!("[probe] {} worktree snapshots:", snapshots.len());
        for snap in snapshots {
            let wt = snap.get("worktree").and_then(|v| v.as_str()).unwrap_or("?");
            let tabs = snap.get("tabs").and_then(|v| v.as_array());
            let count = tabs.map(|a| a.len()).unwrap_or(0);
            eprintln!("  {wt}  tabs={count}");
            let Some(tabs) = tabs else { continue };
            for tab in tabs {
                let parent = tab.get("parentTabId").and_then(|v| v.as_str()).unwrap_or("?");
                let leaf = tab.get("leafId").and_then(|v| v.as_str()).unwrap_or("?");
                let pty = tab.get("ptyId").and_then(|v| v.as_str()).unwrap_or("?");
                eprintln!("    parent={parent} leaf={leaf} pty={pty}");
                if let Some(layout) = tab.get("parentLayout") {
                    eprintln!("      parentLayout={layout}");
                }
            }
        }
    }
    Ok(())
}
