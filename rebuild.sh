#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
cargo_bin="${CARGO_HOME:-$HOME/.cargo}/bin"

# Prefer rustup over a separate system/Homebrew Cargo installation.
if [[ -x "$cargo_bin/cargo" && -x "$cargo_bin/rustup" ]]; then
    export PATH="$cargo_bin:$PATH"
else
    echo "Rustup is required. Install it from https://rustup.rs, then rerun this script." >&2
    exit 1
fi

cd "$repo_root/codex-rs"
echo "Building CustomCodex with the toolchain pinned in rust-toolchain.toml..."
cargo build --locked -p codex-cli --bin codex --target-dir "$repo_root/codex-rs/target"

binary="$repo_root/codex-rs/target/debug/codex"
"$binary" --version
printf '\nRebuild complete. Run: %s\n' "$binary"
