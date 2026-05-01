use helix_event::runtime_local;
use helix_view::editor::AgentLaunchConfig;
use helix_view::DocumentId;
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use tokio::sync::oneshot;

use super::{
    acp::{JsonRpcMessage, JsonRpcRequest},
    transport::AgentProcess,
};

runtime_local! {
    static AGENT_RUNTIME: Mutex<Option<RunningAgent>> = Mutex::new(None);
    static AGENT_BUSY: Mutex<Option<AgentBusyStatus>> = Mutex::new(None);
    static AGENT_TRANSCRIPT: Mutex<Option<DocumentId>> = Mutex::new(None);
    static AGENT_LATEST_PATCH: Mutex<Option<AgentPatchProposal>> = Mutex::new(None);
    static AGENT_TRANSCRIPT_TURNS: Mutex<Vec<AgentTranscriptTurn>> = Mutex::new(Vec::new());
}

static AGENT_CANCEL_GENERATION: AtomicU64 = AtomicU64::new(0);

pub struct RunningAgent {
    pub name: String,
    pub process: AgentProcess,
    pub session_id: Option<String>,
    pub next_request_id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentBusyStatus {
    pub name: String,
    pub session_id: Option<String>,
    pub request_id: Option<u64>,
}

struct AgentBusyGuard;

impl Drop for AgentBusyGuard {
    fn drop(&mut self) {
        clear_busy_agent();
    }
}

pub async fn start(
    launch_config: AgentLaunchConfig,
    handshake: Vec<JsonRpcRequest>,
) -> anyhow::Result<()> {
    let _busy_guard = set_busy_agent(AgentBusyStatus {
        name: launch_config.name.clone(),
        session_id: None,
        request_id: None,
    });

    if let Some(mut running) = take_running_agent() {
        log::info!("stopping existing agent '{}' before restart", running.name);
        running.process.kill().await?;
    }

    log::info!(
        "starting agent '{}' using {:?}",
        launch_config.name,
        launch_config.transport
    );
    let mut process = AgentProcess::spawn(&launch_config).await?;
    for message in handshake {
        process.send(&message).await?;
    }
    log::info!("agent '{}' handshake sent", launch_config.name);

    let mut agent = AGENT_RUNTIME.lock().expect("agent runtime lock poisoned");
    *agent = Some(RunningAgent {
        name: launch_config.name,
        process,
        session_id: None,
        next_request_id: 3,
    });

    Ok(())
}

pub async fn ensure_started(
    launch_config: AgentLaunchConfig,
    handshake: Vec<JsonRpcRequest>,
) -> anyhow::Result<bool> {
    if AGENT_RUNTIME
        .lock()
        .expect("agent runtime lock poisoned")
        .is_some()
    {
        return Ok(false);
    }

    start(launch_config, handshake).await?;
    Ok(true)
}

pub async fn stop() -> anyhow::Result<Option<String>> {
    let Some(mut running) = take_running_agent() else {
        return Ok(None);
    };

    log::info!("stopping agent '{}'", running.name);
    running.process.kill().await?;
    Ok(Some(running.name))
}

pub fn cancel_generation() -> u64 {
    AGENT_CANCEL_GENERATION.load(Ordering::Relaxed)
}

pub fn cancel_all() -> u64 {
    AGENT_CANCEL_GENERATION.fetch_add(1, Ordering::Relaxed) + 1
}

pub fn is_cancelled(generation: u64) -> bool {
    cancel_generation() != generation
}

pub async fn recv_next() -> anyhow::Result<JsonRpcMessage> {
    let Some(mut running) = take_running_agent() else {
        anyhow::bail!("no agent is running");
    };

    match running.process.recv().await {
        Ok(message) => {
            update_session_id(&mut running, &message);
            restore_running_agent(running);
            Ok(message)
        }
        Err(err) => {
            let exit_status = running.process.try_wait()?;
            let stderr = running.process.stderr_snapshot().await?;
            if exit_status.is_none() {
                restore_running_agent(running);
            }

            let mut message = format!("failed to read agent message: {err}");
            if let Some(status) = exit_status {
                message.push_str(&format!("; process exited with {status}"));
            }
            if let Some(stderr) = stderr {
                message.push_str(&format!("; stderr: {stderr}"));
            }

            anyhow::bail!(message);
        }
    }
}

pub async fn send_prompt_turn(prompt: String, meta: Option<Value>) -> anyhow::Result<AgentTurn> {
    send_prompt_turn_streaming(prompt, meta, |_| {}).await
}

pub async fn send_prompt_turn_streaming<F>(
    prompt: String,
    meta: Option<Value>,
    mut on_message: F,
) -> anyhow::Result<AgentTurn>
where
    F: FnMut(&JsonRpcMessage) + Send,
{
    let Some(mut running) = take_running_agent() else {
        anyhow::bail!("no agent is running");
    };
    let _busy_guard = set_busy_agent(AgentBusyStatus {
        name: running.name.clone(),
        session_id: running.session_id.clone(),
        request_id: None,
    });

    let mut messages = Vec::new();
    let mut session_response = None;
    while running.session_id.is_none() {
        let message = recv_running_message(&mut running).await?;
        log_agent_message("startup", &message);
        update_session_id(&mut running, &message);
        update_busy_agent_session(running.session_id.clone());
        if let Some(message) = session_new_failure_message(&message) {
            let _ = running.process.kill().await;
            anyhow::bail!(message);
        }
        if matches!(&message, JsonRpcMessage::Response(response) if json_rpc_id_eq(&response.id, 2))
        {
            session_response = Some(serde_json::to_value(&message)?);
        }
        messages.push(message);
    }

    let session_id = running
        .session_id
        .clone()
        .expect("session id checked before prompt send");
    if let Some(request) =
        approval_mode_request(&mut running, &session_id, session_response.as_ref())?
    {
        let mode_request_id = request.id;
        log::info!(
            "setting agent approval mode with request {mode_request_id} for '{}' session {}",
            running.name,
            session_id
        );
        running.process.send(&request).await?;
        loop {
            let message = recv_running_message(&mut running).await?;
            log_agent_message("approval-mode", &message);
            if let Some(response) = handle_agent_request(&message).await? {
                log_agent_response("approval-mode", &response);
                running.process.send(&response).await?;
                continue;
            }
            let mode_set_done = matches!(&message, JsonRpcMessage::Response(response) if json_rpc_id_eq(&response.id, mode_request_id));
            update_session_id(&mut running, &message);
            on_message(&message);
            messages.push(message);
            if mode_set_done {
                break;
            }
        }
    }
    let request_id = running.next_request_id;
    running.next_request_id += 1;
    update_busy_agent_request(request_id);
    let turn_prompt = prompt.clone();
    log::info!(
        "sending agent prompt request {request_id} to '{}' session {}",
        running.name,
        session_id
    );
    let request = super::acp::prompt_request(request_id, session_id, prompt, meta)?;
    running.process.send(&request).await?;

    loop {
        let message = recv_running_message(&mut running).await?;
        log_agent_message("prompt", &message);
        if let Some(response) = handle_agent_request(&message).await? {
            log_agent_response("prompt", &response);
            running.process.send(&response).await?;
            continue;
        }
        let turn_done = matches!(&message, JsonRpcMessage::Response(response) if json_rpc_id_eq(&response.id, request_id));
        update_session_id(&mut running, &message);
        on_message(&message);
        messages.push(message);
        if turn_done {
            log::info!(
                "agent prompt request {request_id} completed with {} messages",
                messages.len()
            );
            restore_running_agent(running);
            return Ok(AgentTurn {
                request_id,
                prompt: turn_prompt,
                pending_start: None,
                pending_end: None,
                messages,
            });
        }
    }
}

async fn handle_agent_request(message: &JsonRpcMessage) -> anyhow::Result<Option<Value>> {
    let JsonRpcMessage::Request(request) = message else {
        return Ok(None);
    };

    match request.method.as_str() {
        "session/request_permission" => {
            log::info!(
                "agent requested permission: id={}, title={:?}, kind={:?}, options={}",
                request.id,
                permission_title(request.params.as_ref()),
                permission_kind(request.params.as_ref()),
                permission_option_summary(request.params.as_ref())
            );
            let option_id = prompt_acp_permission(request.params.clone()).await?;
            log::info!(
                "agent permission response: id={}, selected option '{}'",
                request.id,
                option_id
            );
            Ok(Some(serde_json::json!({
                "jsonrpc": "2.0",
                "id": request.id.clone(),
                "result": {
                    "outcome": {
                        "outcome": "selected",
                        "optionId": option_id
                    }
                }
            })))
        }
        "agent/approval" => {
            log::info!(
                "agent requested legacy approval: id={}, title={:?}",
                request.id,
                request
                    .params
                    .as_ref()
                    .and_then(|params| params.get("title"))
                    .and_then(Value::as_str)
            );
            let decision = prompt_agent_approval(request.params.clone()).await?;
            log::info!(
                "agent legacy approval response: id={}, decision '{}'",
                request.id,
                decision
            );
            Ok(Some(serde_json::json!({
                "jsonrpc": "2.0",
                "id": request.id.clone(),
                "result": {
                    "decision": decision
                }
            })))
        }
        method => {
            log::warn!(
                "unsupported agent request: id={}, method={}, params={}",
                request.id,
                method,
                request
                    .params
                    .as_ref()
                    .map(Value::to_string)
                    .unwrap_or_else(|| "null".to_string())
            );
            Ok(Some(serde_json::json!({
                "jsonrpc": "2.0",
                "id": request.id.clone(),
                "error": {
                    "code": -32601,
                    "message": format!("unsupported agent request: {method}")
                }
            })))
        }
    }
}

async fn prompt_acp_permission(params: Option<Value>) -> anyhow::Result<String> {
    let params = params.unwrap_or(Value::Null);
    let title = params
        .get("toolCall")
        .and_then(|tool_call| tool_call.get("title"))
        .and_then(Value::as_str)
        .or_else(|| {
            params
                .get("tool_call")
                .and_then(|tool_call| tool_call.get("title"))
                .and_then(Value::as_str)
        })
        .unwrap_or("Approve agent tool call?");
    let body = acp_permission_body(&params);
    let (allow_option, reject_option) = acp_permission_choices(&params);
    let accepted = prompt_yes_no(title, body).await?;
    Ok(if accepted {
        allow_option
    } else {
        reject_option
    })
}

fn acp_permission_choices(params: &Value) -> (String, String) {
    let options = params
        .get("options")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let allow_option = acp_permission_option(&options, true)
        .or_else(|| options.first().and_then(acp_permission_option_id))
        .unwrap_or_else(|| "allow".to_string());
    let reject_option =
        acp_permission_option(&options, false).unwrap_or_else(|| "deny".to_string());

    (allow_option, reject_option)
}

fn permission_title(params: Option<&Value>) -> Option<&str> {
    params
        .and_then(|params| params.get("toolCall").or_else(|| params.get("tool_call")))
        .and_then(|tool_call| tool_call.get("title"))
        .and_then(Value::as_str)
}

fn permission_kind(params: Option<&Value>) -> Option<&str> {
    params
        .and_then(|params| params.get("toolCall").or_else(|| params.get("tool_call")))
        .and_then(|tool_call| tool_call.get("kind"))
        .and_then(Value::as_str)
}

fn permission_option_summary(params: Option<&Value>) -> String {
    let Some(options) = params
        .and_then(|params| params.get("options"))
        .and_then(Value::as_array)
    else {
        return "[]".to_string();
    };

    options
        .iter()
        .filter_map(|option| {
            let id = option
                .get("optionId")
                .or_else(|| option.get("option_id"))
                .and_then(Value::as_str)?;
            let kind = option
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            Some(format!("{id}:{kind}"))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn acp_permission_option(options: &[Value], allow: bool) -> Option<String> {
    options.iter().find_map(|option| {
        let kind = option.get("kind").and_then(Value::as_str).unwrap_or("");
        let name = option.get("name").and_then(Value::as_str).unwrap_or("");
        let is_allow = kind.starts_with("allow") || name.to_ascii_lowercase().contains("allow");
        let is_reject = kind.starts_with("reject")
            || name.to_ascii_lowercase().contains("deny")
            || name.to_ascii_lowercase().contains("reject");
        ((allow && is_allow) || (!allow && is_reject))
            .then(|| acp_permission_option_id(option))
            .flatten()
    })
}

fn acp_permission_option_id(option: &Value) -> Option<String> {
    option
        .get("optionId")
        .or_else(|| option.get("option_id"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn acp_permission_body(params: &Value) -> String {
    let Some(tool_call) = params.get("toolCall").or_else(|| params.get("tool_call")) else {
        return serde_json::to_string_pretty(params).unwrap_or_else(|_| params.to_string());
    };

    let mut sections = Vec::new();
    if let Some(kind) = tool_call.get("kind").and_then(Value::as_str) {
        sections.push(format!("Kind:\n{kind}"));
    }
    if let Some(content) = tool_call.get("content") {
        sections.push(format!(
            "Details:\n{}",
            serde_json::to_string_pretty(content).unwrap_or_else(|_| content.to_string())
        ));
    }
    if let Some(raw_input) = tool_call
        .get("rawInput")
        .or_else(|| tool_call.get("raw_input"))
    {
        sections.push(format!(
            "Input:\n{}",
            serde_json::to_string_pretty(raw_input).unwrap_or_else(|_| raw_input.to_string())
        ));
    }
    if sections.is_empty() {
        serde_json::to_string_pretty(tool_call).unwrap_or_else(|_| tool_call.to_string())
    } else {
        sections.join("\n\n")
    }
}

async fn prompt_agent_approval(params: Option<Value>) -> anyhow::Result<&'static str> {
    let params = params.unwrap_or(Value::Null);
    let title = params
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("Approve agent action?");
    let body = params
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Ok(if prompt_yes_no(title, body).await? {
        "accept"
    } else {
        "decline"
    })
}

async fn prompt_yes_no(title: &str, body: String) -> anyhow::Result<bool> {
    let prompt = format!("{title} [y/N] ");
    let (tx, rx) = oneshot::channel::<bool>();
    crate::job::dispatch_blocking(move |editor, compositor| {
        let mut tx = Some(tx);
        let body = body.clone();
        let mut prompt = crate::ui::Prompt::new(
            prompt.into(),
            None,
            |_editor, _input| Vec::new(),
            move |cx, input, event| {
                use crate::ui::PromptEvent;
                match event {
                    PromptEvent::Validate => {
                        let accepted =
                            matches!(input.trim().to_ascii_lowercase().as_str(), "y" | "yes");
                        if let Some(tx) = tx.take() {
                            let _ = tx.send(accepted);
                        }
                        if accepted {
                            cx.editor.set_status("Agent action approved");
                        } else {
                            cx.editor.set_status("Agent action declined");
                        }
                    }
                    PromptEvent::Abort => {
                        if let Some(tx) = tx.take() {
                            let _ = tx.send(false);
                        }
                        cx.editor.set_status("Agent action declined");
                    }
                    PromptEvent::Update => {}
                }
            },
        );
        prompt.doc_fn =
            Box::new(move |_| (!body.is_empty()).then(|| std::borrow::Cow::Owned(body.clone())));
        prompt.recalculate_completion(editor);
        compositor.push(Box::new(prompt));
    });

    Ok(rx.await.unwrap_or(false))
}

pub async fn send_prompt(prompt: String, meta: Option<Value>) -> anyhow::Result<u64> {
    let Some(mut running) = take_running_agent() else {
        anyhow::bail!("no agent is running");
    };

    let Some(session_id) = running.session_id.clone() else {
        restore_running_agent(running);
        anyhow::bail!(
            "agent session id is not known yet; run :agent recv until :agent status shows a session"
        );
    };

    let request_id = running.next_request_id;
    running.next_request_id += 1;
    let request = super::acp::prompt_request(request_id, session_id, prompt, meta)?;
    log::info!(
        "sending detached agent prompt request {request_id} to '{}'",
        running.name
    );
    let send_result = running.process.send(&request).await;
    restore_running_agent(running);
    send_result?;
    Ok(request_id)
}

#[derive(Debug, serde::Serialize)]
pub struct AgentTurn {
    pub request_id: u64,
    pub prompt: String,
    pub pending_start: Option<usize>,
    pub pending_end: Option<usize>,
    pub messages: Vec<JsonRpcMessage>,
}

async fn recv_running_message(running: &mut RunningAgent) -> anyhow::Result<JsonRpcMessage> {
    match running.process.recv().await {
        Ok(message) => Ok(message),
        Err(err) => {
            let exit_status = running.process.try_wait()?;
            let stderr = running.process.stderr_snapshot().await?;

            let mut message = format!("failed to read agent message: {err}");
            if let Some(status) = exit_status {
                message.push_str(&format!("; process exited with {status}"));
            }
            if let Some(stderr) = stderr {
                message.push_str(&format!("; stderr: {stderr}"));
            }

            anyhow::bail!(message);
        }
    }
}

fn take_running_agent() -> Option<RunningAgent> {
    let mut agent = AGENT_RUNTIME.lock().expect("agent runtime lock poisoned");
    agent.take()
}

fn restore_running_agent(running: RunningAgent) {
    let mut agent = AGENT_RUNTIME.lock().expect("agent runtime lock poisoned");
    *agent = Some(running);
}

pub fn status() -> AgentRuntimeStatus {
    let agent = AGENT_RUNTIME.lock().expect("agent runtime lock poisoned");
    match agent.as_ref() {
        Some(agent) => AgentRuntimeStatus::Running {
            name: agent.name.clone(),
            session_id: agent.session_id.clone(),
            next_request_id: agent.next_request_id,
        },
        None => AGENT_BUSY
            .lock()
            .expect("agent busy lock poisoned")
            .clone()
            .map_or(AgentRuntimeStatus::Stopped, AgentRuntimeStatus::Busy),
    }
}

fn set_busy_agent(status: AgentBusyStatus) -> AgentBusyGuard {
    *AGENT_BUSY.lock().expect("agent busy lock poisoned") = Some(status);
    AgentBusyGuard
}

fn update_busy_agent_session(session_id: Option<String>) {
    if let Some(status) = AGENT_BUSY
        .lock()
        .expect("agent busy lock poisoned")
        .as_mut()
    {
        status.session_id = session_id;
    }
}

fn update_busy_agent_request(request_id: u64) {
    if let Some(status) = AGENT_BUSY
        .lock()
        .expect("agent busy lock poisoned")
        .as_mut()
    {
        status.request_id = Some(request_id);
    }
}

fn clear_busy_agent() {
    *AGENT_BUSY.lock().expect("agent busy lock poisoned") = None;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentRuntimeStatus {
    Running {
        name: String,
        session_id: Option<String>,
        next_request_id: u64,
    },
    Busy(AgentBusyStatus),
    Stopped,
}

#[derive(Clone, Debug)]
pub struct AgentPatchProposal {
    pub patch: String,
    pub cwd: String,
    pub source_path: Option<String>,
    pub request_id: u64,
}

#[derive(Clone, Debug)]
pub struct AgentTranscriptTurn {
    pub id: u64,
    pub kind: AgentTranscriptKind,
    pub prompt: String,
    pub response: Option<String>,
    pub status_message: Option<String>,
    pub status: AgentTranscriptStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentTranscriptKind {
    Chat,
    Explain,
    Fix,
    Refactor,
    Edit,
}

impl AgentTranscriptKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Explain => "explain",
            Self::Fix => "fix",
            Self::Refactor => "refactor",
            Self::Edit => "edit",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentTranscriptStatus {
    Pending,
    Complete,
    Cancelled,
    Failed,
}

pub fn append_transcript_turn(id: u64, kind: AgentTranscriptKind, prompt: String) {
    AGENT_TRANSCRIPT_TURNS
        .lock()
        .expect("agent transcript turns lock poisoned")
        .push(AgentTranscriptTurn {
            id,
            kind,
            prompt,
            response: None,
            status_message: Some("Working...".to_string()),
            status: AgentTranscriptStatus::Pending,
        });
}

pub fn complete_transcript_turn(id: u64, response: String) {
    update_transcript_turn(id, AgentTranscriptStatus::Complete, Some(response));
}

pub fn append_transcript_turn_response(id: u64, chunk: String) {
    if chunk.is_empty() {
        return;
    }

    let mut turns = AGENT_TRANSCRIPT_TURNS
        .lock()
        .expect("agent transcript turns lock poisoned");
    if let Some(turn) = turns.iter_mut().find(|turn| turn.id == id) {
        turn.response
            .get_or_insert_with(String::new)
            .push_str(&chunk);
    }
}

pub fn update_transcript_turn_status(id: u64, status_message: String) {
    let mut turns = AGENT_TRANSCRIPT_TURNS
        .lock()
        .expect("agent transcript turns lock poisoned");
    if let Some(turn) = turns.iter_mut().find(|turn| turn.id == id) {
        turn.status_message = Some(status_message);
    }
}

pub fn fail_transcript_turn(id: u64, response: String) {
    update_transcript_turn(id, AgentTranscriptStatus::Failed, Some(response));
}

pub fn cancel_pending_transcript_turns() -> usize {
    let mut turns = AGENT_TRANSCRIPT_TURNS
        .lock()
        .expect("agent transcript turns lock poisoned");
    let mut cancelled = 0;
    for turn in turns.iter_mut() {
        if turn.status == AgentTranscriptStatus::Pending {
            turn.status = AgentTranscriptStatus::Cancelled;
            turn.response = Some("Cancelled".to_string());
            cancelled += 1;
        }
    }
    cancelled
}

pub fn clear_transcript_turns() {
    AGENT_TRANSCRIPT_TURNS
        .lock()
        .expect("agent transcript turns lock poisoned")
        .clear();
}

pub fn render_transcript() -> String {
    let turns = AGENT_TRANSCRIPT_TURNS
        .lock()
        .expect("agent transcript turns lock poisoned");
    let mut rendered = String::new();
    for (index, turn) in turns.iter().enumerate() {
        if index > 0 {
            rendered.push_str("\n\n---\n\n");
        }

        rendered.push_str(&format!("**You:**\n\n{}\n\n", turn.prompt.trim()));
        rendered.push_str("**Codex:**\n\n");
        match turn.status {
            AgentTranscriptStatus::Pending => {
                let status_message = turn.status_message.as_deref().unwrap_or("Working...");
                if let Some(response) = turn
                    .response
                    .as_ref()
                    .filter(|response| !response.is_empty())
                {
                    rendered.push_str(response);
                    rendered.push_str("\n\n");
                    rendered.push_str(status_message);
                } else {
                    rendered.push_str(status_message);
                }
            }
            AgentTranscriptStatus::Cancelled => rendered.push_str("Cancelled"),
            AgentTranscriptStatus::Failed | AgentTranscriptStatus::Complete => {
                rendered.push_str(turn.response.as_deref().unwrap_or(""));
            }
        }
    }
    rendered
}

fn update_transcript_turn(id: u64, status: AgentTranscriptStatus, response: Option<String>) {
    let mut turns = AGENT_TRANSCRIPT_TURNS
        .lock()
        .expect("agent transcript turns lock poisoned");
    if let Some(turn) = turns.iter_mut().find(|turn| turn.id == id) {
        turn.status = status;
        turn.response = response;
    }
}

pub fn transcript_doc_id() -> Option<DocumentId> {
    *AGENT_TRANSCRIPT
        .lock()
        .expect("agent transcript lock poisoned")
}

pub fn set_transcript_doc_id(doc_id: DocumentId) {
    *AGENT_TRANSCRIPT
        .lock()
        .expect("agent transcript lock poisoned") = Some(doc_id);
}

pub fn latest_patch() -> Option<AgentPatchProposal> {
    AGENT_LATEST_PATCH
        .lock()
        .expect("agent latest patch lock poisoned")
        .clone()
}

pub fn set_latest_patch(patch: AgentPatchProposal) {
    *AGENT_LATEST_PATCH
        .lock()
        .expect("agent latest patch lock poisoned") = Some(patch);
}

pub fn clear_latest_patch() {
    *AGENT_LATEST_PATCH
        .lock()
        .expect("agent latest patch lock poisoned") = None;
}

fn update_session_id(agent: &mut RunningAgent, message: &JsonRpcMessage) {
    let JsonRpcMessage::Response(response) = message else {
        return;
    };

    if !json_rpc_id_eq(&response.id, 2) {
        return;
    }

    let Some(session_id) = response
        .result
        .as_ref()
        .and_then(|result| result.get("sessionId"))
        .and_then(|session_id| session_id.as_str())
    else {
        return;
    };

    agent.session_id = Some(session_id.to_string());
}

fn session_new_failure_message(message: &JsonRpcMessage) -> Option<String> {
    let JsonRpcMessage::Response(response) = message else {
        return None;
    };

    if !json_rpc_id_eq(&response.id, 2) {
        return None;
    }

    if response
        .result
        .as_ref()
        .and_then(|result| result.get("sessionId"))
        .and_then(|session_id| session_id.as_str())
        .is_some()
    {
        return None;
    }

    Some(match &response.error {
        Some(error) => format!("agent session/new failed: {}", error.message),
        None => "agent session/new response did not include a sessionId".to_string(),
    })
}

fn json_rpc_id_eq(id: &Value, expected: u64) -> bool {
    id.as_u64().is_some_and(|id| id == expected)
        || id
            .as_str()
            .and_then(|id| id.parse::<u64>().ok())
            .is_some_and(|id| id == expected)
}

fn log_agent_message(phase: &str, message: &JsonRpcMessage) {
    match message {
        JsonRpcMessage::Response(response) => {
            log::info!(
                "agent {phase} response: id={}, result={}, error={:?}",
                response.id,
                response
                    .result
                    .as_ref()
                    .map(response_result_summary)
                    .unwrap_or_else(|| "none".to_string()),
                response.error.as_ref().map(|error| error.message.as_str())
            );
        }
        JsonRpcMessage::Request(request) => {
            log::info!(
                "agent {phase} request: id={}, method={}",
                request.id,
                request.method
            );
        }
        JsonRpcMessage::Notification(notification) => {
            log::info!(
                "agent {phase} notification: method={}, update={:?}, tool={:?}, status={:?}",
                notification.method,
                notification_update_kind(notification.params.as_ref()),
                notification_tool_kind(notification.params.as_ref()),
                notification_tool_status(notification.params.as_ref())
            );
        }
    }
}

fn log_agent_response(phase: &str, response: &Value) {
    log::info!("agent {phase} client response: {response}");
}

fn response_result_summary(result: &Value) -> String {
    let session_id = result
        .get("sessionId")
        .and_then(Value::as_str)
        .map(|session_id| format!("sessionId={session_id}"));
    let config_mode = result
        .get("configOptions")
        .and_then(Value::as_array)
        .and_then(|options| {
            options.iter().find_map(|option| {
                let category = option.get("category").and_then(Value::as_str);
                let id = option.get("id").and_then(Value::as_str);
                (category == Some("mode") || id == Some("mode"))
                    .then(|| option.get("currentValue").and_then(Value::as_str))
                    .flatten()
            })
        })
        .map(|mode| format!("configMode={mode}"));
    let session_mode = result
        .get("modes")
        .and_then(|modes| modes.get("currentModeId"))
        .and_then(Value::as_str)
        .map(|mode| format!("mode={mode}"));

    let summary = [session_id, config_mode, session_mode]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(", ");
    if summary.is_empty() {
        "ok".to_string()
    } else {
        summary
    }
}

fn notification_update_kind(params: Option<&Value>) -> Option<&str> {
    params?
        .get("update")?
        .get("sessionUpdate")
        .and_then(Value::as_str)
}

fn notification_tool_kind(params: Option<&Value>) -> Option<&str> {
    params?
        .get("update")?
        .get("toolCall")?
        .get("kind")
        .and_then(Value::as_str)
}

fn notification_tool_status(params: Option<&Value>) -> Option<&str> {
    params?
        .get("update")?
        .get("toolCall")?
        .get("status")
        .and_then(Value::as_str)
}

fn approval_mode_request(
    running: &mut RunningAgent,
    session_id: &str,
    session_response: Option<&Value>,
) -> anyhow::Result<Option<JsonRpcRequest>> {
    let Some(response) = session_response else {
        return Ok(None);
    };
    let Some(result) = response.get("result") else {
        return Ok(None);
    };

    if let Some((config_id, value)) = preferred_config_mode(result) {
        let request_id = running.next_request_id;
        running.next_request_id += 1;
        return Ok(Some(super::acp::set_config_option_request(
            request_id,
            session_id.to_string(),
            config_id,
            value,
        )?));
    }

    if let Some(mode_id) = preferred_session_mode(result) {
        let request_id = running.next_request_id;
        running.next_request_id += 1;
        return Ok(Some(super::acp::set_mode_request(
            request_id,
            session_id.to_string(),
            mode_id,
        )?));
    }

    Ok(None)
}

fn preferred_config_mode(result: &Value) -> Option<(String, String)> {
    let config_options = result.get("configOptions")?.as_array()?;
    for option in config_options {
        let category = option.get("category").and_then(Value::as_str);
        let id = option.get("id").and_then(Value::as_str)?;
        if category != Some("mode") && id != "mode" {
            continue;
        }

        let current = option.get("currentValue").and_then(Value::as_str);
        let values = config_option_values(option);
        let value = preferred_mode_value(&values, current)?;
        return Some((id.to_string(), value));
    }

    None
}

fn config_option_values(option: &Value) -> Vec<String> {
    option
        .get("options")
        .and_then(Value::as_array)
        .map(|options| {
            options
                .iter()
                .filter_map(|option| option.get("value").and_then(Value::as_str))
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn preferred_session_mode(result: &Value) -> Option<String> {
    let modes = result.get("modes")?;
    let current = modes.get("currentModeId").and_then(Value::as_str);
    let values = modes
        .get("availableModes")?
        .as_array()?
        .iter()
        .filter_map(|mode| mode.get("id").and_then(Value::as_str))
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();

    preferred_mode_value(&values, current)
}

fn preferred_mode_value(values: &[String], current: Option<&str>) -> Option<String> {
    const PREFERRED_APPROVAL_MODES: &[&str] = &["read-only", "suggest", "ask"];
    for preferred in PREFERRED_APPROVAL_MODES {
        if values.iter().any(|value| value == preferred) && current != Some(*preferred) {
            return Some((*preferred).to_string());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn falls_back_to_suggest_config_mode() {
        let result = json!({
            "configOptions": [
                {
                    "id": "mode",
                    "category": "mode",
                    "currentValue": "full-auto",
                    "options": [
                        { "value": "suggest" },
                        { "value": "auto-edit" },
                        { "value": "full-auto" }
                    ]
                }
            ]
        });

        assert_eq!(
            preferred_config_mode(&result),
            Some(("mode".to_string(), "suggest".to_string()))
        );
    }

    #[test]
    fn prefers_read_only_codex_config_mode() {
        let result = json!({
            "configOptions": [
                {
                    "id": "mode",
                    "category": "mode",
                    "currentValue": "auto",
                    "options": [
                        { "value": "read-only" },
                        { "value": "auto" },
                        { "value": "full-access" }
                    ]
                }
            ]
        });

        assert_eq!(
            preferred_config_mode(&result),
            Some(("mode".to_string(), "read-only".to_string()))
        );
    }

    #[test]
    fn falls_back_to_ask_session_mode() {
        let result = json!({
            "modes": {
                "currentModeId": "code",
                "availableModes": [
                    { "id": "ask" },
                    { "id": "code" }
                ]
            }
        });

        assert_eq!(preferred_session_mode(&result), Some("ask".to_string()));
    }

    #[test]
    fn maps_acp_permission_options_to_allow_and_deny_choices() {
        let params = json!({
            "sessionId": "session-1",
            "toolCall": {
                "toolCallId": "call-1",
                "title": "Edit src/main.rs",
                "kind": "edit",
                "status": "pending"
            },
            "options": [
                {
                    "optionId": "approved",
                    "name": "Yes, proceed",
                    "kind": "allow_once"
                },
                {
                    "optionId": "approved-for-session",
                    "name": "Yes, and don't ask again",
                    "kind": "allow_always"
                },
                {
                    "optionId": "denied",
                    "name": "No, continue without running",
                    "kind": "reject_once"
                }
            ]
        });

        assert_eq!(
            acp_permission_choices(&params),
            ("approved".to_string(), "denied".to_string())
        );
    }

    #[test]
    fn acp_permission_options_have_stable_fallbacks() {
        assert_eq!(
            acp_permission_choices(&json!({})),
            ("allow".to_string(), "deny".to_string())
        );
    }
}
