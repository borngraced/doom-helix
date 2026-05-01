use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{
    env,
    io::{self, BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    thread,
};
use tungstenite::{accept, Message, WebSocket};

const ACP_PROTOCOL_VERSION: u64 = 1;

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    if args.next().as_deref() == Some("--websocket") {
        let addr = args.next().unwrap_or_else(|| "127.0.0.1:9000".to_string());
        return run_websocket_server(&addr);
    }

    run_stdio()
}

fn run_stdio() -> Result<()> {
    let stdin = io::stdin();
    let mut input = BufReader::new(stdin.lock());
    let mut stdout = io::stdout().lock();
    let mut state = AgentState::default();

    while let Some(message) = read_content_length_message(&mut input)? {
        let mut output = ContentLengthOutput {
            writer: &mut stdout,
        };
        handle_agent_message(message, &mut state, &mut output)?;
    }

    Ok(())
}

fn run_websocket_server(addr: &str) -> Result<()> {
    let listener = TcpListener::bind(addr).with_context(|| format!("failed to bind {addr}"))?;
    eprintln!("helix-codex-agent websocket listening on ws://{addr}/acp");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                thread::spawn(move || {
                    if let Err(err) = handle_websocket_client(stream) {
                        eprintln!("helix-codex-agent websocket client error: {err:#}");
                    }
                });
            }
            Err(err) => eprintln!("helix-codex-agent websocket accept error: {err}"),
        }
    }

    Ok(())
}

fn handle_websocket_client(stream: TcpStream) -> Result<()> {
    let mut websocket = accept(stream).context("failed to accept websocket connection")?;
    let mut state = AgentState::default();

    loop {
        let message = match websocket
            .read()
            .context("failed to read websocket message")?
        {
            Message::Text(text) => serde_json::from_str(&text)?,
            Message::Binary(bytes) => serde_json::from_slice(&bytes)?,
            Message::Ping(bytes) => {
                websocket
                    .send(Message::Pong(bytes))
                    .context("failed to write websocket pong")?;
                continue;
            }
            Message::Pong(_) | Message::Frame(_) => continue,
            Message::Close(_) => break,
        };

        let mut output = WebSocketOutput {
            websocket: &mut websocket,
        };
        handle_agent_message(message, &mut state, &mut output)?;
    }

    Ok(())
}

fn handle_agent_message(
    message: Value,
    state: &mut AgentState,
    output: &mut impl AgentOutput,
) -> Result<()> {
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        return Ok(());
    };

    let id = message.get("id").cloned();
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    match (id, method) {
        (Some(id), "initialize") => {
            let protocol_version = params
                .get("protocolVersion")
                .and_then(Value::as_u64)
                .unwrap_or(ACP_PROTOCOL_VERSION);
            output.write_message(&initialize_response(id, protocol_version)?)?;
        }
        (Some(id), "session/new") => {
            let cwd = params
                .get("cwd")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .or_else(|| env::current_dir().ok().map(|cwd| cwd.display().to_string()))
                .unwrap_or_else(|| ".".to_string());
            let app_server = CodexAppServer::start(&cwd)?;
            let session_id = app_server.thread_id.clone();
            state.cwd = Some(cwd);
            state.session_id = Some(session_id.clone());
            state.app_server = Some(app_server);
            output.write_message(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "sessionId": session_id,
                    "_meta": {
                        "helixCodexAgent": {
                            "backend": "codex app-server"
                        }
                    }
                }
            }))?;
        }
        (Some(id), "session/prompt") => {
            let session_id = params
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or("helix-codex-session")
                .to_string();
            let prompt = prompt_text(&params);
            let codex_prompt = codex_prompt(&prompt, params.get("_meta"));
            let app_server = state
                .app_server
                .as_mut()
                .context("codex app-server is not running")?;
            app_server.run_turn(&codex_prompt, &session_id, output)?;
            output.write_message(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "stopReason": "end_turn"
                }
            }))?;
        }
        (Some(id), method) => {
            output.write_message(&method_not_found(id, method)?)?;
        }
        (None, "session/cancel") => {}
        (None, _) => {}
    }

    Ok(())
}

#[derive(Default)]
struct AgentState {
    session_id: Option<String>,
    cwd: Option<String>,
    app_server: Option<CodexAppServer>,
}

trait AgentOutput {
    fn write_message(&mut self, message: &Value) -> Result<()>;
    fn request_approval(&mut self, id: u64, params: Value) -> Result<Value>;
}

struct ContentLengthOutput<'a, W: Write> {
    writer: &'a mut W,
}

impl<W: Write> AgentOutput for ContentLengthOutput<'_, W> {
    fn write_message(&mut self, message: &Value) -> Result<()> {
        write_content_length_message(self.writer, message)
    }

    fn request_approval(&mut self, _id: u64, _params: Value) -> Result<Value> {
        Ok(json!({ "decision": "decline" }))
    }
}

