use super::context::{current_snapshot, EditorSnapshot};
use helix_view::Editor;
use serde::Serialize;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Serialize)]
pub struct AgentSession {
    pub schema_version: u32,
    pub kind: &'static str,
    pub id: String,
    pub status: AgentSessionStatus,
    pub messages: Vec<AgentMessage>,
    pub context: EditorSnapshot,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentSessionStatus {
    NotStarted,
}

#[derive(Debug, Serialize)]
pub struct AgentMessage {
    pub role: AgentMessageRole,
    pub content: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentMessageRole {
    System,
}

pub fn new_session(editor: &Editor) -> AgentSession {
    AgentSession {
        schema_version: 1,
        kind: "session",
        id: session_id(),
        status: AgentSessionStatus::NotStarted,
        messages: vec![AgentMessage {
            role: AgentMessageRole::System,
            content: "Agent session initialized from the current Helix editor context.".to_string(),
        }],
        context: current_snapshot(editor),
    }
}

pub fn new_session_pretty(editor: &Editor) -> anyhow::Result<String> {
    Ok(serde_json::to_string_pretty(&new_session(editor))?)
}

fn session_id() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();

    format!("agent-{millis}")
}
