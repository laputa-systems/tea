# apple-fm-server

An OpenAI-compatible Chat Completions server for **Apple's on-device Foundation
Models**, so tea's `local` provider can talk to the model that ships with
macOS 26.

This is a sidecar, not tea itself: no cargo crate depends on it, nothing in the
`tea` build imports it, and it never becomes part of the core contract. It lives
here because it is the fastest route to exercising the local provider against a
real model that costs nothing and never leaves the machine. For the tea-side
view — flags, expectations, and limits — see
[docs/local-provider.md](../../docs/local-provider.md).

## Requirements

- macOS 26+ on Apple Silicon, with Apple Intelligence enabled.
- Xcode 26+ installed and its license agreed to (open Xcode once).
- [`uv`](https://docs.astral.sh/uv/).

## Run

```sh
cd tools/apple-fm-server
uv sync
uv run python -m apple_fm_server            # binds 127.0.0.1:8000, the port tea's local provider defaults to
```

The server prints the exact tea invocation to use. `--dump-requests DIR` writes
every request body to numbered files, which is how the token measurements below
were taken and is the first thing to reach for when a turn misbehaves;
`--verbose` logs each HTTP request.

## Point tea at it

```sh
tea --provider local \
    --model apple-foundation-models \
    --local-base-url http://127.0.0.1:8000/v1 \
    --local-context-window 16384
```

The window flag is deliberately larger than the model's real 4096-token window:
it is tea's *compaction* capacity, and 4096 makes tea compact on the first turn
and fail. See [the limits below](#what-works-and-what-the-4096-token-window-rules-out).
This server still enforces the real window and reports honestly.

Or one-shot, which is the quickest end-to-end check:

```sh
tea -p "List the files in the workspace using the bash tool." --provider local \
    --model apple-foundation-models --local-base-url http://127.0.0.1:8000/v1 \
    --local-context-window 16384
```

## What works, and what the 4096-token window rules out

Verified against tea's terminal host on an M-series Mac:

- A full agentic turn works. tea sent its harness prompt and twelve tool schemas;
  the model called `bash` (`ls`); tea validated the call against its own host
  schema, ran it, sent the result back, and the model answered from it. The
  streaming, the tool-call bridge, the usage report, and the multi-turn tool
  round trip all behaved.
- The window is the limit, and it is smaller than tea's harness. tea's default
  coding harness costs **3258 tokens** before the user says anything: a
  3293-character system prompt plus twelve tool schemas, measured with the
  model's own tokenizer. That leaves roughly 800 tokens of the 4096 for the
  actual conversation.

Two consequences, both measured rather than predicted:

- `--local-context-window 4096` cannot work. tea reserves a quarter of the
  window and compacts above the remaining three quarters — 3072 tokens — which
  the harness overhead alone already exceeds. Compaction therefore fires on the
  first turn with nothing yet to compact, and the run ends with
  `automatic compactor returned an empty checkpoint`. Pass a larger value
  (e.g. `--local-context-window 16384`) to keep compaction out of the way.
- Once a conversation genuinely exceeds 4096 tokens, this server rejects the
  request with a context-window error rather than truncating it. tea then tries
  overflow recovery, and its compaction request — which reuses the harness
  prompt *and* the tool schemas — tends to come back as a tool call instead of a
  summary, which tea reports as `compaction provider returned a tool call
  instead of a summary`. Short, focused turns are the way to use this model;
  a long coding session is not.

Both failures are properties of running tea's full harness on a 4096-token
model, not of this server: the token counts it reports are the model's own.

### Recipes that fit

Everything in this section is a request, so it is bounded by the window alone.
Measured on the same machine:

| Request | Prompt tokens | Time |
| --- | --- | --- |
| `Reply with exactly: pong` | 21 | ~4 s |
| One tool, one call-and-answer round trip | 174 → 189 | ~5 s + ~1 s |
| tea's harness prompt with all twelve tools | 3159 | tool call returned in ~5–10 s |

## What the model actually is

| Property | Value on an M-series Mac running macOS 26 |
| --- | --- |
| Context window | 4096 tokens (total: instructions + prompt + tool schemas + response) |
| Streaming | Genuine, via cumulative snapshots. ~6–7 s to the first token, then roughly one snapshot every 3 s at ~4 tokens/s. |
| Tool calls | Executed **inside** the session. See below. |
| Usage reporting | None. Token counts here are measured with the model's own tokenizer, not reported by the model. |
| Concurrency | One generation at a time. Requests queue behind a lock in the backend. |

The 4096-token window is the binding constraint. tea's default coding harness
sends a system prompt plus twelve tool schemas, and that overhead is measured
before every request; an over-budget request is rejected with a clear error
instead of being truncated silently.

## Tool calls, and the placeholder trick

Apple's framework gives a tool call to the *session*, which runs the tool and
continues generating. There is no mode that hands a proposed call back to the
caller — and no streaming path that reports one.

So each OpenAI function tool becomes an Apple tool that does nothing except
record its invocation and return a placeholder string. When the recorder sees a
call:

- generation is abandoned immediately, because everything after that point is
  text the model wrote against the placeholder;
- the recorded call is reported to the client as an OpenAI `tool_calls` entry
  with `finish_reason: "tool_calls"`.

Text the model produced *before* the first call is genuine and is forwarded.
Text produced after it is never sent.

The consequence is honest but worth stating: a turn that uses a tool streams
its preamble, waits for the model to decide and call, and then returns the call.
A turn that uses no tool streams normally.

### JSON Schema support

Apple's `GenerationSchema` is close to, but not the same as, JSON Schema.
Translated: `type` (object/string/integer/number/boolean/array), nested objects,
`required` (via an optional marker — see `_Optional` in `tools.py` for why),
`enum`, `minimum`, `maximum`, `minItems`, `maxItems`, `pattern`.

A pure `oneOf`/`anyOf` choice between object shapes is **merged** into a single
object schema: the union of the alternatives' properties, with only the fields
every alternative requires kept as required, and a description naming the
shapes. tea's `web` tool is exactly this, and dropping the choice instead left
the model with an object that declared no properties — it called `web` with `{}`,
which the host rejects. The exactly-one rule does not survive the merge, and the
warning says so.

Everything else with no Apple equivalent is dropped and named on stderr per
request: `allOf`, `not`, `const`, `minLength`, `maxLength`, `exclusiveMinimum`,
`exclusiveMaximum`, and friends. Dropping a keyword changes what the model is
*guided* to produce; it never relaxes what tea accepts, because tea re-validates
every tool call against its own authoritative schema before executing it.

`edit`'s `oneOf` is the notable casualty: it constrains which of an already
declared `edits`/`content` is required, which is not a shape, so the model sees
an edit but not the "either `edits` or `content`" rule.

## What is ignored

Request fields with no faithful Apple equivalent are accepted and ignored rather
than mistranslated, and each one is logged:

- `top_p` / `min_p` — Apple's `SamplingMode.random(top:)` is top-**k**, a
  different operation. Silently treating one as the other would be a lie.
- `chat_template_kwargs` — there is no chat template to switch.

`temperature` and `max_tokens` do map. `max_tokens` is additionally capped to
the context remaining after the measured prompt.

## Files

| Path | Role |
| --- | --- |
| `apple_fm_server/protocol.py` | OpenAI wire shapes; rendering a conversation into one Apple prompt. No SDK import. |
| `apple_fm_server/tools.py` | JSON Schema → `GenerationSchema`, and the recording stub tools. |
| `apple_fm_server/backend.py` | The only module that imports `apple_fm_sdk`. Owns sessions, streaming, usage, error mapping. |
| `apple_fm_server/server.py` | stdlib HTTP transport, SSE framing, the thread↔event-loop bridge. |
| `apple_fm_server/__main__.py` | Argument parsing, availability gate, `serve_forever`. |
| `tests/fixtures/tea-harness-tools.json` | The tool payload of one real tea request, with regeneration instructions. |

## Tests

```sh
uv run python -m pytest
```

The wire and HTTP tests run without a model; the schema tests exercise the
`apple_fm_sdk` constructors directly. The tool tests run against
`tests/fixtures/tea-harness-tools.json` — the exact twelve-tool payload one real
tea request carried — so a tool whose schema the bridge mishandles fails the
suite instead of degrading quietly against a live model.
