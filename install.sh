#!/bin/sh
set -eu

repo_url=${DOOMHELIX_REPO:-https://github.com/borngraced/doom-helix.git}
repo_ref=${DOOMHELIX_REF:-main}
prefix=${DOOMHELIX_PREFIX:-"$HOME/.local"}
bin_dir=${DOOMHELIX_BIN_DIR:-"$prefix/bin"}
share_dir=${DOOMHELIX_SHARE_DIR:-"$prefix/share/doomhelix"}
runtime_dir=${DOOMHELIX_RUNTIME_DIR:-"$share_dir/runtime"}

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

need cargo
need install
need tar

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
cargo build --release -p helix-codex-agent

mkdir -p "$bin_dir" "$share_dir"
install -m 755 target/release/dhx "$bin_dir/dhx-bin"
install -m 755 target/release/helix-codex-agent "$bin_dir/doomhelix-codex-agent"
copy_dir runtime "$runtime_dir"
cat >"$bin_dir/dhx" <<EOF
#!/bin/sh
HELIX_RUNTIME=${runtime_dir} exec ${bin_dir}/dhx-bin "\$@"
EOF
chmod 755 "$bin_dir/dhx"

cat <<EOF
DoomHelix installed.

Binary:
  $bin_dir/dhx
  $bin_dir/dhx-bin

Codex adapter:
  $bin_dir/doomhelix-codex-agent

Runtime:
  $runtime_dir

Make sure '$bin_dir' is on PATH, then run:
  dhx
EOF
