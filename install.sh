#!/bin/sh
set -eu

repo_url=${DOOMHELIX_REPO:-https://github.com/borngraced/doom-helix.git}
repo_ref=${DOOMHELIX_REF:-main}
release_repo=${DOOMHELIX_RELEASE_REPO:-borngraced/doom-helix}
build_from_source=${DOOMHELIX_BUILD_FROM_SOURCE:-0}
prefix=${DOOMHELIX_PREFIX:-"$HOME/.local"}
bin_dir=${DOOMHELIX_BIN_DIR:-"$prefix/bin"}
share_dir=${DOOMHELIX_SHARE_DIR:-"$prefix/share/doomhelix"}
runtime_dir=${DOOMHELIX_RUNTIME_DIR:-"$share_dir/runtime"}
install_codex_acp=${DOOMHELIX_INSTALL_CODEX_ACP:-1}
codex_acp_version=${CODEX_ACP_VERSION:-0.12.0}

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

need install
need tar

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
  tag=${DOOMHELIX_RELEASE_TAG:-}
  if [ -z "$tag" ]; then
    tag=$repo_ref
  fi

  version=${tag#v}
  asset="doom-helix-${version}-${target}.tar.gz"
  if [ "$tag" = latest ]; then
    url="https://github.com/${release_repo}/releases/latest/download/${asset}"
  else
    url="https://github.com/${release_repo}/releases/download/${tag}/${asset}"
  fi
  tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/doomhelix-release.XXXXXX")

  echo "Installing DoomHelix prebuilt release ${tag} (${target})..."
  if ! download "$url" "$tmp_dir/$asset"; then
    rm -rf "$tmp_dir"
    return 1
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
  if [ "$install_codex_acp" = 0 ]; then
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
    echo "Set DOOMHELIX_INSTALL_CODEX_ACP=0 to skip this step." >&2
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
{
  printf '%s\n' '#!/bin/sh'
  printf 'HELIX_RUNTIME=%s exec %s/dhx-bin "$@"\n' "$runtime_dir" "$bin_dir"
} >"$bin_dir/dhx"
chmod 755 "$bin_dir/dhx"

printf '%s\n' \
  'DoomHelix installed.' \
  '' \
  'Binary:' \
  "  $bin_dir/dhx" \
  "  $bin_dir/dhx-bin" \
  '' \
  'Codex ACP adapter:' \
  "  $bin_dir/codex-acp" \
  '' \
  'Runtime:' \
  "  $runtime_dir" \
  '' \
  "Make sure '$bin_dir' is on PATH, then run:" \
  '  dhx'
