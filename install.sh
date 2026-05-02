#!/bin/sh
set -eu

repo_url=${DOOMHELIX_REPO:-https://github.com/borngraced/doom-helix.git}
repo_ref=${DOOMHELIX_REF:-main}
release_repo=${DOOMHELIX_RELEASE_REPO:-borngraced/doom-helix}
build_from_source=${DOOMHELIX_BUILD_FROM_SOURCE:-0}
prefix=${DOOMHELIX_PREFIX:-"$HOME/.local"}
bin_dir=${DOOMHELIX_BIN_DIR:-"$prefix/bin"}
share_dir=${DOOMHELIX_SHARE_DIR:-"$prefix/share/helix"}
runtime_dir=${DOOMHELIX_RUNTIME_DIR:-"$share_dir/runtime"}
config_dir=${DOOMHELIX_CONFIG_DIR:-"$HOME/.config/helix"}
agent_config_file=${DOOMHELIX_AGENT_CONFIG_FILE:-"$config_dir/agent.toml"}
agent_choice=${DOOMHELIX_AGENT:-}
noninteractive=${DOOMHELIX_NONINTERACTIVE:-0}
install_codex_acp=${DOOMHELIX_INSTALL_CODEX_ACP:-}
codex_acp_version=${CODEX_ACP_VERSION:-0.12.0}
claude_acp_package=${CLAUDE_ACP_PACKAGE:-@zed-industries/claude-code-acp@0.16.2}
claude_acp_command=${CLAUDE_ACP_COMMAND:-claude-code-acp}

need() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "error: required command '$1' was not found" >&2
    exit 1
  fi
}

copy_dir() {
  src=$1
  dest=$2

  rm -rf "$dest"
  mkdir -p "$dest"
  tar -C "$src" -cf - . | tar -C "$dest" -xf -
}

link_runtime_into_bin_dir() {
  linked_runtime_dir=$bin_dir/runtime

  mkdir -p "$bin_dir"

  if [ -L "$linked_runtime_dir" ]; then
    rm -f "$linked_runtime_dir"
  elif [ -e "$linked_runtime_dir" ]; then
    echo "warning: $linked_runtime_dir already exists; leaving it unchanged." >&2
    echo "DoomHelix will rely on Helix's normal runtime lookup order." >&2
    return
  fi

  ln -s "$runtime_dir" "$linked_runtime_dir"
}

need install
need tar

choose_agent() {
  if [ -n "$agent_choice" ]; then
    printf '%s\n' "$agent_choice"
    return
  fi

  if [ "$noninteractive" = 1 ]; then
    printf '%s\n' codex
    return
  fi

  if [ -r /dev/tty ]; then
    {
      printf '%s\n' \
        'Install DoomHelix agent backend?' \
        '  1) Codex (recommended)' \
        '  2) Claude' \
        '  3) Both' \
        '  4) Custom ACP / configure later'
      printf '%s' 'Choose [1]: '
    } >/dev/tty
    read answer </dev/tty
  elif [ -t 0 ]; then
    printf '%s\n' \
      'Install DoomHelix agent backend?' \
      '  1) Codex (recommended)' \
      '  2) Claude' \
      '  3) Both' \
      '  4) Custom ACP / configure later'
    printf '%s' 'Choose [1]: '
    read answer
  else
    printf '%s\n' codex
    return
  fi

  case "$answer" in
    ""|1|codex|Codex) printf '%s\n' codex ;;
    2|claude|Claude) printf '%s\n' claude ;;
    3|both|Both) printf '%s\n' both ;;
    4|custom|Custom|none|None|no|No) printf '%s\n' none ;;
    *)
      echo "error: unknown agent choice: $answer" >&2
      exit 1
      ;;
  esac
}

selected_agent=$(choose_agent)
case "$selected_agent" in
  codex|claude|both|none) ;;
  *)
    echo "error: DOOMHELIX_AGENT must be codex, claude, both, or none" >&2
    exit 1
    ;;
esac

want_codex=0
want_claude=0
case "$selected_agent" in
  codex)
    want_codex=1
    ;;
  claude)
    want_claude=1
    ;;
  both)
    want_codex=1
    want_claude=1
    ;;
