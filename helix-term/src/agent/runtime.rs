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
    while running.session_id.is_none() {
        let message = recv_running_message(&mut running).await?;
        update_session_id(&mut running, &message);
        update_busy_agent_session(running.session_id.clone());
        messages.push(message);
    }

    let session_id = running
        .session_id
        .clone()
        .expect("session id checked before prompt send");
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
        if let Some(response) = handle_agent_request(&message).await? {
            running.process.send(&response).await?;
            continue;
        }
        let turn_done =
            matches!(&message, JsonRpcMessage::Response(response) if response.id == request_id);
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
        "agent/approval" => {
            let decision = prompt_agent_approval(request.params.clone()).await?;
            Ok(Some(serde_json::json!({
                "jsonrpc": "2.0",
                "id": request.id,
                "result": {
                    "decision": decision
                }
            })))
        }
        method => Ok(Some(serde_json::json!({
            "jsonrpc": "2.0",
            "id": request.id,
            "error": {
                "code": -32601,
                "message": format!("unsupported agent request: {method}")
            }
        }))),
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

    Ok(if rx.await.unwrap_or(false) {
        "accept"
    } else {
        "decline"
    })
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

    if response.id != 2 {
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
