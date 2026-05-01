# DoomHelix

DoomHelix is an experimental agentic modal code editor based on
[Helix](https://helix-editor.com/). It keeps Helix's modal editing core and adds
an editor-native agent workflow for chat, explanations, patch proposals, apply
review, and streamed transcript rendering.

The editor binary is:

```sh
dhx
```

## Agent Workflow

- `:agent chat` opens a prompt-backed chat turn.
- `:agent explain` explains the current selection.
- `:agent fix` asks for a fix proposal for the current selection.
- `:agent refactor` asks for a refactor proposal.
- `:agent edit` asks for a unified diff patch proposal.
- `:agent patch` previews the latest patch.
- `:agent apply` applies the latest patch after confirmation.
- `:agent panel` opens or focuses the transcript panel.

See [docs/agent.md](docs/agent.md) for the current configuration and keymap
surface.

## Install

From a checkout:

```sh
sh install.sh
```

From a remote script:

```sh
curl -fsSL https://raw.githubusercontent.com/borngraced/doom-helix/main/install.sh | sh
```

The installer builds from source and installs:

- `dhx` to `~/.local/bin/dhx`
- `dhx-bin` to `~/.local/bin/dhx-bin`
- `codex-acp` to `~/.local/bin/codex-acp`
- runtime files to `~/.local/share/doomhelix/runtime`
- config is read from `~/.config/doomhelix/config.toml`

Override paths with `DOOMHELIX_PREFIX`, `DOOMHELIX_BIN_DIR`, or
`DOOMHELIX_RUNTIME_DIR`.
Install the legacy DoomHelix Codex adapter with `DOOMHELIX_INSTALL_LEGACY_CODEX_AGENT=1`.

## Upstream

DoomHelix is currently a fork of Helix. The original Helix project is available
at <https://github.com/helix-editor/helix>.

## License

DoomHelix keeps Helix's MPL-2.0 licensing for modified upstream source files.
See [LICENSE](LICENSE).
