# Agent Integration

This is an experimental fork-only surface for exploring editor-native coding agents in Helix.

The initial implementation is intentionally read-only. It focuses on making Helix produce a stable, structured context payload before adding subprocesses, ACP transport, edit tools, or shell execution.

## Commands

`:agent`

Opens a scratch buffer containing the current agent context snapshot as JSON.

`:agent context`

Same as `:agent`. This exists as the explicit subcommand form.

`:agent new`

Opens a new agent session scratch buffer. The session currently contains a generated session id, initial status, a system message, and the full editor context.

`:agent-context`

Compatibility command that opens the same context snapshot directly.

`:agent-new`

Compatibility command that opens the same new session buffer directly.

`:agent ask <prompt>`

Opens a scratch buffer containing a dry-run agent request payload. The payload includes:

- `schema_version`
- request `kind`
- user `prompt`
- full editor context

This does not call an external agent yet. It is a local preview of the payload that will later be sent to an ACP-compatible agent process.

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

## Near-Term Direction

The next implementation steps are:

1. Add an agent session model.
2. Spawn an external ACP-compatible subprocess.
3. Send the dry-run request payload over the transport.
4. Render responses in an agent buffer.
5. Add explicit permission gates before any write, shell, or command-execution tool.
