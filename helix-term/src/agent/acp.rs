use serde::Serialize;
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

pub fn pretty_json<T: Serialize>(value: &T) -> anyhow::Result<String> {
    Ok(serde_json::to_string_pretty(value)?)
}

pub fn session_handshake_pretty(editor: &helix_view::Editor) -> anyhow::Result<String> {
    let session = session::new_session(editor);
    let messages = [
        serde_json::to_value(initialize_request(1)?)?,
        serde_json::to_value(new_session_request(2, session)?)?,
    ];

    pretty_json(&messages)
}