struct WebSocketOutput<'a> {
    websocket: &'a mut WebSocket<TcpStream>,
}

impl AgentOutput for WebSocketOutput<'_> {
    fn write_message(&mut self, message: &Value) -> Result<()> {
        let body = serde_json::to_string(message)?;
        self.websocket
            .send(Message::Text(body.into()))
            .context("failed to write websocket message")?;
        Ok(())
    }

    fn request_approval(&mut self, id: u64, params: Value) -> Result<Value> {
        self.write_message(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "agent/approval",
            "params": params
        }))?;

        loop {
            let message = match self
                .websocket
                .read()
                .context("failed to read approval response")?
            {
                Message::Text(text) => serde_json::from_str::<Value>(&text)?,
                Message::Binary(bytes) => serde_json::from_slice::<Value>(&bytes)?,
                Message::Ping(bytes) => {
                    self.websocket
                        .send(Message::Pong(bytes))
                        .context("failed to write websocket pong")?;
                    continue;
                }
                Message::Pong(_) | Message::Frame(_) => continue,
                Message::Close(_) => anyhow::bail!("websocket closed while waiting for approval"),
            };

            if message.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(error) = message.get("error") {
                    anyhow::bail!("approval request failed: {error}");
                }
                return Ok(message.get("result").cloned().unwrap_or(Value::Null));
            }
        }
    }
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

struct CodexAppServer {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    thread_id: String,
    next_request_id: u64,
    next_client_request_id: u64,
}

