use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{
    env,
    io::{self, BufRead, BufReader, Read, Write},
    process::{Command, Stdio},
    thread,
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
                run_codex_exec_stream(
                    state.cwd.as_deref(),
                    &codex_prompt,
                    &session_id,
                    &mut output,
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
            "You are being invoked from Helix through ACP.\n\nDo not modify files or run write operations. If code changes are requested, return a git-apply compatible unified diff in your final answer. Helix will let the user inspect and explicitly apply that patch.\n\nHelix editor context JSON:\n```json\n{}\n```\n\nUser prompt:\n{}",
            serde_json::to_string_pretty(context).unwrap_or_else(|_| context.to_string()),
            prompt
        ),
        None => prompt.to_string(),
    }
}

fn run_codex_exec_stream(
    cwd: Option<&str>,
    prompt: &str,
    session_id: &str,
    output: &mut impl Write,
) -> Result<()> {
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
    let mut stdin = child
        .stdin
        .take()
        .context("codex exec stdin is unavailable")?;
    stdin
        .write_all(prompt.as_bytes())
        .context("failed to write prompt to codex exec")?;
    drop(stdin);

    let stdout = child
        .stdout
        .take()
        .context("codex exec stdout is unavailable")?;
    let stderr = child
        .stderr
        .take()
        .context("codex exec stderr is unavailable")?;
    let stderr_reader = thread::spawn(move || {
        let mut stderr = stderr;
        let mut buffer = String::new();
        stderr.read_to_string(&mut buffer).map(|_| buffer)
    });

    let mut saw_stdout = false;
    let mut stdout = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        let bytes = stdout
            .read_line(&mut line)
            .context("failed to read codex exec stdout")?;
        if bytes == 0 {
            break;
        }

        saw_stdout = true;
        write_agent_message_chunk(output, session_id, &line)?;
    }

    let status = child.wait().context("failed to read codex exec output")?;
    let stderr = stderr_reader
        .join()
        .unwrap_or_else(|_| Ok("failed to join codex stderr reader".to_string()))?
        .trim()
        .to_string();

    if status.success() {
        if !saw_stdout {
            write_agent_message_chunk(output, session_id, "(codex exec completed without output)")?;
        }
        return Ok(());
    }

    let message = if stderr.is_empty() {
        format!("codex exec exited with {status}")
    } else {
        format!("codex exec exited with {status}:\n{stderr}")
    };
    write_agent_message_chunk(output, session_id, &message)?;

    Ok(())
}

fn write_agent_message_chunk(writer: &mut impl Write, session_id: &str, text: &str) -> Result<()> {
    write_message(
        writer,
        &json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {
                        "type": "text",
                        "text": text
                    }
                }
            }
        }),
    )
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
