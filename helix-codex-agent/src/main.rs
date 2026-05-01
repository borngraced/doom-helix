use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{
    env,
    io::{self, BufRead, BufReader, Write},
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

const ACP_PROTOCOL_VERSION: u64 = 1;

fn main() -> Result<()> {
    let stdin = io::stdin();
    let mut input = BufReader::new(stdin.lock());
    let mut output = io::stdout().lock();
    let mut state = AgentState::default();

    while let Some(message) = read_content_length_message(&mut input)? {
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            continue;
        };

        let id = message.get("id").cloned();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        match (id, method) {
            (Some(id), "initialize") => {
                let protocol_version = params
                    .get("protocolVersion")
                    .and_then(Value::as_u64)
                    .unwrap_or(ACP_PROTOCOL_VERSION);
                write_message(&mut output, &initialize_response(id, protocol_version)?)?;
            }
            (Some(id), "session/new") => {
                state.cwd = params
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
                    .or_else(|| env::current_dir().ok().map(|cwd| cwd.display().to_string()));
                state.session_id = Some(new_session_id());
                write_message(
                    &mut output,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "sessionId": state.session_id,
                            "_meta": {
                                "helixCodexAgent": {
                                    "backend": "codex exec"
                                }
                            }
                        }
                    }),
                )?;
            }
            (Some(id), "session/prompt") => {
                let session_id = params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .unwrap_or("helix-codex-session")
                    .to_string();
                let prompt = prompt_text(&params);
                let codex_prompt = codex_prompt(&prompt, params.get("_meta"));
                let codex_output = run_codex_exec(state.cwd.as_deref(), &codex_prompt)?;

                write_message(
                    &mut output,
                    &json!({
                        "jsonrpc": "2.0",
                        "method": "session/update",
                        "params": {
                            "sessionId": session_id,
                            "update": {
                                "sessionUpdate": "agent_message_chunk",
                                "content": {
                                    "type": "text",
                                    "text": codex_output
                                }
                            }
                        }
                    }),
                )?;
                write_message(
                    &mut output,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "stopReason": "end_turn"
                        }
                    }),
                )?;
            }
            (Some(id), method) => {
                write_message(&mut output, &method_not_found(id, method)?)?;
            }
            (None, "session/cancel") => {}
            (None, _) => {}
        }
    }

    Ok(())
}

#[derive(Default)]
struct AgentState {
    session_id: Option<String>,
    cwd: Option<String>,
}

fn initialize_response(id: Value, protocol_version: u64) -> Result<Value> {
    Ok(json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "protocolVersion": protocol_version,
            "agentCapabilities": {
                "loadSession": false,
                "mcpCapabilities": {
                    "http": false,
                    "sse": false
                },
                "promptCapabilities": {
                    "audio": false,
                    "embeddedContext": true,
                    "image": false
                },
                "sessionCapabilities": {}
            },
            "agentInfo": {
                "name": "helix-codex-agent",
                "title": "Helix Codex Agent",
                "version": env!("CARGO_PKG_VERSION")
            },
            "authMethods": []
        }
    }))
}

fn method_not_found(id: Value, method: &str) -> Result<Value> {
    Ok(json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": -32601,
            "message": format!("method not found: {method}")
        }
    }))
}

fn prompt_text(params: &Value) -> String {
    params
        .get("prompt")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .filter(|prompt| !prompt.trim().is_empty())
        .unwrap_or_default()
}

fn codex_prompt(prompt: &str, meta: Option<&Value>) -> String {
    let context = meta
        .and_then(|meta| meta.get("helix"))
        .and_then(|helix| helix.get("context"));

    match context {
        Some(context) => format!(
            "You are being invoked from Helix through ACP.\n\nFor this adapter MVP, do not modify files or run write operations. If code changes are needed, describe the change or provide a patch/diff in your final answer for Helix to review later.\n\nHelix editor context JSON:\n```json\n{}\n```\n\nUser prompt:\n{}",
            serde_json::to_string_pretty(context).unwrap_or_else(|_| context.to_string()),
            prompt
        ),
        None => prompt.to_string(),
    }
}

fn run_codex_exec(cwd: Option<&str>, prompt: &str) -> Result<String> {
    let command = env::var("HELIX_CODEX_COMMAND").unwrap_or_else(|_| "codex".to_string());
    let mut child = Command::new(command);
    child
        .arg("exec")
        .arg("--color")
        .arg("never")
        .arg("--skip-git-repo-check")
        .arg("--sandbox")
        .arg("read-only")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(cwd) = cwd {
        child.arg("--cd").arg(cwd);
    }

    child.arg("-");

    let mut child = child.spawn().context("failed to spawn codex exec")?;
    child
        .stdin
        .take()
        .context("codex exec stdin is unavailable")?
        .write_all(prompt.as_bytes())
        .context("failed to write prompt to codex exec")?;

    let output = child
        .wait_with_output()
        .context("failed to read codex exec output")?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

    if output.status.success() {
        if stdout.is_empty() {
            Ok("(codex exec completed without output)".to_string())
        } else {
            Ok(stdout)
        }
    } else if stderr.is_empty() {
        Ok(format!("codex exec exited with {}", output.status))
    } else {
        Ok(format!(
            "codex exec exited with {}:\n{}",
            output.status, stderr
        ))
    }
}

fn new_session_id() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    format!("helix-codex-{millis}")
}

fn read_content_length_message(reader: &mut impl BufRead) -> Result<Option<Value>> {
    let mut line = String::new();
    let mut content_length = None;

    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }

        if line == "\r\n" {
            break;
        }

        if let Some((name, value)) = line.trim().split_once(": ") {
            if name.eq_ignore_ascii_case("Content-Length") {
                content_length = Some(value.parse::<usize>()?);
            }
        }
    }

    let content_length = content_length.context("ACP message is missing Content-Length")?;
    let mut content = vec![0; content_length];
    reader.read_exact(&mut content)?;
    Ok(Some(serde_json::from_slice(&content)?))
}

fn write_message(writer: &mut impl Write, message: &Value) -> Result<()> {
    let body = serde_json::to_string(message)?;
    write!(writer, "Content-Length: {}\r\n\r\n{}", body.len(), body)?;
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_text_prompt_blocks() {
        let params = json!({
            "prompt": [
                { "type": "text", "text": "one" },
                { "type": "text", "text": "two" }
            ]
        });

        assert_eq!(prompt_text(&params), "one\ntwo");
    }

    #[test]
    fn formats_codex_prompt_with_helix_context() {
        let meta = json!({
            "helix": {
                "context": {
                    "theme": "amberwood",
                    "mode": "normal"
                }
            }
        });

        let prompt = codex_prompt("what is open?", Some(&meta));
        assert!(prompt.contains("Helix editor context JSON"));
        assert!(prompt.contains("\"theme\": \"amberwood\""));
        assert!(prompt.contains("what is open?"));
    }

    #[test]
    fn decodes_content_length_message() {
        let mut input =
            BufReader::new(b"Content-Length: 17\r\n\r\n{\"jsonrpc\":\"2.0\"}".as_slice());
        let message = read_content_length_message(&mut input).unwrap().unwrap();
        assert_eq!(message["jsonrpc"], "2.0");
    }
}