esac

if [ "$install_codex_acp" = 0 ]; then
  want_codex=0
fi

download() {
  url=$1
  dest=$2

  if command -v curl >/dev/null 2>&1; then
    curl -fL --retry 3 --retry-delay 2 --connect-timeout 15 --max-time 300 "$url" -o "$dest"
  elif command -v wget >/dev/null 2>&1; then
    wget -O "$dest" "$url"
  else
    echo "error: required command 'curl' or 'wget' was not found" >&2
    exit 1
  fi
}

sh_quote() {
  printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"
}

toml_string() {
  printf '"%s"' "$(printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g')"
}

agent_config_parent() {
  dirname "$agent_config_file"
}

platform_info() {
  arch=$(uname -m)
  os=$(uname -s)

  case "$arch" in
    x86_64|amd64) arch=x86_64 ;;
    aarch64|arm64) arch=aarch64 ;;
    *)
      echo "error: unsupported architecture for codex-acp: $arch" >&2
      exit 1
      ;;
  esac

  case "$os" in
    Linux)
      platform=unknown-linux-gnu
      ext=tar.gz
      ;;
    Darwin)
      platform=apple-darwin
      ext=tar.gz
      ;;
    *)
      echo "error: unsupported OS for codex-acp: $os" >&2
      exit 1
      ;;
  esac

  printf '%s %s %s\n' "$arch" "$platform" "$ext"
}

doomhelix_target() {
  set -- $(platform_info)
  arch=$1
  platform=$2

  case "$platform" in
    unknown-linux-gnu) printf '%s\n' "${arch}-unknown-linux-gnu" ;;
    apple-darwin) printf '%s\n' "${arch}-apple-darwin" ;;
    *)
      echo "error: unsupported DoomHelix release platform: $platform" >&2
      exit 1
      ;;
  esac
}

