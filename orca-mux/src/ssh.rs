use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::anyhow;
use sha2::{Digest, Sha256};

const ARTIFACTS: [&str; 3] = [
    "orcad.js",
    "daemon-entry.js",
    "parcel-watcher-process-entry.js",
];
pub(crate) struct RemoteRuntime {
    pub remote_port: u16,
    pub pairing_url: String,
}

pub(crate) async fn has_live_relay(target: &str) -> bool {
    let script = r#"
for s in "$HOME/.orca-remote/"relay-*/relay-*.sock; do
  [ -e "$s" ] && [ -e "$s.credential" ] || continue
  if pgrep -u "$(id -u)" -f 'relay.js --detached' >/dev/null 2>&1; then echo yes; exit 0; fi
done
echo no
"#;
    match ssh_output(target, script).await {
        Ok(output) => String::from_utf8_lossy(&output.stdout).contains("yes"),
        Err(_) => false,
    }
}

pub(crate) async fn ensure_remote_runtime(
    target: &str,
    refresh_pairing: bool,
) -> anyhow::Result<RemoteRuntime> {
    match probe(target).await? {
        ProbeResult::Orcad { readiness } => parse_readiness(target, &readiness),
        ProbeResult::App {
            metadata_path,
            dead_version,
        } => match adopt_app_runtime(target, &metadata_path, refresh_pairing).await {
            Ok(runtime) => Ok(runtime),
            Err(err) => {
                log::warn!(
                    "orca runtime on {target} cannot be adopted ({err:#}); \
                     falling back to orcad"
                );
                match dead_version {
                    Some(version) => revive(target, &version).await,
                    None => bootstrap(target).await,
                }
            }
        },
        ProbeResult::Dead { version } => revive(target, &version).await,
        ProbeResult::NoInstall => bootstrap(target).await,
    }
}

async fn revive(target: &str, version: &str) -> anyhow::Result<RemoteRuntime> {
    ensure_native(target, version).await?;
    relaunch(target, version).await?;
    parse_readiness(target, &await_readiness(target, version).await?)
}

async fn bootstrap(target: &str) -> anyhow::Result<RemoteRuntime> {
    let version = deploy(target).await?;
    parse_readiness(target, &await_readiness(target, &version).await?)
}

enum ProbeResult {
    Orcad {
        readiness: String,
    },
    App {
        metadata_path: String,
        dead_version: Option<String>,
    },
    Dead {
        version: String,
    },
    NoInstall,
}

