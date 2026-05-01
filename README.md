# DoomHelix

DoomHelix is an agentic modal code editor based on
[Helix](https://helix-editor.com/). It keeps Helix's fast modal editing,
tree-sitter highlighting, LSP support, selections, splits, and command model,
then adds an editor-native agent workflow for real coding work.

The editor command is `dhx`.

## Why DoomHelix

DoomHelix is built around one idea: the agent should feel like part of the
editor, not a separate terminal chore. Chat, explain, fix, refactor, patch
review, approvals, and apply flow happen inside the editor with the current file,
selection, diagnostics, theme, mode, visible buffers, and workspace context
attached automatically.

## Highlights

- Long-lived ACP agent sessions through `codex-acp`.
- Uses the Agent Client Protocol path pioneered by Zed's agent integration.
- Streaming transcript panel inside the editor.
- Prompt UI for chat and coding actions.
- Selection-aware `explain`, `fix`, `refactor`, and `edit` commands.
- Current-file context even when the agent panel is focused.
- Command/file approval prompts for agent tool use.
- Patch preview and confirmed `git apply` flow.
- Apply failure diagnostics shown in editor buffers.
- One managed agent panel to avoid message routing confusion.
- Configurable and runtime-movable panel position: `left`, `right`, `top`, or
  `bottom`.
- Configurable and keyboard-resizable panel size with `<space>a +` and
  `<space>a -`.
- Markdown transcript with selectable/copyable text and code fences.
- `gd` on transcript file links jumps back into the referenced source file.
- Agent transcript restore within the running editor session.
- Reuses Helix config paths and keeps agent settings in `~/.config/helix/agent.toml`.

## Agent Workflow

Common commands:

```text
:agent chat          # prompt-backed chat turn
:agent explain       # explain selected code
:agent fix           # ask for a fix proposal
:agent refactor      # ask for a refactor proposal
:agent edit          # ask for a patch/diff edit
:agent patch         # preview latest patch
:agent apply         # confirm and apply latest patch
:agent panel         # open/focus transcript panel
:agent restore       # restore transcript panel
:agent position left # move transcript panel
:agent resize +5     # grow transcript panel
:agent resize -5     # shrink transcript panel
:agent status        # show agent runtime status
:agent restart       # restart the configured ACP backend
```

See [docs/agent.md](docs/agent.md) for the full command, configuration, and
keymap reference.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/borngraced/doom-helix/main/install.sh | sh
```

The installer downloads a prebuilt DoomHelix release when available and falls
back to building from source. It prompts for an agent backend:

- Codex
- Claude
- Both
- Custom ACP / configure later

For non-interactive installs, set `DOOMHELIX_AGENT`:

```sh
curl -fsSL https://raw.githubusercontent.com/borngraced/doom-helix/main/install.sh | DOOMHELIX_AGENT=claude sh
```

Supported values are `codex`, `claude`, `both`, and `none`. Use `none` when
you want to configure a custom ACP-compatible agent manually.

The installer places:

- `dhx` to `~/.local/bin/dhx`
- `dhx-bin` to `~/.local/bin/dhx-bin`
- selected ACP adapter binaries when requested
- runtime files to `~/.local/share/helix/runtime`
- starter agent config at `~/.config/helix/agent.toml` if one does not already exist

Override install paths with `DOOMHELIX_PREFIX`, `DOOMHELIX_BIN_DIR`, or
`DOOMHELIX_RUNTIME_DIR`. Override the agent config destination with
`DOOMHELIX_CONFIG_DIR` or `DOOMHELIX_AGENT_CONFIG_FILE`.

Force a local source build:

```sh
DOOMHELIX_BUILD_FROM_SOURCE=1 sh install.sh
```

Install only the editor:

```sh
DOOMHELIX_AGENT=none sh install.sh
```

## Minimal Agent Config

```toml
enable = true
name = "codex"
command = "codex-acp"
args = []
panel-position = "right"
panel-size = 30
```

Put this in `~/.config/helix/agent.toml`. Keep theme, editor settings, and
keymaps in Helix's normal `config.toml`.

## Agent Backend

DoomHelix talks to agents over ACP, the Agent Client Protocol used by Zed's
agent integration. Codex uses Zed's `codex-acp` adapter, which bridges
DoomHelix's ACP client to Codex's long-lived app-server backend. Claude uses
Zed's `@zed-industries/claude-code-acp` adapter, installed as
`claude-code-acp`. The installer installs npm-based adapters under DoomHelix's
install prefix, so the default install stays inside `~/.local`.

## Upstream

DoomHelix is built from Helix. The original Helix project is available at
<https://github.com/helix-editor/helix>.

## License

DoomHelix keeps Helix's MPL-2.0 licensing for modified upstream source files.
See [LICENSE](LICENSE).