install_prebuilt_doomhelix() {
  target=$(doomhelix_target)
  tag=${DOOMHELIX_RELEASE_TAG:-latest}

  if [ "$tag" = latest ]; then
    asset="doom-helix-*-${target}.tar.gz"
    api_url="https://api.github.com/repos/${release_repo}/releases/latest"
    tmp_meta=$(mktemp "${TMPDIR:-/tmp}/doomhelix-release.XXXXXX.json")
    if ! download "$api_url" "$tmp_meta"; then
      rm -f "$tmp_meta"
      return 1
    fi
    url=$(sed -n "s/.*\"browser_download_url\": *\"\([^\"]*doom-helix-[^\"]*-${target}\\.tar\\.gz\)\".*/\1/p" "$tmp_meta" | head -n 1)
    if [ -z "$url" ]; then
      rm -f "$tmp_meta"
      return 1
    fi
    asset=${url##*/}
    checksum_url=$(sed -n "s/.*\"browser_download_url\": *\"\([^\"]*${asset}\\.sha256\)\".*/\1/p" "$tmp_meta" | head -n 1)
    rm -f "$tmp_meta"
  else
    version=${tag#v}
    asset="doom-helix-${version}-${target}.tar.gz"
    url="https://github.com/${release_repo}/releases/download/${tag}/${asset}"
    checksum_url="https://github.com/${release_repo}/releases/download/${tag}/${asset}.sha256"
  fi
  tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/doomhelix-release.XXXXXX")

  echo "Installing DoomHelix prebuilt release ${tag} (${target})..."
  if ! download "$url" "$tmp_dir/$asset"; then
    rm -rf "$tmp_dir"
    return 1
  fi
  if [ -n "${checksum_url:-}" ] && command -v sha256sum >/dev/null 2>&1; then
    if download "$checksum_url" "$tmp_dir/$asset.sha256"; then
      (cd "$tmp_dir" && sha256sum -c "$asset.sha256")
    else
      echo "warning: release checksum unavailable; continuing without archive verification." >&2
    fi
  fi

  tar -xzf "$tmp_dir/$asset" -C "$tmp_dir"
  package_dir=$(find "$tmp_dir" -mindepth 1 -maxdepth 1 -type d | head -n 1)
  if [ -z "$package_dir" ] || [ ! -f "$package_dir/dhx-bin" ] || [ ! -d "$package_dir/runtime" ]; then
    rm -rf "$tmp_dir"
    echo "error: invalid DoomHelix release archive: $asset" >&2
    exit 1
  fi

  mkdir -p "$bin_dir" "$share_dir"
  install -m 755 "$package_dir/dhx-bin" "$bin_dir/dhx-bin"
  copy_dir "$package_dir/runtime" "$runtime_dir"
  rm -rf "$tmp_dir"
  return 0
}

install_codex_acp() {
  if [ "$want_codex" = 0 ]; then
    return
  fi

  if command -v codex-acp >/dev/null 2>&1; then
    echo "codex-acp already available on PATH; leaving it unchanged."
    return
  fi

  set -- $(platform_info)
  arch=$1
  platform=$2
  ext=$3
  asset="codex-acp-${codex_acp_version}-${arch}-${platform}.${ext}"
  url="https://github.com/zed-industries/codex-acp/releases/download/v${codex_acp_version}/${asset}"
  tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/codex-acp.XXXXXX")

  echo "Installing codex-acp v${codex_acp_version}..."
  if ! download "$url" "$tmp_dir/$asset"; then
    rm -rf "$tmp_dir"
    echo "error: failed to download codex-acp from $url" >&2
    echo "Set DOOMHELIX_AGENT=none or DOOMHELIX_INSTALL_CODEX_ACP=0 to skip this step." >&2
    exit 1
  fi

  case "$ext" in
    tar.gz) tar -xzf "$tmp_dir/$asset" -C "$tmp_dir" ;;
    *)
      rm -rf "$tmp_dir"
      echo "error: unsupported codex-acp archive extension: $ext" >&2
      exit 1
      ;;
  esac

  codex_acp_bin=$(find "$tmp_dir" -type f -name codex-acp -perm -111 | head -n 1)
  if [ -z "$codex_acp_bin" ]; then
    rm -rf "$tmp_dir"
    echo "error: codex-acp binary not found in downloaded archive" >&2
    exit 1
  fi

  install -m 755 "$codex_acp_bin" "$bin_dir/codex-acp"
  rm -rf "$tmp_dir"
}

install_claude_acp() {
  if [ "$want_claude" = 0 ]; then
    return
  fi

  if [ -x "$bin_dir/$claude_acp_command" ]; then
    echo "$claude_acp_command already installed at $bin_dir/$claude_acp_command; leaving it unchanged."
    return
  fi

  if command -v "$claude_acp_command" >/dev/null 2>&1; then
    echo "$claude_acp_command already available on PATH; leaving it unchanged."
    return
  fi

  if ! command -v npm >/dev/null 2>&1; then
    echo "warning: npm was not found; skipping Claude ACP adapter install." >&2
    echo "Install Node.js/npm, then run:" >&2
    echo "  npm install -g --prefix \"$prefix\" ${claude_acp_package}" >&2
    return
  fi

  echo "Installing Claude ACP adapter (${claude_acp_package}) under $prefix..."
  npm install -g --prefix "$prefix" "$claude_acp_package"

  if [ "$bin_dir" != "$prefix/bin" ] && [ ! -e "$bin_dir/$claude_acp_command" ] && [ -x "$prefix/bin/$claude_acp_command" ]; then
    mkdir -p "$bin_dir"
    ln -s "$prefix/bin/$claude_acp_command" "$bin_dir/$claude_acp_command"
  fi
}