async fn probe(target: &str) -> anyhow::Result<ProbeResult> {
    let script = format!(
        r#"
alive() {{
  [ -n "$1" ] || return 1
  kill -0 "$1" 2>/dev/null && return 0
  kill -0 "$1" 2>&1 | grep -qi "not permitted"
}}
dead=""
if cd "$HOME/.orca-remote" 2>/dev/null; then
  active=$(sed -n 's/.*"active": *"\([^"]*\)".*/\1/p' orcad-active.json 2>/dev/null || true)
  if [ -n "$active" ] && [ -d "orcad-$active" ]; then
    pid=$(cat "orcad-$active/.orcad-pid" 2>/dev/null)
    [ -n "$pid" ] || pid=$(cat "orcad-$active/orcad.pid" 2>/dev/null)
    if alive "$pid"; then
      echo orcad
      head -1 "orcad-$active/.orcad-readiness"
      exit 0
    fi
    dead="$active"
  fi
fi
for data in {candidates}; do
  meta="$data/orca-runtime.json"
  [ -f "$meta" ] || continue
  pid=$(sed -n 's/.*"pid": *\([0-9][0-9]*\).*/\1/p' "$meta" | head -1)
  if alive "$pid"; then
    echo app
    echo "$meta"
    echo "$dead"
    exit 0
  fi
done
if [ -n "$dead" ]; then
  echo dead
  echo "$dead"
  exit 0
fi
echo none
"#,
        candidates = data_dir_candidates(),
    );
    let output = ssh_output(target, &script).await?;
    if !output.status.success() {
        anyhow::bail!(
            "ssh {target} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let mut lines = stdout.lines();
    match lines.next() {
        Some("orcad") => {
            let readiness = lines
                .next()
                .filter(|line| !line.is_empty())
                .ok_or_else(|| anyhow!("orcad on {target} reported no readiness"))?;
            Ok(ProbeResult::Orcad {
                readiness: readiness.to_owned(),
            })
        }
        Some("app") => {
            let metadata_path = lines
                .next()
                .filter(|line| !line.is_empty())
                .ok_or_else(|| anyhow!("orca runtime metadata path missing on {target}"))?;
            let dead_version = lines
                .next()
                .filter(|line| !line.is_empty())
                .map(|line| line.to_owned());
            Ok(ProbeResult::App {
                metadata_path: metadata_path.to_owned(),
                dead_version,
            })
        }
        Some("dead") => {
            let version = lines
                .next()
                .filter(|line| !line.is_empty())
                .ok_or_else(|| anyhow!("orcad activation on {target} is unreadable"))?;
            Ok(ProbeResult::Dead {
                version: version.to_owned(),
            })
        }
        Some("none") => Ok(ProbeResult::NoInstall),
        _ => anyhow::bail!("unreadable runtime probe output from {target}"),
    }
}

fn data_dir_candidates() -> String {
    let mut words = config::configuration()
        .orca_remote_data_dirs
        .iter()
        .map(|dir| remote_quote(dir))
        .collect::<Vec<_>>();
    words.push(r#""$HOME/Library/Application Support/orca""#.to_owned());
    words.push(r#""${XDG_CONFIG_HOME:-$HOME/.config}/orca""#.to_owned());
    words.join(" ")
}

fn remote_quote(path: &str) -> String {
    match path.strip_prefix("~/") {
        Some(rest) => format!(r#""$HOME"/{}"#, shell_quote(rest)),
        None => shell_quote(path),
    }
}

const MINT_BRIDGE: &str = r#"
const fs = require("fs");
const net = require("net");
const metaPath = process.argv[1];
const cachePath = process.argv[2];
const refresh = process.argv[3] === "refresh";
function fail(message, code) { console.error(message); process.exit(code); }
const meta = JSON.parse(fs.readFileSync(metaPath, "utf8"));
const unix = (meta.transports || []).find(function (t) { return t.kind === "unix"; });
const ws = (meta.transports || []).find(function (t) { return t.kind === "websocket"; });
if (!unix || !meta.authToken) {
  fail("runtime metadata lacks a unix transport or auth token", 3);
}
if (!refresh && ws) {
  try {
    const cached = JSON.parse(fs.readFileSync(cachePath, "utf8"))[meta.runtimeId];
    if (cached) {
      console.log(JSON.stringify({ pairingUrl: cached, endpoint: ws.endpoint }));
      process.exit(0);
    }
  } catch (err) {}
}
const socket = net.createConnection(unix.endpoint);
socket.setEncoding("utf8");
let buffer = "";
socket.on("error", function (err) { fail(err.message, 4); });
socket.on("connect", function () {
  socket.write(JSON.stringify({
    id: "wezterm-pairing",
    method: "pairing.createOffer",
    params: { name: "wezterm", rotate: refresh },
    authToken: meta.authToken
  }) + "\n");
});
socket.on("data", function (chunk) {
  buffer += chunk;
  let index;
  while ((index = buffer.indexOf("\n")) >= 0) {
    const line = buffer.slice(0, index);
    buffer = buffer.slice(index + 1);
    if (!line.trim()) continue;
    const frame = JSON.parse(line);
    if (frame._keepalive) continue;
    if (!frame.ok) {
      const code = frame.error && frame.error.code;
      if (code === "method_not_found") {
        fail("the orca runtime predates pairing.createOffer; update orca on the host", 6);
      }
      fail((frame.error && frame.error.message) || "pairing offer request failed", 4);
    }
    const result = frame.result || {};
    if (!result.available) {
      fail(result.guidance || "pairing unavailable", 4);
    }
    let cache = {};
    try { cache = JSON.parse(fs.readFileSync(cachePath, "utf8")); } catch (err) {}
    cache[meta.runtimeId] = result.pairingUrl;
    fs.writeFileSync(cachePath, JSON.stringify(cache), { mode: 0o600 });
    console.log(JSON.stringify({ pairingUrl: result.pairingUrl, endpoint: result.endpoint }));
    process.exit(0);
  }
});
setTimeout(function () {
  fail("timed out waiting for the runtime socket", 4);
}, 10000);
"#;

async fn adopt_app_runtime(
    target: &str,
    metadata_path: &str,
    refresh_pairing: bool,
) -> anyhow::Result<RemoteRuntime> {
    let script = format!(
        r#"
umask 077
mkdir -p "$HOME/.orca-remote"
{NODE_DISCOVERY}
[ -n "$node_bin" ] || {{ echo "no node runtime on host" >&2; exit 5; }}
exec "$node_bin" -e {bridge} {meta} "$HOME/.orca-remote/app-pairing.json" {mode}
"#,
        bridge = shell_quote(MINT_BRIDGE),
        meta = shell_quote(metadata_path),
        mode = if refresh_pairing { "refresh" } else { "reuse" },
    );
    let output = ssh_output(target, &script).await?;
    if !output.status.success() {
        anyhow::bail!(
            "pairing with the orca runtime on {target} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    parse_minted_offer(target, &String::from_utf8_lossy(&output.stdout))
}

fn parse_minted_offer(target: &str, offer: &str) -> anyhow::Result<RemoteRuntime> {
    let offer = serde_json::from_str::<serde_json::Value>(offer.trim())
        .map_err(|_| anyhow!("unreadable pairing response from the orca runtime on {target}"))?;
    let endpoint = offer
        .get("endpoint")
        .and_then(|value| value.as_str())
        .ok_or_else(|| anyhow!("pairing offer from {target} carries no endpoint"))?;
    let remote_port = endpoint_port(endpoint)
        .ok_or_else(|| anyhow!("unparseable orca endpoint {endpoint} on {target}"))?;
    let pairing_url = offer
        .get("pairingUrl")
        .and_then(|value| value.as_str())
        .ok_or_else(|| anyhow!("pairing offer from {target} carries no pairing URL"))?;
    Ok(RemoteRuntime {
        remote_port,
        pairing_url: pairing_url.to_owned(),
    })
}

async fn deploy(target: &str) -> anyhow::Result<String> {
    let artifacts = resolve_artifacts()?;
    let node_pty = resolve_node_pty(&artifacts)?;
    let version = artifact_version(&artifacts)?;
    upload(target, &artifacts, &node_pty, &version).await?;
    activate(target, &version).await?;
    ensure_native(target, &version).await?;
    relaunch(target, &version).await?;
    Ok(version)
}

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

async fn ensure_native(target: &str, version: &str) -> anyhow::Result<()> {
    let script = format!(
        r#"
cd "$HOME/.orca-remote/orcad-{version}" || exit 7
{NODE_DISCOVERY}
[ -n "$node_bin" ] || {{ echo "no node runtime on host" >&2; exit 5; }}
[ "$node_major" -ge 20 ] || {{ echo "orcad needs node >= 20; newest on host is $node_major" >&2; exit 5; }}
export PATH="$(dirname "$node_bin"):$PATH"
if "$node_bin" -e "require('./node_modules/node-pty')" 2>/dev/null; then exit 0; fi
rm -rf node_modules/node-pty/build
npm rebuild --ignore-scripts=false node-pty > npm-build.log 2>&1 || {{
  tail -20 npm-build.log >&2
  exit 6
}}
"$node_bin" -e "require('./node_modules/node-pty')" || {{
  echo "node-pty still fails to load after rebuild" >&2
  exit 6
}}
find node_modules/node-pty -name spawn-helper -exec chmod +x {{}} + 2>/dev/null
true
"#
    );
    run_ssh(target, &script).await
}

fn resolve_artifacts() -> anyhow::Result<PathBuf> {
    let configured = config::configuration().orca_orcad_dir.clone();
    let Some(dir) = configured.filter(|dir| !dir.is_empty()) else {
        anyhow::bail!(
            "no orcad install on the host and no artifact source configured; \
             set orca_orcad_dir to a directory containing orcad.js \
             (e.g. an orca checkout's out/orcad)"
        );
    };
    let dir = PathBuf::from(shellexpand_home(&dir));
    for artifact in ARTIFACTS {
        if !dir.join(artifact).is_file() {
            anyhow::bail!("orca_orcad_dir {} is missing {artifact}", dir.display());
        }
    }
    Ok(dir)
}

fn shellexpand_home(path: &str) -> String {
    match (path.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => path.to_owned(),
    }
}

fn resolve_node_pty(artifacts: &Path) -> anyhow::Result<PathBuf> {
    let candidates = [
        artifacts.join("node_modules/node-pty"),
        artifacts.join("../../node_modules/node-pty"),
    ];
    for candidate in &candidates {
        if candidate.join("package.json").is_file() {
            return Ok(candidate.clone());
        }
    }
    anyhow::bail!(
        "node-pty package not found near {}; orcad needs it for remote PTYs",
        artifacts.display()
    )
}

fn artifact_version(artifacts: &Path) -> anyhow::Result<String> {
    let bundle = std::fs::read(artifacts.join("orcad.js"))?;
    let digest = Sha256::digest(&bundle);
    let mut hash = String::new();
    for byte in digest.iter().take(6) {
        hash.push_str(&format!("{byte:02x}"));
    }
    Ok(format!("0.0.0+{hash}"))
}

async fn upload(
    target: &str,
    artifacts: &Path,
    node_pty: &Path,
    version: &str,
) -> anyhow::Result<()> {
    let node_pty_parent = node_pty
        .parent()
        .ok_or_else(|| anyhow!("node-pty path has no parent"))?;
    let dir = format!("$HOME/.orca-remote/orcad-{version}");
    let pipeline = format!(
        "/usr/bin/tar -c -h --no-xattrs -f - --exclude 'node-pty/build' -C {} {} -C {} node-pty node-addon-api \
         | ssh -oBatchMode=yes -oConnectTimeout=10 {} 'mkdir -p \"{dir}/node_modules\" \
         && tar xf - -C \"{dir}\" \
         && rm -rf \"{dir}/node_modules/node-pty\" \"{dir}/node_modules/node-addon-api\" \
         && mv \"{dir}/node-pty\" \"{dir}/node_modules/node-pty\" \
         && mv \"{dir}/node-addon-api\" \"{dir}/node_modules/node-addon-api\" \
         && {{ find \"{dir}/node_modules/node-pty\" -name spawn-helper -exec chmod +x {{}} + 2>/dev/null || true; }}'",
        shell_quote(&artifacts.to_string_lossy()),
        ARTIFACTS.join(" "),
        shell_quote(&node_pty_parent.to_string_lossy()),
        shell_quote(target),
    );
    let output = smol::process::Command::new("sh")
        .arg("-c")
        .arg(&pipeline)
        .output()
        .await?;
    if !output.status.success() {
        anyhow::bail!(
            "orcad upload to {target} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    if artifacts.join("prebuilds").is_dir() {
        let overlay = format!(
            "/usr/bin/tar -c -h --no-xattrs -f - -C {} prebuilds \
             | ssh -oBatchMode=yes -oConnectTimeout=10 {} 'tar xf - -C \"{dir}/node_modules/node-pty\"'",
            shell_quote(&artifacts.to_string_lossy()),
            shell_quote(target),
        );
        let output = smol::process::Command::new("sh")
            .arg("-c")
            .arg(&overlay)
            .output()
            .await?;
        if !output.status.success() {
            anyhow::bail!(
                "orcad prebuild upload to {target} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    }
    Ok(())
}

async fn activate(target: &str, version: &str) -> anyhow::Result<()> {
    let record = format!(
        r#"{{"schemaVersion":1,"active":"{version}","previous":null,"activatedAt":null,"snapshot":null}}"#
    );
    let script = format!(
        "mkdir -p \"$HOME/.orca-remote\" && printf '%s' '{record}' > \"$HOME/.orca-remote/orcad-active.json\""
    );
    run_ssh(target, &script).await
}

async fn relaunch(target: &str, version: &str) -> anyhow::Result<()> {
    let script = format!(
        r#"
set -e
cd "$HOME/.orca-remote/orcad-{version}"
{NODE_DISCOVERY}
[ -n "$node_bin" ] || {{ echo "no node runtime on host" >&2; exit 5; }}
[ "$node_major" -ge 20 ] || {{ echo "orcad needs node >= 20; newest on host is $node_major" >&2; exit 5; }}
: > .orcad-readiness
umask 077
ORCA_VERSION='{version}' ORCA_USER_DATA="$HOME/.orca-remote/orcad-data" \
  nohup "$node_bin" orcad.js --json --bind 127.0.0.1 > .orcad-readiness 2>> orcad.log < /dev/null &
echo $! > .orcad-pid
"#
    );
    run_ssh(target, &script).await
}

async fn await_readiness(target: &str, version: &str) -> anyhow::Result<String> {
    let script = format!(r#"head -1 "$HOME/.orca-remote/orcad-{version}/.orcad-readiness""#);
    for _ in 0..20 {
        smol::Timer::after(Duration::from_millis(750)).await;
        let output = ssh_output(target, &script).await?;
        let line = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if output.status.success()
            && !line.is_empty()
            && serde_json::from_str::<serde_json::Value>(&line).is_ok()
        {
            return Ok(line);
        }
    }
    anyhow::bail!(
        "orcad on {target} did not become ready; see ~/.orca-remote/orcad-{version}/orcad.log"
    )
}

fn parse_readiness(target: &str, line: &str) -> anyhow::Result<RemoteRuntime> {
    let readiness = serde_json::from_str::<serde_json::Value>(line.trim())
        .map_err(|_| anyhow!("orcad on {target} produced unreadable readiness output"))?;
    let bound = readiness
        .get("boundEndpoint")
        .and_then(|value| value.as_str())
        .ok_or_else(|| anyhow!("orcad on {target} reported no bound endpoint"))?;
    let remote_port = endpoint_port(bound)
        .ok_or_else(|| anyhow!("unparseable orcad endpoint {bound} on {target}"))?;
    let pairing = readiness.get("pairing");
    let pairing_url = pairing
        .and_then(|pairing| pairing.get("url"))
        .and_then(|value| value.as_str())
        .ok_or_else(|| {
            let guidance = pairing
                .and_then(|pairing| pairing.get("guidance"))
                .and_then(|value| value.as_str())
                .unwrap_or("pairing unavailable");
            anyhow!("orcad on {target}: {guidance}")
        })?;
    Ok(RemoteRuntime {
        remote_port,
        pairing_url: pairing_url.to_owned(),
    })
}

fn endpoint_port(endpoint: &str) -> Option<u16> {
    endpoint.rsplit(':').next()?.parse::<u16>().ok()
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

async fn run_ssh(target: &str, script: &str) -> anyhow::Result<()> {
    let output = ssh_output(target, script).await?;
    if !output.status.success() {
        anyhow::bail!(
            "ssh {target} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

async fn ssh_output(target: &str, script: &str) -> anyhow::Result<std::process::Output> {
    Ok(smol::process::Command::new("ssh")
        .arg("-oBatchMode=yes")
        .arg("-oConnectTimeout=10")
        .arg(target)
        .arg(format!("sh -c {}", shell_quote(script)))
        .output()
        .await?)
}
