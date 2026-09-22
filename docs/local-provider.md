# Local provider with Apple Foundation Models

The `local` provider talks to any OpenAI-compatible Chat Completions server the
caller names. Apple's on-device model — the one behind Apple Intelligence on
macOS 26 — is not such a server by itself, so
[`tools/apple-fm-server`](../tools/apple-fm-server/README.md) is a small Python
sidecar that presents it as one. Nothing in the core depends on that sidecar;
it is how the local provider gets a real model to talk to on a Mac.

## Run it

Requirements: macOS 26+ on Apple Silicon, Apple Intelligence enabled, Xcode 26+
with its license agreed to, and [`uv`](https://docs.astral.sh/uv/).

```bash
cd tools/apple-fm-server
uv sync
uv run python -m apple_fm_server
```

It binds `127.0.0.1:8000`, which is already this provider's default base URL,
and prints the exact invocation for the model it found.

## Use it

```bash
tea -p "List the files in the workspace using the bash tool." \
    --provider local --model apple-foundation-models \
    --local-base-url http://127.0.0.1:8000/v1 --local-context-window 16384
```

or the same flags without `-p` for the terminal.

`--local-context-window` is this provider's compaction capacity, not a claim
about the server. Pass a value well above the model's real 4096-token window:
at `4096` the terminal reserves a quarter of it and compacts above the
remaining 3072 tokens, which is less than the 3258-token cost of its own
harness prompt, so it would compact as soon as it starts. The server enforces
the real window regardless and reports an honest error when a request outgrows
it.

## What to expect

| | |
| --- | --- |
| Context window | 4096 tokens for instructions, prompt, tool schemas, and the response together |
| Harness overhead | ~3.3k tokens: the terminal's system prompt plus twelve tool schemas, measured with the model's own tokenizer |
| Tool calls | Bridged, but the model executes them inside its own session; see the sidecar README |
| Speed | ~6 s to the first token, ~4 tokens/s after |

The model runs entirely on the machine and reports no usage; the token counts
in the footer are measured with the model's own tokenizer.

A turn that fits completes normally, tool calls included. A conversation that
grows past the window does not: the server rejects the request rather than
truncating it, and the terminal then has too little room left to summarize its
own history, because the summary request carries the harness prompt and every
tool schema too. Short, focused turns are the way to use this model. Use
`--dump-requests DIR` on the server to see exactly what the terminal sent.
