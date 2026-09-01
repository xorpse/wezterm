use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::anyhow;
use sha2::{Digest, Sha256};

const ARTIFACTS: [&str; 3] = [
    "orcad.js",
    "daemon-entry.js",
    "parcel-watcher-process-entry.js",
];
const PROBE_NO_INSTALL: i32 = 3;
const PROBE_DEAD: i32 = 4;

pub(crate) struct RemoteRuntime {
    pub remote_port: u16,
    pub pairing_url: String,
}

pub(crate) async fn ensure_remote_runtime(target: &str) -> anyhow::Result<RemoteRuntime> {
    match probe(target).await? {
        ProbeResult::Live(readiness) => parse_readiness(target, &readiness),
        ProbeResult::Dead { version } => {
            ensure_native(target, &version).await?;
            relaunch(target, &version).await?;
            parse_readiness(target, &await_readiness(target, &version).await?)
        }
        ProbeResult::NoInstall => {
            let version = deploy(target).await?;
            parse_readiness(target, &await_readiness(target, &version).await?)
        }
    }
}

enum ProbeResult {
    Live(String),
    Dead { version: String },
    NoInstall,
}

async fn probe(target: &str) -> anyhow::Result<ProbeResult> {
    let script = r#"
cd "$HOME/.orca-remote" 2>/dev/null || exit 3
active=$(sed -n 's/.*"active": *"\([^"]*\)".*/\1/p' orcad-active.json 2>/dev/null || true)
[ -n "$active" ] || exit 3
[ -d "orcad-$active" ] || exit 3
echo "$active"
pid=$(cat "orcad-$active/orcad.pid" 2>/dev/null)
if [ -z "$pid" ] || ! kill -0 "$pid" 2>/dev/null; then exit 4; fi
head -1 "orcad-$active/.orcad-readiness"
"#;
    let output = ssh_output(target, script).await?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    match output.status.code() {
        Some(0) => {
            let readiness = stdout
                .lines()
                .nth(1)
                .ok_or_else(|| anyhow!("orcad on {target} reported no readiness"))?;
            Ok(ProbeResult::Live(readiness.to_owned()))
        }
        Some(PROBE_NO_INSTALL) => Ok(ProbeResult::NoInstall),
        Some(PROBE_DEAD) => {
            let version = stdout
                .lines()
                .next()
                .filter(|line| !line.is_empty())
                .ok_or_else(|| anyhow!("orcad activation on {target} is unreadable"))?;
            Ok(ProbeResult::Dead {
                version: version.to_owned(),
            })
        }
        _ => anyhow::bail!(
            "ssh {target} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
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

async fn ensure_native(target: &str, version: &str) -> anyhow::Result<()> {
    let script = format!(
        r#"
cd "$HOME/.orca-remote/orcad-{version}" || exit 7
shell_path=$("${{SHELL:-/bin/sh}}" -l -c env 2>/dev/null </dev/null | sed -n 's/^PATH=//p' | tail -1)
[ -n "$shell_path" ] && PATH="$shell_path:$PATH"
export PATH
node_bin=""
node_major=0
for candidate in node /opt/homebrew/bin/node /usr/local/bin/node /usr/bin/node "$HOME"/.nvm/versions/node/*/bin/node; do
  resolved=$(command -v "$candidate" 2>/dev/null) || resolved="$candidate"
  [ -x "$resolved" ] || continue
  major=$("$resolved" -e 'process.stdout.write(process.versions.node.split(".")[0])' 2>/dev/null) || continue
  if [ "$major" -gt "$node_major" ]; then node_bin="$resolved"; node_major=$major; fi
done
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
shell_path=$("${{SHELL:-/bin/sh}}" -l -c env 2>/dev/null </dev/null | sed -n 's/^PATH=//p' | tail -1)
[ -n "$shell_path" ] && PATH="$shell_path:$PATH"
export PATH
node_bin=""
node_major=0
for candidate in node /opt/homebrew/bin/node /usr/local/bin/node /usr/bin/node "$HOME"/.nvm/versions/node/*/bin/node; do
  resolved=$(command -v "$candidate" 2>/dev/null) || resolved="$candidate"
  [ -x "$resolved" ] || continue
  major=$("$resolved" -e 'process.stdout.write(process.versions.node.split(".")[0])' 2>/dev/null) || continue
  if [ "$major" -gt "$node_major" ]; then node_bin="$resolved"; node_major=$major; fi
done
[ -n "$node_bin" ] || {{ echo "no node runtime on host" >&2; exit 5; }}
[ "$node_major" -ge 20 ] || {{ echo "orcad needs node >= 20; newest on host is $node_major" >&2; exit 5; }}
: > .orcad-readiness
umask 077
ORCA_VERSION='{version}' ORCA_USER_DATA="$HOME/.orca-remote/orcad-data" \
  nohup "$node_bin" orcad.js --json --bind 127.0.0.1 > .orcad-readiness 2>> orcad.log < /dev/null &
echo $! > orcad.pid
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
    let remote_port = bound
        .rsplit(':')
        .next()
        .and_then(|port| port.parse::<u16>().ok())
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
        .arg(script)
        .output()
        .await?)
}
