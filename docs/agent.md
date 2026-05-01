# Agent Integration

This is an experimental fork-only surface for exploring editor-native coding agents in Helix.

The agent process is intentionally read-only. Codex can explain code and propose patches, while Helix owns the write step: it stores the latest patch proposal, lets the user inspect it, and applies it only after confirmation.

## Commands

`:agent`

Opens a scratch buffer containing the current agent context snapshot as JSON.

`:agent context`

Same as `:agent`. This exists as the explicit subcommand form.

`:agent new`

Opens a new agent session scratch buffer. The session currently contains a generated session id, initial status, a system message, and the full editor context.

`:agent acp`

Opens a dry-run ACP handshake payload as JSON. The payload currently previews the `initialize` and `session/new` messages Helix would send to an ACP-compatible agent process. Helix-specific session context is attached under ACP's `_meta` extension field.

`:agent launch-config`

Opens the resolved default agent launch config from the active editor config.

`:agent start`

Starts the configured ACP agent process and sends the initial session handshake.

`:agent status`

Reports whether an agent process is currently registered as running.

`:agent stop`

Stops the registered agent process and clears the runtime slot.

`:agent recv`

Reads one framed JSON-RPC message from the running agent process and opens it in a JSON scratch buffer. This is a low-level debug command for inspecting raw ACP frames. Run this until `:agent status` shows a real session id instead of `<pending>`; many ACP servers reply to `initialize` before replying to `session/new`.
If the agent exits before sending a response, Helix reports the process exit status and any stderr output it can capture.

`:agent prompt <text>`

Sends a `session/prompt` request to the running agent. The prompt includes a fresh Helix context snapshot under `_meta.helix.context`, so the agent sees the active file, cursor, selections, theme, mode, diagnostics, LSP servers, Git state, and recent commands at send time. After `:agent start`, run `:agent recv` until `:agent status` shows a real session id before sending prompts.

`:agent chat`

Opens an agent prompt in Helix's prompt UI. When submitted, Helix starts the configured agent if needed, sends the prompt with a fresh context snapshot, automatically reads any pending handshake messages, waits for the prompt turn to finish, and appends the agent text to the transcript buffer.

`:agent ask <text>`

Sends a direct chat turn without opening the prompt UI. This uses the same structured transcript path as `:agent chat`.

`:agent explain`

Explains the current primary selection. The selected text is included directly in the prompt with file path and line/column range.
Starts the configured agent first if needed.

`:agent fix`

Asks Codex to identify a problem in the current primary selection and propose a fix without editing files.
Starts the configured agent first if needed.

`:agent refactor`

Asks Codex to suggest a clean refactor for the current primary selection without editing files.
Starts the configured agent first if needed.

`:agent edit`

Asks Codex to return a git-apply compatible unified diff patch proposal for the current primary selection. Codex does not write files; Helix stores the patch for `:agent patch` and `:agent apply`.
Starts the configured agent first if needed.

`:agent patch`

Opens the latest stored agent patch proposal in a diff scratch buffer.

`:agent apply`

Prompts for confirmation, then applies the latest stored agent patch with `git apply --whitespace=nowarn -`.

`:agent panel`

Opens or focuses the agent transcript buffer. The split direction follows `[editor.agent].panel-position`.
There is one agent transcript per running agent runtime; new chat/action turns append to that transcript.

`:agent restore`

Restores the single agent transcript panel from the saved transcript state if the split was closed. This does not restart the agent or create a new conversation.

`:agent next`

Moves the cursor to the next turn in the agent transcript.

`:agent prev`

Moves the cursor to the previous turn in the agent transcript.

`:agent clear`

Clears the agent transcript buffer without stopping the running agent process.

`:agent-context`

Compatibility command that opens the same context snapshot directly.

`:agent-new`

Compatibility command that opens the same new session buffer directly.

## Current Context Fields

The snapshot currently includes:

- workspace root
- current working directory
- theme
- editor mode
- active file metadata
- cursor position
- visible range
- selections and selected text
- open buffers
- diagnostics
- active language servers
- Git branch and changed files
- recent `:` commands

## Configuration

The fork supports an experimental `[editor.agent]` table:

```toml
[editor.agent]
enable = true
default-agent = "codex"
panel-position = "right"
panel-size = 30
auto-context-on-open = true
include-theme = true
include-command-history = true
include-visible-buffer = true
include-diagnostics = true
require-approval-for-shell = true

[editor.agent.servers.codex]
command = "target/debug/helix-codex-agent"
args = []
```

The process-spawning layer resolves the configured `default-agent` from this table.
Agent launch commands must speak ACP over stdio using `Content-Length` framed JSON-RPC. The local Codex CLI currently available in this environment does not expose a `codex acp` subcommand; configuring `codex acp` here will print Codex help and close stdout.

`panel-position` controls where a new agent transcript split opens. Supported values are `left`, `right`, `top`, and `bottom`. `panel-size` is stored as a percentage for the intended panel size; the current split implementation opens an equal-sized split, and exact percentage sizing is reserved for a later weighted-split pass.

For Codex, build the experimental adapter first:

```sh
cargo build -p helix-codex-agent
```

Then configure Helix to launch `target/debug/helix-codex-agent`. The adapter speaks ACP to Helix and forwards prompt turns to `codex exec --skip-git-repo-check --sandbox read-only` with the current Helix context included in stdin. Set `HELIX_CODEX_COMMAND` if the Codex executable is not named `codex`.

The adapter is intentionally read-only. It should return explanations or proposed patches, not write files directly. For edits, Codex returns a unified diff; Helix previews it with `:agent patch` and applies it with `:agent apply` only after confirmation.

## Suggested Keymap

Add local bindings for the common agent actions:

```toml
[keys.normal.space.a]
c = ":agent chat"
e = ":agent explain"
f = ":agent fix"
r = ":agent refactor"
E = ":agent edit"
a = ":agent apply"
p = ":agent patch"
P = ":agent panel"
R = ":agent restore"
s = ":agent start"
x = ":agent clear"
S = ":agent status"

[keys.select.space.a]
c = ":agent chat"
e = ":agent explain"
f = ":agent fix"
r = ":agent refactor"
E = ":agent edit"
```

With this map, select code and press `<space>a e` to explain it, `<space>a f` to ask for a fix proposal, `<space>a r` to ask for a refactor proposal, or `<space>a E` to ask for a unified diff edit proposal. Use `<space>a p` to inspect the latest patch and `<space>a a` to apply it after confirmation.

## Near-Term Direction

The next implementation steps are:

1. Spawn an external ACP-compatible subprocess.
2. Send `initialize` and `session/new` JSON-RPC messages over stdio using `Content-Length` framing.
3. Render responses in an agent buffer.
4. Add explicit permission gates before any write, shell, or command-execution tool.
