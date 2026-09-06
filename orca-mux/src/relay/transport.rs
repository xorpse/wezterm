use anyhow::{Context, anyhow};
use smol::io::{AsyncReadExt, AsyncWriteExt};
use smol::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use super::frame::{Frame, FrameDecoder};

const SENTINEL: &[u8] = b"ORCA-RELAY v0.1.0 READY\n";

const NODE_DISCOVERY: &str = r#"shell_path=$("${SHELL:-/bin/sh}" -l -c env 2>/dev/null </dev/null | sed -n 's/^PATH=//p' | tail -1)
[ -n "$shell_path" ] && PATH="$shell_path:$PATH"
export PATH
node_bin=""
node_major=0
for candidate in node /opt/homebrew/bin/node /usr/local/bin/node /usr/bin/node "$HOME"/.nvm/versions/node/*/bin/node; do
  resolved=$(command -v "$candidate" 2>/dev/null) || resolved="$candidate"
  [ -x "$resolved" ] || continue
  major=$("$resolved" -e 'process.stdout.write(process.versions.node.split(".")[0])' 2>/dev/null) || continue
  if [ "$major" -gt "$node_major" ]; then node_bin="$resolved"; node_major=$major; fi
done"#;

pub struct RelayDaemon {
    pub dir: String,
    pub sock: String,
    pub credential: String,
}

pub async fn discover_all_daemons(target: &str) -> anyhow::Result<Vec<RelayDaemon>> {
    let script = r#"
for s in $(ls -t "$HOME/.orca-remote/"relay-*/relay-*.sock 2>/dev/null); do
  [ -e "$s.credential" ] || continue
  echo "$s"
done
"#;
    let output = ssh_capture(target, script).await?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let daemons = stdout
        .lines()
        .filter(|line| !line.is_empty())
        .map(|sock| {
            let dir = std::path::Path::new(sock)
                .parent()
                .map(|parent| format!("{}/", parent.display()))
                .unwrap_or_default();
            RelayDaemon {
                dir,
                sock: sock.to_owned(),
                credential: format!("{sock}.credential"),
            }
        })
        .collect::<Vec<_>>();
    Ok(daemons)
}

pub async fn discover_daemon(target: &str) -> anyhow::Result<RelayDaemon> {
    let script = r#"
found=""
for s in $(ls -t "$HOME/.orca-remote/"relay-*/relay-*.sock 2>/dev/null); do
  [ -e "$s.credential" ] || continue
  found="$s"
  break
done
[ -n "$found" ] || { echo NO_RELAY; exit 0; }
dir=$(dirname "$found")/
echo "$dir"; echo "$found"; echo "$found.credential"
"#;
    let output = ssh_capture(target, script).await?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut lines = stdout.lines();
    match lines.next() {
        Some("NO_RELAY") => Err(anyhow!("no orca relay is installed on {target}")),
        Some("NO_SOCK") => Err(anyhow!("no live relay daemon socket found on {target}")),
        Some(dir) => {
            let sock = lines
                .next()
                .ok_or_else(|| anyhow!("relay discovery on {target} returned no socket"))?;
            let credential = lines
                .next()
                .ok_or_else(|| anyhow!("relay discovery on {target} returned no credential"))?;
            Ok(RelayDaemon {
                dir: dir.to_owned(),
                sock: sock.to_owned(),
                credential: credential.to_owned(),
            })
        }
        None => Err(anyhow!("relay discovery on {target} produced no output")),
    }
}

pub struct RelayReader {
    stdout: ChildStdout,
    decoder: FrameDecoder,
    read_buffer: Vec<u8>,
}

pub struct RelayWriter {
    child: Child,
    stdin: ChildStdin,
}

pub async fn connect(
    target: &str,
    daemon: &RelayDaemon,
) -> anyhow::Result<(RelayReader, RelayWriter)> {
    let remote = format!(
        "{NODE_DISCOVERY}\n[ -n \"$node_bin\" ] || {{ echo 'no node runtime on host' >&2; exit 5; }}\ncd {dir} && exec \"$node_bin\" relay.js --connect --sock-path {sock} --credential-file {cred}",
        dir = shell_quote(&daemon.dir),
        sock = shell_quote(&daemon.sock),
        cred = shell_quote(&daemon.credential),
    );
    let mut child = Command::new("ssh")
        .arg("-oBatchMode=yes")
        .arg("-oConnectTimeout=10")
        .arg(target)
        .arg(format!("sh -c {}", shell_quote(&remote)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("spawning ssh relay --connect")?;
    let stdin = child.stdin.take().expect("piped stdin");
    let mut stdout = child.stdout.take().expect("piped stdout");
    wait_for_sentinel(&mut stdout).await?;
    Ok((
        RelayReader {
            stdout,
            decoder: FrameDecoder::new(),
            read_buffer: vec![0u8; 64 * 1024],
        },
        RelayWriter { child, stdin },
    ))
}

impl RelayReader {
    pub async fn recv(&mut self) -> anyhow::Result<Frame> {
        loop {
            if let Some(frame) = self.decoder.next_frame()? {
                return Ok(frame);
            }
            let read = self.stdout.read(&mut self.read_buffer).await?;
            if read == 0 {
                anyhow::bail!("relay connection closed");
            }
            self.decoder.feed(&self.read_buffer[..read]);
        }
    }
}

impl RelayWriter {
    pub async fn send(&mut self, frame: &Frame) -> anyhow::Result<()> {
        self.stdin.write_all(&frame.encode()).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    pub async fn shutdown(mut self) {
        let _ = self.stdin.close().await;
        let _ = self.child.kill();
    }
}

async fn wait_for_sentinel(stdout: &mut ChildStdout) -> anyhow::Result<()> {
    let mut seen = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let read = stdout.read(&mut byte).await?;
        if read == 0 {
            anyhow::bail!(
                "relay closed before the ready sentinel (saw {:?})",
                String::from_utf8_lossy(&seen)
            );
        }
        seen.push(byte[0]);
        if seen.ends_with(SENTINEL) {
            return Ok(());
        }
        if seen.len() > SENTINEL.len() * 4 {
            seen.drain(..seen.len() - SENTINEL.len());
        }
    }
}

async fn ssh_capture(target: &str, script: &str) -> anyhow::Result<std::process::Output> {
    Ok(Command::new("ssh")
        .arg("-oBatchMode=yes")
        .arg("-oConnectTimeout=10")
        .arg(target)
        .arg(format!("sh -c {}", shell_quote(script)))
        .stderr(Stdio::inherit())
        .output()
        .await?)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}
