use serde::de::DeserializeOwned;
use serde::Serialize;
use std::process::Stdio;
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, BufWriter},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command},
};

use super::{acp, config::AgentLaunchConfig, session};

pub struct AgentProcess {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    stderr: Option<BufReader<ChildStderr>>,
}

impl AgentProcess {
    pub async fn spawn(config: &AgentLaunchConfig) -> anyhow::Result<Self> {
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

    pub async fn spawn_and_handshake(
        config: &AgentLaunchConfig,
        editor: &helix_view::Editor,
    ) -> anyhow::Result<Self> {
        let mut process = Self::spawn(config).await?;
        process.send_session_handshake(editor).await?;
        Ok(process)
    }

    pub async fn send<T: Serialize>(&mut self, message: &T) -> anyhow::Result<()> {
        let frame = encode_content_length_message(message)?;
        self.stdin.write_all(&frame).await?;
        self.stdin.flush().await?;
        Ok(())
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
        read_content_length_message(&mut self.stdout).await
    }

    pub fn stdout(&mut self) -> &mut BufReader<ChildStdout> {
        &mut self.stdout
    }

    pub fn stderr(&mut self) -> Option<&mut BufReader<ChildStderr>> {
        self.stderr.as_mut()
    }

    pub async fn wait(&mut self) -> anyhow::Result<std::process::ExitStatus> {
        Ok(self.child.wait().await?)
    }
}

pub fn encode_content_length_message<T: Serialize>(message: &T) -> anyhow::Result<Vec<u8>> {
    let body = serde_json::to_string(message)?;
    Ok(encode_json_content_length_message(&body))
}

pub fn encode_json_content_length_message(body: &str) -> Vec<u8> {
    let mut message = Vec::with_capacity(
        "Content-Length: \r\n\r\n".len() + body.len().to_string().len() + body.len(),
    );
    message.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
    message.extend_from_slice(body.as_bytes());
    message
}

pub async fn read_content_length_message<T: DeserializeOwned>(
    reader: &mut (impl AsyncBufRead + Unpin),
) -> anyhow::Result<T> {
    let mut buffer = String::new();
    let mut content_length = None;

    loop {
        buffer.clear();
        if reader.read_line(&mut buffer).await? == 0 {
            anyhow::bail!("agent stream closed while reading message header");
        }

        if buffer == "\r\n" {
            break;
        }

        let Some((name, value)) = buffer.trim().split_once(": ") else {
            continue;
        };

        if name.eq_ignore_ascii_case("Content-Length") {
            content_length = Some(value.parse::<usize>()?);
        }
    }

    let content_length =
        content_length.ok_or_else(|| anyhow::anyhow!("agent message is missing Content-Length"))?;
    let mut content = vec![0; content_length];
    reader.read_exact(&mut content).await?;
    Ok(serde_json::from_slice(&content)?)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tokio::io::{duplex, AsyncWriteExt};

    use super::*;

    #[test]
    fn encodes_content_length_frame() {
        let message = encode_json_content_length_message(r#"{"jsonrpc":"2.0"}"#);
        assert_eq!(
            String::from_utf8(message).unwrap(),
            "Content-Length: 17\r\n\r\n{\"jsonrpc\":\"2.0\"}"
        );
    }

    #[test]
    fn encodes_serializable_message() {
        let message = encode_content_length_message(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize"
        }))
        .unwrap();

        let message = String::from_utf8(message).unwrap();
        assert!(message.starts_with("Content-Length: "));
        assert!(message.ends_with(r#""method":"initialize"}"#));
    }

    #[tokio::test]
    async fn reads_content_length_frame() {
        let (mut writer, reader) = duplex(128);
        let frame = encode_json_content_length_message(r#"{"ok":true}"#);
        writer.write_all(&frame).await.unwrap();
        drop(writer);

        let mut reader = tokio::io::BufReader::new(reader);
        let message: serde_json::Value = read_content_length_message(&mut reader).await.unwrap();
        assert_eq!(message, json!({ "ok": true }));
    }
}
