use serde::de::DeserializeOwned;
use serde::Serialize;
use std::{process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, BufWriter},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command},
    time::{sleep, timeout, Instant},
};
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};

use super::{acp, session};
use futures_util::{SinkExt, StreamExt};
use helix_view::editor::{AgentLaunchConfig, AgentTransport};

pub struct AgentProcess {
    inner: AgentConnection,
}

enum AgentConnection {
    Stdio(StdioAgentProcess),
    WebSocket(WebSocketAgentProcess),
}

struct StdioAgentProcess {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    stderr: Option<BufReader<ChildStderr>>,
}

struct WebSocketAgentProcess {
    stream: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    child: Option<Child>,
    stderr: Option<BufReader<ChildStderr>>,
}

impl AgentProcess {
    pub async fn spawn(config: &AgentLaunchConfig) -> anyhow::Result<Self> {
        match config.transport {
            AgentTransport::Stdio => Ok(Self {
                inner: AgentConnection::Stdio(StdioAgentProcess::spawn(config).await?),
            }),
            AgentTransport::Websocket => Ok(Self {
                inner: AgentConnection::WebSocket(WebSocketAgentProcess::connect(config).await?),
            }),
        }
    }

    pub async fn spawn_and_handshake(
        config: &AgentLaunchConfig,
        editor: &helix_view::Editor,
    ) -> anyhow::Result<Self> {
        let mut process = Self::spawn(config).await?;
        process.send_session_handshake(editor).await?;
        Ok(process)
    }

    pub async fn send<T: Serialize>(&mut self, message: &T) -> anyhow::Result<()> {
        match &mut self.inner {
            AgentConnection::Stdio(process) => process.send(message).await,
            AgentConnection::WebSocket(process) => process.send(message).await,
        }
    }

    pub async fn send_session_handshake(
        &mut self,
        editor: &helix_view::Editor,
    ) -> anyhow::Result<()> {
        self.send(&acp::initialize_request(1)?).await?;
        self.send(&acp::new_session_request(2, session::new_session(editor))?)
            .await?;
        Ok(())
    }

    pub async fn recv<T: DeserializeOwned>(&mut self) -> anyhow::Result<T> {
        match &mut self.inner {
            AgentConnection::Stdio(process) => process.recv().await,
            AgentConnection::WebSocket(process) => process.recv().await,
        }
    }

    pub fn try_wait(&mut self) -> anyhow::Result<Option<std::process::ExitStatus>> {
        match &mut self.inner {
            AgentConnection::Stdio(process) => process.try_wait(),
            AgentConnection::WebSocket(_) => Ok(None),
        }
    }

    pub async fn stderr_snapshot(&mut self) -> anyhow::Result<Option<String>> {
        match &mut self.inner {
            AgentConnection::Stdio(process) => process.stderr_snapshot().await,
            AgentConnection::WebSocket(process) => process.stderr_snapshot().await,
        }
    }

    pub async fn kill(&mut self) -> anyhow::Result<()> {
        match &mut self.inner {
            AgentConnection::Stdio(process) => process.kill().await,
            AgentConnection::WebSocket(process) => process.close().await,
        }
    }
}

