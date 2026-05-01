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
    restore_running_agent(running);
    message
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
        },
        None => AgentRuntimeStatus::Stopped,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentRuntimeStatus {
    Running { name: String },
    Stopped,
}
