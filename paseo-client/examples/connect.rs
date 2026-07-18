use paseo_client::{parse_offer_url, CreateTerminalOpts, PaseoClient, TerminalStreamEvent};
use std::time::Duration;

const CLIENT_ID: &str = "paseo-wezterm-example";

struct Options {
    stream: bool,
    probe: bool,
    restore: String,
    create_probe: Option<String>,
}

fn usage() -> anyhow::Error {
    anyhow::anyhow!(
        "usage:\n  connect <pairing-offer-url> [--stream] [--probe]\n  connect --local <host:port> [--tls] [--password <pw>] [--stream] [--probe]\n\n  --stream  subscribe to the first terminal and print output for ~3s (read-only)\n  --probe   additionally send a test keystroke and a resize (mutates that terminal)"
    )
}

async fn connect_from_args(args: &[String]) -> anyhow::Result<(PaseoClient, Options)> {
    let mut iter = args.iter().skip(1).peekable();
    let first = iter.next().ok_or_else(usage)?.clone();

    let mut host_port: Option<String> = None;
    let mut use_tls = false;
    let mut password: Option<String> = None;
    let mut options = Options {
        stream: false,
        probe: false,
        restore: "live".to_string(),
        create_probe: None,
    };

    if first == "--local" {
        host_port = Some(iter.next().ok_or_else(usage)?.clone());
    }

    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--tls" => use_tls = true,
            "--password" => password = iter.next().cloned(),
            "--stream" => options.stream = true,
            "--restore" => {
                options.stream = true;
                options.restore = iter.next().cloned().ok_or_else(usage)?;
            }
            "--probe" => {
                options.stream = true;
                options.probe = true;
            }
            "--create-probe" => {
                options.create_probe = Some(iter.next().cloned().ok_or_else(usage)?);
            }
            other => return Err(anyhow::anyhow!("unknown flag: {other}")),
        }
    }

    let client = if let Some(host_port) = host_port {
        println!("connecting to local daemon {host_port} (tls={use_tls})");
        PaseoClient::connect_local(&host_port, use_tls, password.as_deref(), CLIENT_ID).await?
    } else {
        let offer = parse_offer_url(&first)?;
        println!(
            "connecting via relay {} (serverId={})",
            offer.relay.endpoint, offer.server_id
        );
        PaseoClient::connect_relay(&offer, CLIENT_ID).await?
    };

    Ok((client, options))
}

async fn run_script(client: PaseoClient, options: Options) -> anyhow::Result<()> {
    let info = client.server_info();
    println!(
        "server_info: serverId={} version={:?} hostname={:?}",
        info.server_id, info.version, info.hostname
    );
    let mut features: Vec<&String> = info.features.keys().collect();
    features.sort();
    println!("features: {features:?}");

    let agents = client.fetch_agents().await?;
    println!("agents: {}", agents.len());
    for entry in &agents {
        let agent = &entry.agent;
        println!(
            "  {} [{}] {} ({})",
            agent.id,
            agent.status,
            agent.title.clone().unwrap_or_default(),
            agent.provider
        );
    }

    let terminals = client.list_terminals(None).await?;
    println!("terminals: {}", terminals.len());
    for terminal in &terminals {
        println!(
            "  {} name={} cwd={}",
            terminal.id, terminal.name, terminal.cwd
        );
    }

    if let Some(cwd) = &options.create_probe {
        create_probe(&client, cwd).await?;
    } else if options.stream {
        if let Some(terminal) = terminals.first() {
            stream_terminal(&client, &terminal.id, &options.restore, options.probe).await?;
        } else {
            println!("no terminals to stream");
        }
    }

    client.close().await;
    Ok(())
}

async fn create_probe(client: &PaseoClient, cwd: &str) -> anyhow::Result<()> {
    println!("opening project for {cwd}");
    let workspace_id = client.open_project(cwd).await?;
    println!("workspace {workspace_id}");
    println!("creating throwaway terminal in {cwd}");
    let opts = CreateTerminalOpts {
        name: Some("paseo-wezterm-test".to_string()),
        workspace_id: Some(workspace_id),
        rows: 24,
        cols: 80,
        ..Default::default()
    };
    let terminal = client.create_terminal(cwd, opts).await?;
    println!("created terminal {}", terminal.id);

    let handle = client.subscribe_terminal(&terminal.id, "live").await?;
    let rx = handle.output();

    handle.resize(30, 100).await?;
    handle.send_input(b"echo paseo-wezterm-roundtrip\r").await?;

    let collect = async {
        while let Ok(event) = rx.recv_async().await {
            if let TerminalStreamEvent::Output(bytes) | TerminalStreamEvent::Restore(bytes) = event
            {
                print!("{}", String::from_utf8_lossy(&bytes));
            }
        }
    };
    let timer = async {
        smol::Timer::after(Duration::from_secs(2)).await;
    };
    smol::future::or(collect, timer).await;
    println!();

    handle.unsubscribe().await?;
    println!("killing terminal {}", terminal.id);
    client.kill_terminal(&terminal.id).await?;
    Ok(())
}

async fn stream_terminal(
    client: &PaseoClient,
    terminal_id: &str,
    restore: &str,
    probe: bool,
) -> anyhow::Result<()> {
    println!("subscribing to terminal {terminal_id} for ~3s (restore={restore}, probe={probe})");
    let handle = client.subscribe_terminal(terminal_id, restore).await?;
    let rx = handle.output();

    if probe {
        handle.send_input(b"echo paseo-wezterm-probe\r").await?;
        handle.resize(40, 120).await?;
    }

    let collect = async {
        while let Ok(event) = rx.recv_async().await {
            match event {
                TerminalStreamEvent::Output(bytes) | TerminalStreamEvent::Restore(bytes) => {
                    print!("{}", String::from_utf8_lossy(&bytes));
                }
                TerminalStreamEvent::Snapshot(_) => println!("[snapshot]"),
            }
        }
    };
    let timer = async {
        smol::Timer::after(Duration::from_secs(3)).await;
    };
    smol::future::or(collect, timer).await;
    println!();

    handle.unsubscribe().await?;
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    smol::block_on(async move {
        let (client, options) = connect_from_args(&args).await?;
        let script = run_script(client.clone(), options);
        let driver = async {
            let _ = client.run().await;
            Ok::<(), anyhow::Error>(())
        };
        smol::future::or(script, driver).await
    })
}
