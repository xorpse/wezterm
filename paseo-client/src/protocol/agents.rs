use crate::protocol::timeline::ToolCallDetail;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSnapshot {
    pub id: String,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub labels: HashMap<String, Value>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub requires_attention: bool,
    #[serde(default)]
    pub attention_reason: Option<String>,
    #[serde(default)]
    pub pending_permissions: Vec<PermissionRequest>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentListEntry {
    pub agent: AgentSnapshot,
    #[serde(default)]
    pub project: Value,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionAction {
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub behavior: String,
    #[serde(default)]
    pub variant: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRequest {
    pub id: String,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub detail: Option<ToolCallDetail>,
    #[serde(default)]
    pub actions: Vec<PermissionAction>,
}

#[derive(Clone, Debug)]
pub enum PermissionResponse {
    Allow {
        selected_action_id: Option<String>,
    },
    Deny {
        message: Option<String>,
        interrupt: bool,
    },
}

impl PermissionResponse {
    pub fn to_value(&self) -> Value {
        match self {
            PermissionResponse::Allow { selected_action_id } => {
                let mut v = json!({ "behavior": "allow" });
                if let Some(id) = selected_action_id {
                    v["selectedActionId"] = json!(id);
                }
                v
            }
            PermissionResponse::Deny { message, interrupt } => {
                let mut v = json!({ "behavior": "deny", "interrupt": interrupt });
                if let Some(msg) = message {
                    v["message"] = json!(msg);
                }
                v
            }
        }
    }
}

pub fn fetch_agents_request(request_id: &str) -> Value {
    json!({
        "type": "fetch_agents_request",
        "requestId": request_id,
        "page": { "limit": 200 }
    })
}

pub fn fetch_agent_request(request_id: &str, agent_id: &str) -> Value {
    json!({
        "type": "fetch_agent_request",
        "requestId": request_id,
        "agentId": agent_id
    })
}

pub fn fetch_agent_timeline_request(
    request_id: &str,
    agent_id: &str,
    direction: &str,
    limit: u32,
) -> Value {
    json!({
        "type": "fetch_agent_timeline_request",
        "requestId": request_id,
        "agentId": agent_id,
        "direction": direction,
        "limit": limit
    })
}

pub fn set_timeline_subscription_request(request_id: &str, agent_ids: &[String]) -> Value {
    json!({
        "type": "agent.timeline.set_subscription.request",
        "requestId": request_id,
        "agentIds": agent_ids
    })
}

pub fn send_agent_message_request(request_id: &str, agent_id: &str, text: &str) -> Value {
    json!({
        "type": "send_agent_message_request",
        "requestId": request_id,
        "agentId": agent_id,
        "text": text
    })
}

pub fn permission_response_message(
    agent_id: &str,
    request_id: &str,
    response: &PermissionResponse,
) -> Value {
    json!({
        "type": "agent_permission_response",
        "agentId": agent_id,
        "requestId": request_id,
        "response": response.to_value()
    })
}
