# Agent Integration

DoomHelix turns Helix into an editor-native agent workspace. The agent is not a
separate chat window: it receives structured editor context, streams into a
real buffer, asks for approval before sensitive work, and produces patches that
can be reviewed and applied without leaving the editor.

The integration relies on ACP, the Agent Client Protocol used by Zed's agent
integration. The default supported Codex path uses Zed's `codex-acp` adapter:
it speaks ACP to DoomHelix and talks to Codex through Codex's long-lived
`app-server` protocol. That gives DoomHelix one persistent backend thread
across editor turns instead of spawning a fresh `codex exec` process for every
prompt.

## Features

- Prompt-backed chat with streamed responses.
- ACP transport compatible with stdio and WebSocket agent servers.
- Default Codex support through Zed's `codex-acp` adapter.
- `explain`, `fix`, `refactor`, and `edit` commands for selected code.
- Active-file context for normal chat, even when the cursor is in the agent
  panel.
- Cursor-only selections are ignored, so a single character is not sent as
  selected code by accident.
- Selected code is included directly in the user-visible prompt with path and
  line/column range.
- Internal formatting instructions are hidden from the transcript.
- One managed transcript panel per editor runtime.
- Transcript restore after closing the panel.
- Configurable panel position and initial size.
- Keyboard panel resizing with `:agent resize`.
- Markdown transcript that remains selectable and copyable.
- `gd` on Markdown file links in the transcript opens the referenced file and
  line in a normal code split.
- Permission prompts for agent command/file requests.
- Latest patch extraction, preview, confirmation, and apply.
- Apply failures are shown with full diagnostics instead of statusline-only
  truncation.
- Buffers reload after successful agent-side edits.

## Commands

`:agent`

Opens a scratch buffer containing the current agent context snapshot as JSON.
Use this when you want to inspect exactly what DoomHelix would send as context.

`:agent context`

Same as `:agent`.

`:agent launch-config`

Opens the resolved default agent launch config from the active editor config.

`:agent start`

Starts the configured ACP agent and performs the session handshake.

`:agent status`

Shows whether an agent is stopped, running, or busy. When running, the status
includes the current ACP session id when one is known.

`:agent stop`

Stops the registered agent process and clears the runtime slot.

`:agent chat`

Opens DoomHelix's prompt UI for a chat turn. DoomHelix starts the configured
agent if needed, attaches a fresh context snapshot, appends your message to the
transcript immediately, shows a working state, and streams the response into the
agent panel.

`:agent ask <text>`

Sends a direct chat turn without opening the prompt UI.

`:agent explain`

Explains the current primary selection. The selected text is included in the
visible prompt with file path and line/column range.

`:agent fix`

Asks the agent to identify the problem in the current selection and propose a
fix.

`:agent refactor`

Asks the agent to suggest a clean refactor for the current selection.

`:agent edit`

Asks the agent to edit the current selection. The agent is allowed to request
tool permissions through ACP; DoomHelix shows an approval prompt before
granting command or file access.

`:agent patch`

Opens the latest extracted patch proposal in a diff scratch buffer.

`:agent apply`

Prompts for confirmation, then applies the latest stored patch with
`git apply --whitespace=nowarn -`. On success, the patch buffer is closed and
changed buffers are reloaded. On failure, the full apply diagnostic is shown in
an editor buffer.

`:agent panel`

Opens or focuses the single transcript panel. The split direction follows
`[editor.agent].panel-position`.

`:agent restore`

Restores the transcript panel from the in-memory transcript state if the split
was closed. This does not restart the agent or create a new conversation.

`:agent next`

Moves the cursor to the next turn in the transcript.

`:agent prev`

Moves the cursor to the previous turn in the transcript.

`:agent resize <size|+delta|-delta>`

Resizes only the agent transcript panel for the current editor session.

Examples:

```text
:agent resize 40
:agent resize +5
:agent resize -5
```

Values are clamped between 5% and 95%. Runtime resizing overrides
`panel-size` until the editor exits.

`:agent clear`

Clears the transcript buffer without stopping the running agent process.

## Transcript Panel

The transcript panel is a normal editor buffer, so selection, cursor rendering,
copying, searching, and code fences behave predictably. DoomHelix uses Markdown
source rendering rather than a transformed preview so raw buffer positions stay
aligned with the cursor and selection overlay.

DoomHelix keeps one transcript panel per editor runtime. This keeps message
routing clear: every chat/action turn appends to the same transcript instead of
creating disconnected panels.

When an agent response contains file links like:

```markdown
[services/user_interfaces.go:179](/home/user/project/services/user_interfaces.go:179)
```

pressing `gd` on the link in the transcript opens that file and line in a normal
code split.

## Context

Each agent turn receives a fresh context snapshot. Depending on config, that can
include:

- workspace root
- current working directory
- theme
- editor mode
- active file metadata
- cursor position
- visible range
- real selections and selected text
- open buffers
- diagnostics
- active language servers
- Git branch and changed files
- recent `:` commands

When the agent panel is focused, DoomHelix uses the last active real file from
that panel's view history as the coding context. This makes prompts like
`review this file` work naturally after interacting with the transcript.

## Permissions

ACP permission requests are handled inside DoomHelix. When Codex requests
command execution or file changes, `codex-acp` sends
`session/request_permission`; DoomHelix shows a `[y/N]` prompt with the request
details and sends the selected ACP permission option back to the agent.

This is why DoomHelix uses the ACP `codex-acp` adapter instead of shelling out
to `codex exec`. Plain `codex exec` is not a good editor backend for
interactive approval because it is process-oriented rather than session-oriented.

## Configuration

DoomHelix reads user config from:

```text
~/.config/doomhelix/config.toml
```

Minimal Codex config:

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
transport = "stdio"
command = "codex-acp"
args = []
```

`panel-position` supports:

```toml
panel-position = "left"
panel-position = "right"
panel-position = "top"
panel-position = "bottom"
```

`panel-size` is the initial percentage size for the transcript panel. Runtime
resizes through `:agent resize` apply only to the current editor session.

## WebSocket Transport

ACP servers can use stdio or WebSocket transport.

```toml
[editor.agent.servers.codex-ws]
transport = "websocket"
url = "ws://127.0.0.1:9000/acp"
command = "codex-acp"
args = ["--websocket", "127.0.0.1:9000"]
```

For WebSocket servers, DoomHelix starts `command` when provided, then connects
to `url`. Each WebSocket text or binary frame carries one ACP JSON-RPC message.

## Suggested Keymap

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
"+" = ":agent resize +5"
"-" = ":agent resize -5"
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

With this map:

- select code and press `<space>a e` to explain it
- select code and press `<space>a f` to ask for a fix
- select code and press `<space>a r` to ask for a refactor
- select code and press `<space>a E` to request a patch
- press `<space>a p` to inspect the latest patch
- press `<space>a a` to apply after confirmation
- press `<space>a +` or `<space>a -` to resize the agent panel

## Installation

```sh
curl -fsSL https://raw.githubusercontent.com/borngraced/doom-helix/main/install.sh | sh
```

The installer places `dhx`, `dhx-bin`, `codex-acp`, and the DoomHelix runtime
under `~/.local` by default.