impl Drop for CodexAppServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl CodexAppServer {
    fn start(cwd: &str) -> Result<Self> {
        let command = env::var("HELIX_CODEX_COMMAND").unwrap_or_else(|_| "codex".to_string());
        let mut child = Command::new(command)
            .arg("app-server")
            .arg("--listen")
            .arg("stdio://")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to spawn codex app-server")?;

        if let Some(stderr) = child.stderr.take() {
            thread::spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines().map_while(Result::ok) {
                    eprintln!("codex app-server: {line}");
                }
            });
        }

        let stdin = child
            .stdin
            .take()
            .context("codex app-server stdin is unavailable")?;
        let stdout = child
            .stdout
            .take()
            .context("codex app-server stdout is unavailable")?;
        let mut server = Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            thread_id: String::new(),
            next_request_id: 3,
            next_client_request_id: 10_000,
        };

        server.send_app_request(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "clientInfo": {
                    "name": "doomhelix",
                    "title": "DoomHelix",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "capabilities": {
                    "experimentalApi": true
                }
            }
        }))?;
        server.read_app_response(1)?;

        server.send_app_request(json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "thread/start",
            "params": {
                "cwd": cwd,
                "approvalPolicy": "untrusted",
                "approvalsReviewer": "user",
                "sandbox": "workspace-write",
                "experimentalRawEvents": false,
                "persistExtendedHistory": true
            }
        }))?;
        let response = server.read_app_response(2)?;
        let thread_id = response
            .get("result")
            .and_then(|result| result.get("thread"))
            .and_then(|thread| thread.get("id"))
            .and_then(Value::as_str)
            .context("codex app-server thread/start response did not include thread.id")?
            .to_string();
        server.thread_id = thread_id;
        Ok(server)
    }

    fn run_turn(
        &mut self,
        prompt: &str,
        session_id: &str,
        output: &mut impl AgentOutput,
    ) -> Result<()> {
        let request_id = self.next_request_id;
        self.next_request_id += 1;
        self.send_app_request(json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "turn/start",
            "params": {
                "threadId": self.thread_id,
                "approvalPolicy": "untrusted",
                "approvalsReviewer": "user",
                "input": [
                    {
                        "type": "text",
                        "text": prompt,
                        "text_elements": []
                    }
                ]
            }
        }))?;

        let mut turn_id = None;
        loop {
            let message = self.read_app_message()?;
            if self.handle_app_server_request(&message, session_id, output)? {
                continue;
            }

            if message.get("id").and_then(Value::as_u64) == Some(request_id) {
                turn_id = message
                    .get("result")
                    .and_then(|result| result.get("turn"))
                    .and_then(|turn| turn.get("id"))
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                continue;
            }

            match message.get("method").and_then(Value::as_str) {
                Some("turn/started") => {
                    turn_id = message
                        .get("params")
                        .and_then(|params| params.get("turn"))
                        .and_then(|turn| turn.get("id"))
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);
                    write_agent_status(output, session_id, "Thinking...")?;
                }
                Some("item/agentMessage/delta") => {
                    if let Some(delta) = message
                        .get("params")
                        .and_then(|params| params.get("delta"))
                        .and_then(Value::as_str)
                    {
                        write_agent_message_chunk(output, session_id, delta)?;
                    }
                }
                Some("item/commandExecution/outputDelta") => {
                    if let Some(delta) = message
                        .get("params")
                        .and_then(|params| params.get("delta"))
                        .and_then(Value::as_str)
                    {
                        write_agent_status(
                            output,
                            session_id,
                            &format!("Command output: {delta}"),
                        )?;
                    }
                }
                Some("turn/completed") => {
                    let completed_turn = message
                        .get("params")
                        .and_then(|params| params.get("turn"))
                        .and_then(|turn| turn.get("id"))
                        .and_then(Value::as_str);
                    if turn_id.as_deref().is_none() || completed_turn == turn_id.as_deref() {
                        write_agent_status(output, session_id, "Done")?;
                        return Ok(());
                    }
                }
                Some("error") => {
                    let text = message
                        .get("params")
                        .and_then(|params| params.get("error"))
                        .map(|error| error.to_string())
                        .unwrap_or_else(|| "codex app-server error".to_string());
                    write_agent_message_chunk(
                        output,
                        session_id,
                        &format!("Agent turn failed:\n\n```text\n{text}\n```"),
                    )?;
                }
                Some("item/started") => {
                    if let Some(item_type) = message
                        .get("params")
                        .and_then(|params| params.get("item"))
                        .and_then(|item| item.get("type"))
                        .and_then(Value::as_str)
                    {
                        write_agent_status(output, session_id, &format!("Codex: {item_type}"))?;
                    }
                }
                _ => {}
            }
        }
    }

    fn handle_app_server_request(
        &mut self,
        message: &Value,
        session_id: &str,
        output: &mut impl AgentOutput,
    ) -> Result<bool> {
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            return Ok(false);
        };
        let Some(id) = message.get("id").cloned() else {
            return Ok(false);
        };

        let Some(approval) = app_server_approval_prompt(method, message.get("params")) else {
            let response = json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32601,
                    "message": format!("unsupported codex app-server request: {method}")
                }
            });
            self.send_app_request(response)?;
            return Ok(true);
        };

        write_agent_status(
            output,
            session_id,
            &format!("Approval required: {}", approval.title),
        )?;
        let client_request_id = self.next_client_request_id;
        self.next_client_request_id += 1;
        let client_result = output.request_approval(
            client_request_id,
            json!({
                "title": approval.title,
                "body": approval.body,
                "kind": approval.kind,
            }),
        )?;
        let accepted = client_result
            .get("decision")
            .and_then(Value::as_str)
            .is_some_and(|decision| decision == "accept");
        let response = match method {
            "item/commandExecution/requestApproval" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "decision": if accepted { "accept" } else { "decline" }
                }
            }),
            "item/fileChange/requestApproval" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "decision": if accepted { "accept" } else { "decline" }
                }
            }),
            "applyPatchApproval" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "decision": if accepted { "approved" } else { "denied" }
                }
            }),
            "execCommandApproval" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "decision": if accepted { "approved" } else { "denied" }
                }
            }),
            "item/permissions/requestApproval" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": if accepted {
                    {
                        let permissions = message
                            .get("params")
                            .and_then(|params| params.get("permissions"))
                            .cloned()
                            .unwrap_or_else(|| json!({}));
                        json!({
                            "permissions": permissions,
                            "scope": "turn",
                            "strictAutoReview": true
                        })
                    }
                } else {
                    json!({
                        "permissions": {},
                        "scope": "turn",
                        "strictAutoReview": true
                    })
                }
            }),
            _ => unreachable!(),
        };
        self.send_app_request(response)?;
        Ok(true)
    }

    fn handle_app_server_request_declined(&mut self, message: &Value) -> Result<bool> {
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            return Ok(false);
        };
        let Some(id) = message.get("id").cloned() else {
            return Ok(false);
        };
        let Some(_) = app_server_approval_prompt(method, message.get("params")) else {
            return Ok(false);
        };
        let response = match method {
            "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
                json!({ "jsonrpc": "2.0", "id": id, "result": { "decision": "decline" } })
            }
            "item/permissions/requestApproval" => {
                json!({ "jsonrpc": "2.0", "id": id, "result": { "permissions": {}, "scope": "turn", "strictAutoReview": true } })
            }
            "applyPatchApproval" | "execCommandApproval" => {
                json!({ "jsonrpc": "2.0", "id": id, "result": { "decision": "denied" } })
            }
            _ => unreachable!(),
        };
        self.send_app_request(response)?;
        Ok(true)
    }

    fn send_app_request(&mut self, message: Value) -> Result<()> {
        serde_json::to_writer(&mut self.stdin, &message)?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        Ok(())
    }

    fn read_app_response(&mut self, id: u64) -> Result<Value> {
        loop {
            let message = self.read_app_message()?;
            if self.handle_app_server_request_declined(&message)? {
                continue;
            }
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(error) = message.get("error") {
                    anyhow::bail!("codex app-server request {id} failed: {error}");
                }
                return Ok(message);
            }
        }
    }

    fn read_app_message(&mut self) -> Result<Value> {
        let mut line = String::new();
        let bytes = self
            .stdout
            .read_line(&mut line)
            .context("failed to read codex app-server message")?;
        if bytes == 0 {
            anyhow::bail!("codex app-server closed while reading message");
        }
        Ok(serde_json::from_str(line.trim())?)
    }
}

