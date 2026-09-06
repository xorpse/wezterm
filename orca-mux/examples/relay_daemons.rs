use anyhow::anyhow;

use orca_mux::relay::{DEFAULT_WINDOW_SU, RelayConnection, discover_all_daemons};

fn main() -> anyhow::Result<()> {
    let target = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow!("usage: relay_daemons <ssh-target>"))?;
    smol::block_on(run(target))
}

async fn run(target: String) -> anyhow::Result<()> {
    let daemons = discover_all_daemons(&target).await?;
    eprintln!("[daemons] {} on {target}", daemons.len());
    for daemon in &daemons {
        eprintln!("--- {} ---", daemon.sock);
        let relay = match RelayConnection::open_daemon(&target, daemon).await {
            Ok(relay) => relay,
            Err(err) => {
                eprintln!("  open failed: {err:#}");
                continue;
            }
        };
        if let Err(err) = relay.open_client("subscriber", DEFAULT_WINDOW_SU).await {
            eprintln!("  openClient failed: {err:#}");
            continue;
        }
        match relay.list_processes().await {
            Ok(processes) => {
                eprintln!("  {} processes:", processes.len());
                for process in processes {
                    let id = process.get("id").and_then(|v| v.as_str()).unwrap_or("?");
                    let cwd = process.get("cwd").and_then(|v| v.as_str()).unwrap_or("");
                    let title = process.get("title").and_then(|v| v.as_str()).unwrap_or("");
                    let worktree = process
                        .get("worktreeId")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    eprintln!("    {id}  cwd={cwd}  wt={worktree}  title={title}");
                }
            }
            Err(err) => eprintln!("  listProcesses failed: {err:#}"),
        }
    }
    Ok(())
}
