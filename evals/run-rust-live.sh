#!/usr/bin/env bash
set -euo pipefail

eval_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

# This is an opt-in executable integration, not a core runtime dependency. The
# pinned nightly in rust-toolchain.toml is the single source of truth; plain
# `cargo` here resolves it through the rustup shim. Do not fall back to
# stable Rust.
cd "$eval_root"
exec cargo run --quiet -p tea-core --features eval-runner --bin tea-eval -- "$@"
