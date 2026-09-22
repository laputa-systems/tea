#!/usr/bin/env bash
# Guard the Rust toolchain single source of truth.
#
# rust-toolchain.toml is the only source file that may name a pinned nightly
# version. Every other entry point resolves the pin through the rustup shim
# (plain `cargo` under the repository root; the Dockerfile derives the
# channel from the toml for its install step) or reads it dynamically via
# evals/quality/toolchain.py (Python). A toolchain bump must therefore touch
# exactly one file.
#
# Historical run artifacts (e.g. git-ignored evidence under evals/results/)
# keep the toolchain that produced them and are out of scope for this check.
set -euo pipefail

script_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)

# NOTE: this pattern intentionally also matches this file's own grep below,
# so the file itself is excluded from the search.
pattern='nightly-[0-9]{4}-[0-9]{2}-[0-9]{2}'

if git -C "$repo_root" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  # Tracked source only; ignored local artifacts are not source of truth.
  violations=$(git -C "$repo_root" grep -nE "$pattern" -- . \
    ':!rust-toolchain.toml' \
    ':!scripts/check-toolchain-pin.sh' || true)
else
  matches=$(grep -rEn "$pattern" \
    --exclude-dir=.git \
    --exclude-dir=target \
    --exclude-dir=__pycache__ \
    --exclude-dir=.pytest_cache \
    --exclude-dir=results \
    --exclude=Cargo.lock \
    --exclude=check-toolchain-pin.sh \
    "$repo_root" || true)
  violations=""
  if [[ -n "$matches" ]]; then
    violations=$(printf '%s\n' "$matches" | grep -v 'rust-toolchain\.toml:' || true)
  fi
fi

if [[ -n "$violations" ]]; then
  echo "toolchain pin check failed: pinned nightly appears outside rust-toolchain.toml:" >&2
  printf '%s\n' "$violations" >&2
  echo "Resolve the pin via the rustup shim or read it dynamically instead." >&2
  exit 1
fi

echo "toolchain pin check passed"
