use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::session::{self, AgentSession};

pub const ACP_PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Serialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: &'static str,
    pub params: Value,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: &'static str,
    pub method: &'static str,
    pub params: Value,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum JsonRpcMessage {
    Response(JsonRpcResponse),
    Request(JsonRpcInboundRequest),
    Notification(JsonRpcInboundNotification),
}

#[derive(Debug, Deserialize, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: Option<String>,
    pub id: u64,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(default)]
    pub data: Option<Value>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct JsonRpcInboundRequest {
    pub jsonrpc: Option<String>,
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct JsonRpcInboundNotification {
    pub jsonrpc: Option<String>,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct InitializeParams {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: u32,
    #[serde(rename = "clientCapabilities")]
    pub client_capabilities: ClientCapabilities,
    #[serde(rename = "clientInfo")]
    pub client_info: ImplementationInfo,
}

#[derive(Debug, Serialize)]
pub struct ClientCapabilities {
    pub fs: FileSystemCapabilities,
    pub terminal: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSystemCapabilities {
    pub read_text_file: bool,
    pub write_text_file: bool,
}

#[derive(Debug, Serialize)]
pub struct ImplementationInfo {
    pub name: &'static str,
    pub title: &'static str,
    pub version: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionParams {
    pub cwd: String,
    pub mcp_servers: Vec<Value>,
    #[serde(rename = "_meta")]
    pub meta: Value,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptParams {
    pub session_id: String,
    pub prompt: Vec<ContentBlock>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetModeParams {
    pub session_id: String,
    pub mode_id: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetConfigOptionParams {
    pub session_id: String,
    pub config_id: String,
    pub value: String,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
}

pub fn initialize_request(id: u64) -> anyhow::Result<JsonRpcRequest> {
    Ok(JsonRpcRequest {
        jsonrpc: "2.0",
        id,
        method: "initialize",
        params: serde_json::to_value(InitializeParams {
            protocol_version: ACP_PROTOCOL_VERSION,
            client_capabilities: ClientCapabilities {
                fs: FileSystemCapabilities {
                    read_text_file: false,
                    write_text_file: false,
                },
                terminal: false,
            },
            client_info: ImplementationInfo {
                name: "helix",
                title: "Helix",
                version: helix_loader::VERSION_AND_GIT_HASH,
            },
        })?,
    })
}

pub fn new_session_request(id: u64, session: AgentSession) -> anyhow::Result<JsonRpcRequest> {
    Ok(JsonRpcRequest {
        jsonrpc: "2.0",
        id,
        method: "session/new",
        params: serde_json::to_value(NewSessionParams {
            cwd: session.context.cwd.clone(),
            mcp_servers: Vec::new(),
            meta: json!({
                "helix": {
                    "session": session,
                }
            }),
        })?,
    })
}

pub fn prompt_request(
    id: u64,
    session_id: String,
    prompt: String,
    meta: Option<Value>,
) -> anyhow::Result<JsonRpcRequest> {
    Ok(JsonRpcRequest {
        jsonrpc: "2.0",
        id,
        method: "session/prompt",
        params: serde_json::to_value(PromptParams {
            session_id,
            prompt: vec![ContentBlock::Text { text: prompt }],
            meta,
        })?,
    })
}

pub fn set_mode_request(
    id: u64,
    session_id: String,
    mode_id: String,
) -> anyhow::Result<JsonRpcRequest> {
    Ok(JsonRpcRequest {
        jsonrpc: "2.0",
        id,
        method: "session/set_mode",
        params: serde_json::to_value(SetModeParams {
            session_id,
            mode_id,
        })?,
    })
}

pub fn set_config_option_request(
    id: u64,
    session_id: String,
    config_id: String,
    value: String,
) -> anyhow::Result<JsonRpcRequest> {
    Ok(JsonRpcRequest {
        jsonrpc: "2.0",
        id,
        method: "session/set_config_option",
        params: serde_json::to_value(SetConfigOptionParams {
            session_id,
            config_id,
            value,
        })?,
    })
}

pub fn session_handshake(editor: &helix_view::Editor) -> anyhow::Result<Vec<JsonRpcRequest>> {
    let session = session::new_session(editor);
    Ok(vec![
        initialize_request(1)?,
        new_session_request(2, session)?,
    ])
}

pub fn pretty_json<T: Serialize>(value: &T) -> anyhow::Result<String> {
    Ok(serde_json::to_string_pretty(value)?)
}

pub fn session_handshake_pretty(editor: &helix_view::Editor) -> anyhow::Result<String> {
    let messages = session_handshake(editor)?
        .into_iter()
        .map(serde_json::to_value)
        .collect::<Result<Vec<_>, _>>()?;

    pretty_json(&messages)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_request_includes_meta_when_present() {
        let request = prompt_request(
            7,
            "session-1".to_string(),
            "explain this file".to_string(),
            Some(json!({
                "helix": {
                    "context": {
                        "theme": "base16_default"
                    }
                }
            })),
        )
        .unwrap();

        assert_eq!(request.method, "session/prompt");
        assert_eq!(request.params["sessionId"], "session-1");
        assert_eq!(request.params["prompt"][0]["text"], "explain this file");
        assert_eq!(
            request.params["_meta"]["helix"]["context"]["theme"],
            "base16_default"
        );
    }
}
