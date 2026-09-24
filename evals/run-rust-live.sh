#!/usr/bin/env bash
set -euo pipefail

eval_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

# This historical entry point deliberately cannot run inference. The former
# tea-eval adapter was removed with its provider policy. The only live
# verification boundary is the restricted tea-agent example.
printf '%s\n' \
  'run-rust-live.sh is retired: use cargo run -p tea-agent --example codex-luna-verification --features live-verification -- ...' \
  'No provider request was made.' >&2
exit 2
