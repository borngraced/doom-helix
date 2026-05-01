use helix_event::runtime_local;
use helix_view::editor::AgentLaunchConfig;
use helix_view::DocumentId;
use serde_json::Value;
use std::sync::Mutex;

use super::{
    acp::{JsonRpcMessage, JsonRpcRequest},
    transport::AgentProcess,
};

runtime_local! {
    static AGENT_RUNTIME: Mutex<Option<RunningAgent>> = Mutex::new(None);
    static AGENT_TRANSCRIPT: Mutex<Option<DocumentId>> = Mutex::new(None);
    static AGENT_LATEST_PATCH: Mutex<Option<AgentPatchProposal>> = Mutex::new(None);
}

pub struct RunningAgent {
    pub name: String,
    pub process: AgentProcess,
    pub session_id: Option<String>,
    pub next_request_id: u64,
}

pub async fn start(
    launch_config: AgentLaunchConfig,
    handshake: Vec<JsonRpcRequest>,
) -> anyhow::Result<()> {
    if let Some(mut running) = take_running_agent() {
        running.process.kill().await?;
    }

    let mut process = AgentProcess::spawn(&launch_config).await?;
    for message in handshake {
        process.send(&message).await?;
    }

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

    running.process.kill().await?;
    Ok(Some(running.name))
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
    let Some(mut running) = take_running_agent() else {
        anyhow::bail!("no agent is running");
    };

    let mut messages = Vec::new();
    while running.session_id.is_none() {
        let message = recv_running_message(&mut running).await?;
        update_session_id(&mut running, &message);
        messages.push(message);
    }

    let session_id = running
        .session_id
        .clone()
        .expect("session id checked before prompt send");
    let request_id = running.next_request_id;
    running.next_request_id += 1;
    let turn_prompt = prompt.clone();
    let request = super::acp::prompt_request(request_id, session_id, prompt, meta)?;
    running.process.send(&request).await?;

    loop {
        let message = recv_running_message(&mut running).await?;
        let turn_done =
            matches!(&message, JsonRpcMessage::Response(response) if response.id == request_id);
        update_session_id(&mut running, &message);
        messages.push(message);
        if turn_done {
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
        None => AgentRuntimeStatus::Stopped,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentRuntimeStatus {
    Running {
        name: String,
        session_id: Option<String>,
        next_request_id: u64,
    },
    Stopped,
}

#[derive(Clone, Debug)]
pub struct AgentPatchProposal {
    pub patch: String,
    pub cwd: String,
    pub source_path: Option<String>,
    pub request_id: u64,
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
