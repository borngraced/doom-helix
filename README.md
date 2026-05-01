# DoomHelix

DoomHelix is an agentic modal code editor based on
[Helix](https://helix-editor.com/). It keeps Helix's fast modal editing,
tree-sitter highlighting, LSP support, selections, splits, and command model,
then adds an editor-native agent workflow for real coding work.

The binary is:

```sh
dhx
```

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
- Configurable panel position: `left`, `right`, `top`, or `bottom`.
- Configurable and keyboard-resizable panel size.
- Markdown transcript with selectable/copyable text and code fences.
- `gd` on transcript file links jumps back into the referenced source file.
- Agent transcript restore within the running editor session.
- Separate DoomHelix config path: `~/.config/doomhelix/config.toml`.

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
:agent resize +5     # grow transcript panel
:agent resize -5     # shrink transcript panel
:agent status        # show agent runtime status
```

See [docs/agent.md](docs/agent.md) for the full command, configuration, and
keymap reference.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/borngraced/doom-helix/main/install.sh | sh
```

The installer downloads a prebuilt DoomHelix release when available and falls
back to building from source. It installs:

- `dhx` to `~/.local/bin/dhx`
- `dhx-bin` to `~/.local/bin/dhx-bin`
- `codex-acp` to `~/.local/bin/codex-acp`
- runtime files to `~/.local/share/doomhelix/runtime`
- config is read from `~/.config/doomhelix/config.toml`

Override paths with `DOOMHELIX_PREFIX`, `DOOMHELIX_BIN_DIR`, or
`DOOMHELIX_RUNTIME_DIR`.

Force a local source build with:

```sh
DOOMHELIX_BUILD_FROM_SOURCE=1 sh install.sh
```

Skip installing `codex-acp` with:

```sh
DOOMHELIX_INSTALL_CODEX_ACP=0 sh install.sh
```

## Minimal Config

```toml
[editor.agent]
enable = true
default-agent = "codex"
panel-position = "right"
panel-size = 30

[editor.agent.servers.codex]
transport = "stdio"
command = "codex-acp"
args = []
```

## Agent Backend

DoomHelix talks to agents over ACP, the Agent Client Protocol used by Zed's
agent integration. The default Codex setup uses Zed's `codex-acp` adapter, which
bridges DoomHelix's ACP client to Codex's long-lived app-server backend.

## Upstream

DoomHelix is built from Helix. The original Helix project is available at
<https://github.com/helix-editor/helix>.

## License

DoomHelix keeps Helix's MPL-2.0 licensing for modified upstream source files.
See [LICENSE](LICENSE).
