use helix_event::runtime_local;
use helix_view::editor::AgentLaunchConfig;
use std::sync::Mutex;

use super::{
    acp::{JsonRpcMessage, JsonRpcRequest},
    transport::AgentProcess,
};

runtime_local! {
    static AGENT_RUNTIME: Mutex<Option<RunningAgent>> = Mutex::new(None);
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

    let message = running.process.recv().await;
    if let Ok(message) = &message {
        update_session_id(&mut running, message);
    }
    restore_running_agent(running);
    message
}

pub async fn send_prompt(prompt: String) -> anyhow::Result<u64> {
    let Some(mut running) = take_running_agent() else {
        anyhow::bail!("no agent is running");
    };

    let Some(session_id) = running.session_id.clone() else {
        restore_running_agent(running);
        anyhow::bail!("agent session id is not known yet; run :agent recv after :agent start");
    };

    let request_id = running.next_request_id;
    running.next_request_id += 1;
    let request = super::acp::prompt_request(request_id, session_id, prompt)?;
    let send_result = running.process.send(&request).await;
    restore_running_agent(running);
    send_result?;
    Ok(request_id)
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