impl StdioAgentProcess {
    async fn spawn(config: &AgentLaunchConfig) -> anyhow::Result<Self> {
        let mut child = Command::new(&config.command)
            .args(&config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("agent process stdin is unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("agent process stdout is unavailable"))?;
        let stderr = child.stderr.take().map(BufReader::new);

        Ok(Self {
            child,
            stdin: BufWriter::new(stdin),
            stdout: BufReader::new(stdout),
            stderr,
        })
    }

    async fn send<T: Serialize>(&mut self, message: &T) -> anyhow::Result<()> {
        let frame = encode_newline_message(message)?;
        self.stdin.write_all(&frame).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn recv<T: DeserializeOwned>(&mut self) -> anyhow::Result<T> {
        read_newline_message(&mut self.stdout).await
    }

    fn try_wait(&mut self) -> anyhow::Result<Option<std::process::ExitStatus>> {
        Ok(self.child.try_wait()?)
    }

    async fn stderr_snapshot(&mut self) -> anyhow::Result<Option<String>> {
        let Some(stderr) = self.stderr.as_mut() else {
            return Ok(None);
        };

        let mut output = String::new();
        let read = timeout(
            Duration::from_millis(100),
            stderr.read_to_string(&mut output),
        )
        .await;
        match read {
            Ok(result) => {
                result?;
                let output = output.trim().to_string();
                Ok((!output.is_empty()).then_some(output))
            }
            Err(_) => Ok(None),
        }
    }

    async fn kill(&mut self) -> anyhow::Result<()> {
        self.child.kill().await?;
        Ok(())
    }
}

impl WebSocketAgentProcess {
    async fn connect(config: &AgentLaunchConfig) -> anyhow::Result<Self> {
        if let Ok(stream) = Self::connect_once(&config.url).await {
            return Ok(Self {
                stream,
                child: None,
                stderr: None,
            });
        }

        let (mut child, mut stderr) = if config.command.trim().is_empty() {
            (None, None)
        } else {
            let mut child = Command::new(&config.command)
                .args(&config.args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()?;
            let stderr = child.stderr.take().map(BufReader::new);
            (Some(child), stderr)
        };

        let retry_until = child
            .is_some()
            .then(|| Instant::now() + Duration::from_secs(2));

        loop {
            match Self::connect_once(&config.url).await {
                Ok(stream) => {
                    return Ok(Self {
                        stream,
                        child,
                        stderr,
                    })
                }
                Err(err) if retry_until.is_some_and(|deadline| Instant::now() < deadline) => {
                    sleep(Duration::from_millis(50)).await;
                    let _ = err;
                }
                Err(err) => {
                    let mut message = format!("failed to connect agent websocket: {err}");
                    if let Some(stderr) = stderr.as_mut() {
                        let mut output = String::new();
                        if timeout(
                            Duration::from_millis(100),
                            stderr.read_to_string(&mut output),
                        )
                        .await
                        .is_ok_and(|result| result.is_ok())
                        {
                            let output = output.trim();
                            if !output.is_empty() {
                                message.push_str(&format!("; stderr: {output}"));
                            }
                        }
                    }
                    if let Some(child) = child.as_mut() {
                        let _ = child.kill().await;
                    }
                    return Err(anyhow::anyhow!(message));
                }
            }
        }
    }

    async fn connect_once(
        url: &str,
    ) -> anyhow::Result<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>> {
        let (stream, _) = timeout(Duration::from_millis(500), connect_async(url)).await??;
        Ok(stream)
    }

    async fn send<T: Serialize>(&mut self, message: &T) -> anyhow::Result<()> {
        let body = serde_json::to_string(message)?;
        self.stream.send(Message::Text(body.into())).await?;
        Ok(())
    }

    async fn recv<T: DeserializeOwned>(&mut self) -> anyhow::Result<T> {
        loop {
            let Some(message) = self.stream.next().await else {
                anyhow::bail!("agent websocket closed while reading message");
            };
            match message? {
                Message::Text(text) => return Ok(serde_json::from_str(&text)?),
                Message::Binary(bytes) => return Ok(serde_json::from_slice(&bytes)?),
                Message::Close(frame) => {
                    let reason = frame
                        .as_ref()
                        .map(|frame| frame.reason.to_string())
                        .filter(|reason| !reason.is_empty())
                        .unwrap_or_else(|| "no close reason".to_string());
                    anyhow::bail!("agent websocket closed: {reason}");
                }
                Message::Ping(bytes) => {
                    self.stream.send(Message::Pong(bytes)).await?;
                }
                Message::Pong(_) | Message::Frame(_) => {}
            }
        }
    }

    async fn close(&mut self) -> anyhow::Result<()> {
        self.stream.close(None).await?;
        if let Some(child) = &mut self.child {
            child.kill().await?;
        }
        Ok(())
    }

    async fn stderr_snapshot(&mut self) -> anyhow::Result<Option<String>> {
        let Some(stderr) = self.stderr.as_mut() else {
            return Ok(None);
        };

        let mut output = String::new();
        let read = timeout(
            Duration::from_millis(100),
            stderr.read_to_string(&mut output),
        )
        .await;
        match read {
            Ok(result) => {
                result?;
                let output = output.trim().to_string();
                Ok((!output.is_empty()).then_some(output))
            }
            Err(_) => Ok(None),
        }
    }
}

pub fn encode_newline_message<T: Serialize>(message: &T) -> anyhow::Result<Vec<u8>> {
    let body = serde_json::to_string(message)?;
    Ok(encode_json_newline_message(&body))
}

pub fn encode_json_newline_message(body: &str) -> Vec<u8> {
    let mut message = Vec::with_capacity(body.len() + 1);
    message.extend_from_slice(body.as_bytes());
    message.push(b'\n');
    message
}

pub async fn read_newline_message<T: DeserializeOwned>(
    reader: &mut (impl AsyncBufRead + Unpin),
) -> anyhow::Result<T> {
    let mut line = String::new();
    if reader.read_line(&mut line).await? == 0 {
        anyhow::bail!("agent stream closed while reading message");
    }

    Ok(serde_json::from_str(line.trim_end_matches(['\r', '\n']))?)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tokio::io::{duplex, AsyncWriteExt};

    use super::*;

    #[test]
    fn encodes_newline_frame() {
        let message = encode_json_newline_message(r#"{"jsonrpc":"2.0"}"#);
        assert_eq!(
            String::from_utf8(message).unwrap(),
            "{\"jsonrpc\":\"2.0\"}\n"
        );
    }

    #[test]
    fn encodes_serializable_message() {
        let message = encode_newline_message(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize"
        }))
        .unwrap();

        let message = String::from_utf8(message).unwrap();
        assert!(message.ends_with("}\n"));
        assert!(message.contains(r#""method":"initialize""#));
    }

    #[tokio::test]
    async fn reads_newline_frame() {
        let (mut writer, reader) = duplex(128);
        let frame = encode_json_newline_message(r#"{"ok":true}"#);
        writer.write_all(&frame).await.unwrap();
        drop(writer);

        let mut reader = tokio::io::BufReader::new(reader);
        let message: serde_json::Value = read_newline_message(&mut reader).await.unwrap();
        assert_eq!(message, json!({ "ok": true }));
    }
}