struct AppServerApprovalPrompt {
    kind: &'static str,
    title: String,
    body: String,
}

fn app_server_approval_prompt(
    method: &str,
    params: Option<&Value>,
) -> Option<AppServerApprovalPrompt> {
    let params = params.unwrap_or(&Value::Null);
    match method {
        "item/commandExecution/requestApproval" => {
            let command = params
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("unknown command");
            let cwd = params.get("cwd").and_then(Value::as_str).unwrap_or("");
            let reason = params.get("reason").and_then(Value::as_str).unwrap_or("");
            Some(AppServerApprovalPrompt {
                kind: "command",
                title: "Run command?".to_string(),
                body: format_approval_body(command, cwd, reason),
            })
        }
        "execCommandApproval" => {
            let command = params
                .get("command")
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .filter(|command| !command.is_empty())
                .unwrap_or_else(|| "unknown command".to_string());
            let cwd = params.get("cwd").and_then(Value::as_str).unwrap_or("");
            let reason = params.get("reason").and_then(Value::as_str).unwrap_or("");
            Some(AppServerApprovalPrompt {
                kind: "command",
                title: "Run command?".to_string(),
                body: format_approval_body(&command, cwd, reason),
            })
        }
        "item/fileChange/requestApproval" => {
            let root = params
                .get("grantRoot")
                .and_then(Value::as_str)
                .unwrap_or("");
            let reason = params.get("reason").and_then(Value::as_str).unwrap_or("");
            Some(AppServerApprovalPrompt {
                kind: "file_change",
                title: "Allow file changes?".to_string(),
                body: format_approval_body("file write access", root, reason),
            })
        }
        "item/permissions/requestApproval" => {
            let cwd = params.get("cwd").and_then(Value::as_str).unwrap_or("");
            let reason = params.get("reason").and_then(Value::as_str).unwrap_or("");
            let permissions = params
                .get("permissions")
                .map(|permissions| {
                    serde_json::to_string_pretty(permissions)
                        .unwrap_or_else(|_| permissions.to_string())
                })
                .unwrap_or_default();
            Some(AppServerApprovalPrompt {
                kind: "permissions",
                title: "Grant agent permissions?".to_string(),
                body: format_approval_body(&permissions, cwd, reason),
            })
        }
        "applyPatchApproval" => {
            let reason = params.get("reason").and_then(Value::as_str).unwrap_or("");
            let changes = params
                .get("fileChanges")
                .and_then(Value::as_object)
                .map(|changes| changes.keys().cloned().collect::<Vec<_>>().join("\n"))
                .unwrap_or_default();
            Some(AppServerApprovalPrompt {
                kind: "file_change",
                title: "Apply patch?".to_string(),
                body: format_approval_body("patch", &changes, reason),
            })
        }
        _ => None,
    }
}

fn format_approval_body(action: &str, cwd: &str, reason: &str) -> String {
    let mut body = format!("Action:\n{action}");
    if !cwd.is_empty() {
        body.push_str(&format!("\n\nTarget:\n{cwd}"));
    }
    if !reason.is_empty() {
        body.push_str(&format!("\n\nReason:\n{reason}"));
    }
    body
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
            "You are being invoked from DoomHelix through ACP.\n\nDoomHelix editor context JSON:\n```json\n{}\n```\n\nUser prompt:\n{}",
            serde_json::to_string_pretty(context).unwrap_or_else(|_| context.to_string()),
            prompt
        ),
        None => prompt.to_string(),
    }
}

fn write_agent_status(output: &mut impl AgentOutput, session_id: &str, text: &str) -> Result<()> {
    output.write_message(&json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": session_id,
            "update": {
                "sessionUpdate": "agent_status",
                "content": {
                    "type": "text",
                    "text": text
                }
            }
        }
    }))
}

fn write_agent_message_chunk(
    output: &mut impl AgentOutput,
    session_id: &str,
    text: &str,
) -> Result<()> {
    output.write_message(&json!({
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
    }))
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

fn write_content_length_message(writer: &mut impl Write, message: &Value) -> Result<()> {
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
