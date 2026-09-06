use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, anyhow};
use orca_client::{ServerDir, SshConnectionState, SshTargetSummary, WorktreePsSummary};
use serde_json::{Value, json};
use smol::io::{AsyncReadExt, AsyncWriteExt};

const CALL_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone)]
pub struct LocalRuntime {
    endpoint: String,
    auth_token: String,
    runtime_id: String,
}

impl LocalRuntime {
    pub fn discover() -> Option<LocalRuntime> {
        let metadata = std::fs::read(runtime_metadata_path()?).ok()?;
        let metadata = serde_json::from_slice::<Value>(&metadata).ok()?;
        let auth_token = metadata.get("authToken")?.as_str()?.to_owned();
        let runtime_id = metadata
            .get("runtimeId")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_owned();
        let endpoint = metadata
            .get("transports")?
            .as_array()?
            .iter()
            .find(|transport| transport.get("kind").and_then(|k| k.as_str()) == Some("unix"))?
            .get("endpoint")?
            .as_str()?
            .to_owned();
        Some(LocalRuntime {
            endpoint,
            auth_token,
            runtime_id,
        })
    }

    pub fn runtime_id(&self) -> &str {
        &self.runtime_id
    }

    async fn call(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        // worktree.ps / session.listAll take hundreds of ms on the app side. Run
        // every request on the background executor so callers awaiting from the
        // GUI's main thread simply yield instead of freezing the UI.
        let this = self.clone();
        let method = method.to_owned();
        smol::spawn(async move {
            smol::future::or(this.call_inner(&method, params), async {
                smol::Timer::after(CALL_TIMEOUT).await;
                Err(anyhow!("local runtime call {method} timed out"))
            })
            .await
        })
        .await
    }

    async fn call_inner(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let mut stream = smol::net::unix::UnixStream::connect(&self.endpoint)
            .await
            .with_context(|| format!("connecting to the local orca runtime for {method}"))?;
        let request = json!({
            "id": "wezterm",
            "method": method,
            "params": params,
            "authToken": self.auth_token,
        });
        let mut payload = serde_json::to_vec(&request)?;
        payload.push(b'\n');
        stream.write_all(&payload).await?;
        stream.flush().await?;

        let mut buffer = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            while let Some(position) = buffer.iter().position(|&byte| byte == b'\n') {
                let line = buffer.drain(..=position).collect::<Vec<_>>();
                let line = &line[..line.len() - 1];
                if line.is_empty() {
                    continue;
                }
                let value = serde_json::from_slice::<Value>(line)?;
                if value.get("_keepalive").is_some() {
                    continue;
                }
                if value.get("ok").and_then(|ok| ok.as_bool()) == Some(true) {
                    return Ok(value.get("result").cloned().unwrap_or(Value::Null));
                }
                let message = value
                    .pointer("/error/message")
                    .and_then(|value| value.as_str())
                    .unwrap_or("local runtime call failed");
                anyhow::bail!("orca runtime {method}: {message}");
            }
            let read = stream.read(&mut chunk).await?;
            if read == 0 {
                anyhow::bail!("local orca runtime closed before answering {method}");
            }
            buffer.extend_from_slice(&chunk[..read]);
        }
    }

    pub async fn list_all(&self) -> anyhow::Result<Value> {
        self.call("session.tabs.listAll", json!({})).await
    }

    pub async fn ssh_targets(&self) -> anyhow::Result<Value> {
        self.call("ssh.listTargetSummaries", json!({})).await
    }

    pub async fn ssh_target_summaries(&self) -> anyhow::Result<Vec<SshTargetSummary>> {
        let value = self.call("ssh.listTargetSummaries", json!({})).await?;
        let targets = value.get("targets").cloned().unwrap_or(Value::Null);
        Ok(serde_json::from_value(targets).unwrap_or_default())
    }

    pub async fn ssh_target_state(
        &self,
        target_id: &str,
    ) -> anyhow::Result<Option<SshConnectionState>> {
        self.ssh_state_call("ssh.getState", target_id).await
    }

    pub async fn connect_ssh_target(
        &self,
        target_id: &str,
    ) -> anyhow::Result<Option<SshConnectionState>> {
        self.ssh_state_call("ssh.connect", target_id).await
    }

    async fn ssh_state_call(
        &self,
        method: &str,
        target_id: &str,
    ) -> anyhow::Result<Option<SshConnectionState>> {
        let value = self.call(method, json!({ "targetId": target_id })).await?;
        match value.get("state") {
            Some(Value::Null) | None => Ok(None),
            Some(state) => Ok(serde_json::from_value(state.clone()).ok()),
        }
    }

    pub async fn detect_agents(&self) -> anyhow::Result<Vec<String>> {
        let value = self.call("preflight.detectAgents", json!({})).await?;
        Ok(serde_json::from_value(value).unwrap_or_default())
    }

    pub async fn worktree_ps(&self) -> anyhow::Result<Vec<WorktreePsSummary>> {
        let value = self.call("worktree.ps", json!({})).await?;
        let worktrees = value.get("worktrees").cloned().unwrap_or(Value::Null);
        Ok(serde_json::from_value(worktrees).unwrap_or_default())
    }

    pub async fn create_worktree(&self, repo: &str, name: &str) -> anyhow::Result<()> {
        self.call("worktree.create", json!({ "repo": repo, "name": name }))
            .await?;
        Ok(())
    }

    pub async fn add_repo(&self, path: &str) -> anyhow::Result<()> {
        self.call("repo.add", json!({ "path": path })).await?;
        Ok(())
    }

    pub async fn clone_repo(&self, url: &str, destination: &str) -> anyhow::Result<()> {
        self.call(
            "repo.clone",
            json!({ "url": url, "destination": destination }),
        )
        .await?;
        Ok(())
    }

    pub async fn create_repo(&self, parent_path: &str, name: &str) -> anyhow::Result<()> {
        self.call(
            "repo.create",
            json!({ "parentPath": parent_path, "name": name, "kind": "git" }),
        )
        .await?;
        Ok(())
    }

    pub async fn browse_server_dir(&self, path: &str) -> anyhow::Result<ServerDir> {
        let value = self
            .call("files.browseServerDir", json!({ "path": path }))
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    pub async fn create_terminal(&self, worktree: &str) -> anyhow::Result<Value> {
        self.call(
            "session.tabs.createTerminal",
            json!({ "worktree": worktree }),
        )
        .await
    }

    pub async fn create_agent_terminal(
        &self,
        worktree: &str,
        agent: &str,
    ) -> anyhow::Result<Value> {
        self.call(
            "session.tabs.createTerminal",
            json!({ "worktree": worktree, "launchAgent": agent }),
        )
        .await
    }

    pub async fn close_tab(&self, tab_id: &str) -> anyhow::Result<Value> {
        self.call("session.tabs.close", json!({ "tabId": tab_id }))
            .await
    }
}

fn runtime_metadata_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("ORCA_USER_DATA_PATH") {
        return Some(PathBuf::from(explicit).join("orca-runtime.json"));
    }
    let home = std::env::var("HOME").ok()?;
    if cfg!(target_os = "macos") {
        Some(PathBuf::from(home).join("Library/Application Support/orca/orca-runtime.json"))
    } else {
        let base = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(&home).join(".config"));
        Some(base.join("orca/orca-runtime.json"))
    }
}
