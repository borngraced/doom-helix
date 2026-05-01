use serde::Serialize;
use std::process::Stdio;
use tokio::{
    io::{AsyncWriteExt, BufReader, BufWriter},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command},
};

use super::config::AgentLaunchConfig;

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

    pub async fn send<T: Serialize>(&mut self, message: &T) -> anyhow::Result<()> {
        let frame = encode_content_length_message(message)?;
        self.stdin.write_all(&frame).await?;
        self.stdin.flush().await?;
        Ok(())
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

#[cfg(test)]
mod tests {
    use serde_json::json;

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
}
