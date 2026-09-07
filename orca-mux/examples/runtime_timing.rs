use std::time::Instant;

use anyhow::anyhow;
use orca_mux::LocalRuntime;

fn main() -> anyhow::Result<()> {
    smol::block_on(run())
}

async fn run() -> anyhow::Result<()> {
    let runtime = LocalRuntime::discover().ok_or_else(|| anyhow!("no local orca runtime"))?;
    for _ in 0..2 {
        let t = Instant::now();
        let _ = runtime.worktree_ps().await?;
        eprintln!("worktree.ps       {:?}", t.elapsed());

        let t = Instant::now();
        let _ = runtime.list_all().await?;
        eprintln!("session.listAll   {:?}", t.elapsed());

        let t = Instant::now();
        let agents = runtime.detect_agents().await?;
        eprintln!(
            "detectAgents      {:?}  ({} agents)",
            t.elapsed(),
            agents.len()
        );

        let t = Instant::now();
        let targets = runtime.ssh_target_summaries().await?;
        eprintln!("ssh.summaries     {:?}", t.elapsed());

        for target in &targets {
            let t = Instant::now();
            let _ = runtime.ssh_target_state(&target.id).await?;
            eprintln!("ssh.getState[{}] {:?}", target.label, t.elapsed());
        }
        eprintln!("---");
    }
    Ok(())
}