write_default_config() {
  if [ "$selected_agent" = none ]; then
    return
  fi

  helix_config_file=$config_dir/config.toml
  if [ -e "$agent_config_file" ]; then
    echo "DoomHelix agent config already exists; leaving it unchanged:"
    echo "  $agent_config_file"
    if [ "$selected_agent" != none ]; then
      echo "Selected agent backend '$selected_agent' was installed, but your existing agent config was not changed."
      echo "Update name and command in agent.toml if you want to switch backends."
    fi
    return
  fi
  if [ -e "$helix_config_file" ] && grep -Eq '^\[editor\.agent\]' "$helix_config_file"; then
    echo "Helix config already contains [editor.agent]; leaving agent config unchanged:"
    echo "  $helix_config_file"
    echo "Move that section to $agent_config_file when you want a separate agent config."
    return
  fi
  mkdir -p "$(agent_config_parent)"
  default_agent=$selected_agent
  if [ "$default_agent" = both ]; then
    default_agent=codex
  fi
  agent_command=codex-acp
  if [ "$default_agent" = claude ]; then
    agent_command=$claude_acp_command
  fi

  {
    printf '%s\n' 'enable = true'
    printf 'name = %s\n' "$(toml_string "$default_agent")"
    printf 'command = %s\n' "$(toml_string "$agent_command")"
    printf '%s\n' 'args = []'
    printf '%s\n' 'panel-position = "right"'
    printf '%s\n' 'panel-size = 30'
    printf '%s\n' 'auto-context-on-open = true'
    printf '%s\n' 'include-theme = true'
    printf '%s\n' 'include-command-history = true'
    printf '%s\n' 'include-visible-buffer = true'
    printf '%s\n' 'include-diagnostics = true'
  } >"$agent_config_file"

  echo "Wrote DoomHelix agent config:"
  echo "  $agent_config_file"
}

installed_prebuilt=0
cleanup=

if [ "$build_from_source" != 1 ] && [ -z "${DOOMHELIX_SOURCE:-}" ]; then
  if install_prebuilt_doomhelix; then
    installed_prebuilt=1
  else
    echo "Prebuilt DoomHelix release unavailable; building from source." >&2
  fi
fi

if [ "$installed_prebuilt" = 0 ]; then
  need cargo

  if [ -n "${DOOMHELIX_SOURCE:-}" ]; then
    source_dir=$DOOMHELIX_SOURCE
    cleanup=
  elif [ -f Cargo.toml ] && [ -d helix-term ] && [ -d runtime ]; then
    source_dir=$(pwd)
    cleanup=
  else
    need git
    tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/doomhelix.XXXXXX")
    cleanup=$tmp_dir
    git clone --depth 1 --branch "$repo_ref" "$repo_url" "$tmp_dir"
    source_dir=$tmp_dir
  fi

  cleanup_on_exit() {
    if [ -n "${cleanup:-}" ]; then
      rm -rf "$cleanup"
    fi
  }
  trap cleanup_on_exit EXIT INT TERM

  cd "$source_dir"

  cargo build --release -p helix-term --bin dhx

  mkdir -p "$bin_dir" "$share_dir"
  install -m 755 target/release/dhx "$bin_dir/dhx-bin"
  copy_dir runtime "$runtime_dir"
fi

install_codex_acp
install_claude_acp
write_default_config
link_runtime_into_bin_dir
{
  printf '%s\n' '#!/bin/sh'
  printf '%s\n' 'if [ "$(uname -s)" = Darwin ]; then'
  printf '%s\n' '  if [ -x /usr/libexec/path_helper ]; then'
  printf '%s\n' '    eval "$(/usr/libexec/path_helper -s)"'
  printf '%s\n' '  fi'
  printf '  export PATH=%s\n' "$(sh_quote "$PATH")"
  printf '%s\n' 'fi'
  printf 'exec %s "$@"\n' "$(sh_quote "$bin_dir/dhx-bin")"
} >"$bin_dir/dhx"
chmod 755 "$bin_dir/dhx"

printf '%s\n' \
  'DoomHelix installed.' \
  '' \
  'Binary:' \
  "  $bin_dir/dhx" \
  "  $bin_dir/dhx-bin" \
  '' \
  'Managed runtime:' \
  "  $runtime_dir" \
  '' \
  'Sibling runtime link:' \
  "  $bin_dir/runtime" \
  '' \
  'Runtime lookup:' \
  '  normal Helix order (user/runtime overrides still apply)' \
  '' \
  'Config:' \
  "  $agent_config_file" \
  '' \
  'Agent backend:' \
  "  $selected_agent" \
  '' \
  "Make sure '$bin_dir' is on PATH, then run:" \
  '  dhx'
