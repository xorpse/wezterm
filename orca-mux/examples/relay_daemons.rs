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
                for process in processes.iter().take(2) {
                    eprintln!(
                        "    keys={:?}",
                        process.as_object().map(|o| o.keys().collect::<Vec<_>>())
                    );
                    eprintln!("    full={process}");
                }
            }
            Err(err) => eprintln!("  listProcesses failed: {err:#}"),
        }
    }
    Ok(())
}
