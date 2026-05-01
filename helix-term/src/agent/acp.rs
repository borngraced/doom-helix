use serde::Serialize;
use serde_json::Value;

use super::session::AgentSession;

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
    pub protocol_version: u32,
    pub client: ClientInfo,
}

#[derive(Debug, Serialize)]
pub struct ClientInfo {
    pub name: &'static str,
    pub version: &'static str,
}

#[derive(Debug, Serialize)]
pub struct NewSessionParams {
    pub session: AgentSession,
}

pub fn initialize_request(id: u64) -> anyhow::Result<JsonRpcRequest> {
    Ok(JsonRpcRequest {
        jsonrpc: "2.0",
        id,
        method: "initialize",
        params: serde_json::to_value(InitializeParams {
            protocol_version: ACP_PROTOCOL_VERSION,
            client: ClientInfo {
                name: "helix",
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
        params: serde_json::to_value(NewSessionParams { session })?,
    })
}

pub fn initialized_notification() -> JsonRpcNotification {
    JsonRpcNotification {
        jsonrpc: "2.0",
        method: "initialized",
        params: Value::Null,
    }
}

pub fn pretty_json<T: Serialize>(value: &T) -> anyhow::Result<String> {
    Ok(serde_json::to_string_pretty(value)?)
}
