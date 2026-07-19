use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Workspace {
    pub id: String,
    #[serde(default)]
    pub project_id: String,
    #[serde(default)]
    pub project_display_name: String,
    #[serde(default)]
    pub project_root_path: String,
    #[serde(default)]
    pub workspace_directory: Option<String>,
    #[serde(default)]
    pub workspace_kind: String,
    #[serde(default)]
    pub name: String,
}

impl Workspace {
    pub fn cwd(&self) -> &str {
        self.workspace_directory
            .as_deref()
            .filter(|d| !d.is_empty())
            .unwrap_or(&self.project_root_path)
    }
}

pub fn fetch_workspaces_request(request_id: &str) -> Value {
    json!({ "type": "fetch_workspaces_request", "requestId": request_id })
}

pub fn project_add_request(request_id: &str, cwd: &str) -> Value {
    json!({ "type": "project.add.request", "cwd": cwd, "requestId": request_id })
}

pub fn project_create_directory_request(request_id: &str, parent_path: &str, name: &str) -> Value {
    json!({
        "type": "project.create_directory.request",
        "parentPath": parent_path,
        "name": name,
        "requestId": request_id
    })
}

pub fn project_github_clone_request(request_id: &str, repo: &str, protocol: &str) -> Value {
    json!({
        "type": "project.github.clone.request",
        "repo": repo,
        "cloneProtocol": protocol,
        "requestId": request_id
    })
}
