# Terminal host

The tea terminal is a presentation layer over one managed `SessionSupervisor`. It
does not own a transcript format, an extension registry, or a second agent
loop.

## Scrollback-native presentation

The terminal keeps the semantic typed transcript in `AppState`, but it no
longer keeps a rendered transcript viewport. The presentation projection has a
small explicit frontier:

```text
SessionSupervisor/session
        ↓
semantic AppState transcript
        ↓
presentation projection
        ├── stable prefix → native terminal scrollback
        └── mutable suffix → bounded live tail
```

Welcome, user, notice, error, settled assistant, and settled tool entries
advance the contiguous stable prefix. Their fully styled, width-aware rows are
written once to the main screen. Streaming assistant and in-progress tool rows,
activity, composer, slash completion, and footer remain in the live tail and
may be redrawn. Historical rows are not reflowed after a resize; only the live
tail is relaid out.

Normal conversation always uses the main screen, so terminal-emulator history
navigation is native. `/new` replaces the semantic session projection but does
not erase rows already written to terminal scrollback. Reopening a session
starts a fresh projection and prints its restored stable prefix again.

Help, model/custom-model/thinking/session pickers, and tool detail are true temporary
full-screen surfaces. They borrow the alternate screen only while open; closing
one restores the untouched main screen and its current live tail. Slash
completion remains inline rather than becoming a modal surface.

## Startup

The terminal accepts explicit workspace, provider, model, thinking, compaction,
and Tea-home options. It creates a fresh durable session only when it needs one
and writes the initial model, thinking, harness catalog, snapshot, and revision
before a prompt can start.

`tea --version` (or `tea -v`) prints the package version and the seven-character
Git revision captured at build time. Builds outside a checkout must provide the
validated `TEA_RELEASE_GIT_SHA` build override; the binary never substitutes an
unknown identity. New durable sessions retain both values in their immutable
header, and the session reports include the creating build identity. The
generated CLI reference is available from `tea --help`, while every command
also accepts `-h`/`--help` before required arguments are validated.

After resolving Tea home, interactive and one-shot startup load
`<tea-home>/config.toml` exactly once. A missing or empty file uses defaults;
`--tea-home` redirects this configuration together with session storage. The
strict terminal-only parser rejects symlinks, files over 256 KiB, unknown or
duplicate keys, wrong types, invalid model arrays and limits, and reports the
path plus parser source location when available. `tea session ...` commands do
not load this file (including ID-based `inspect` and `dump`), and reusable
crates never inspect it.

The terminal installs an application-owned `CodingOperations` adapter through
`TeaCodingToolsV2::with_operations`. Filesystem and search calls run on
Smol's blocking pool, while bash polls daemon-safe capture files there and
delivers bounded output updates before settlement. Cancellation kills and
reaps the shell; the core remains executor-agnostic.

A one-shot prompt uses the same durable harness boundary as an interactive
prompt:

~~~text
tea --provider <provider> --model <model> --prompt <text>
~~~

`tea --provider mock` selects the built-in safe playground model without
credentials. It returns randomized Markdown, code-block, and no-op `edit`
fixtures; its `edit` tool reports a successful preview but never reads or
changes workspace files. Each response pauses for a randomized one to ten
seconds so the terminal's thinking spinner and queued-message behavior are easy
to explore. The mock model advertises a 16k context window and returns the
terminal compactor's required structured summary for both standalone and
cache-friendly compaction requests.

## Sessions

Sessions live below the explicit Tea home, scoped by a normalized workspace
identity. Each is a v1 session directory with its colocated object store.

- /resume opens the durable session picker and reopens a selected durable session.
- /new creates a new durable session.
- /models selects the provider/model and then opens the reasoning-effort picker; the footer continuously shows context, cost, and token accounting.

The selected provider/model is sealed in each session's durable header. Opening
Tea without an explicit model starts unselected; `/resume` restores and
configures the model required by the selected session rather than consulting a
global preference.

The `/resume` picker omits the durable session currently attached to the host;
it only offers other saved sessions. Selecting a model through `/models` is a
two-step flow: choose the provider/model, then choose its reasoning effort.

Normal composer input always starts or continues the managed harness. During an
active operation, the terminal projects durable session and live harness events
into the transcript without owning their state.

## Optional subagents

Subagents are disabled unless `config.toml` contains:

```toml
[features]
subagents = true
```

`[subagents]` may select one provider family, preserve an explicit ordered
model allowlist, and set concurrency, total-spawn and timeout limits as defined
in [durable subagents](subagents.md). Without an explicit list, the terminal
uses the checked-in registry catalog for the effective provider and may append
the valid same-provider custom root model. Provider adapters are lazy and
credential lookup occurs only when that exact child model is used.

New enabled sessions persist the complete effective policy and immutable root
and child harness catalog. Reopen uses that persisted model domain; current
global configuration is an authorization ceiling, never a source of silent
expansion: a configured provider must match, and a configured model list must
include every persisted model ID. An enabled session cannot execute while the
global feature is off, though all read-only `tea session ...` operations remain
available.

Only main-lane agent events enter the root transcript. Child streaming and
intermediate output remain private until the root calls `wait_agent`. The live
footer adds `agents active/limit` only for enabled sessions and aggregates usage
and cost across lanes without committing status chatter to scrollback. Ctrl+C,
one-shot exit and terminal shutdown cascade through the supervisor and join all
active child tasks.

## Bundled goal extension

`/goal` is a bundled ABI-v2 Luau extension, not a native terminal command. Its
source is pinned in the immutable harness revision like every other extension,
so reopening a session uses the persisted source rather than the binary's
current bundled files. `/goal`, `/goal <objective>`, `/goal edit <objective>`,
`/goal pause`, `/goal resume`, and `/goal clear` operate on the extension's
external-only `goal.state.v1` `PluginMemory` value. Status reports include the
objective, status, goal-associated token use, budget when present, and active
time.

An active goal may request another ordinary durable operation only after the
previous one settles. While an operation is live, accepted goal controls are
queued; after settlement they are applied before the idle continuation decision.
This avoids mutating an in-flight provider request while ensuring a queued pause
or clear prevents the next automatic goal turn.

Composer history is scoped to the active durable session. The terminal rebuilds
it from that session's accepted user-message entries when a session is resumed;
it does not create a second history file or retain terminal commands and
unsubmitted drafts as session messages. `Up` and `Down` navigate this history
without discarding the current draft.

`Ctrl+R` opens an inline reverse-history search in the mutable tail. Type a
literal query to show up to three matching user-message excerpts above the
composer, with matching text highlighted. `Up` and `Down` select older or
newer matches, and `Enter` copies the selection into the composer without
submitting it. `Esc` or `Ctrl+C` cancels the search and restores the draft that
was present before it opened; another `Ctrl+R` advances to the next older
match.

While an operation is active, normal submitted composer input becomes one visible
local next-message slot. Later submissions append to that slot, separated by a
blank line. After the operation settles, the terminal starts the next durable
prompt with the combined text. Press `Up` with an empty composer to return that
queued text to the editor; with any editor text present, history navigation keeps
the current draft and never discards it.

## Presentation boundary

AppState, render, and the terminal decoder are local UI code. They may be
rebuilt at any time from the durable snapshot and live event subscription.
They cannot modify an operation, synthesize a completed effect, or change a
harness revision.

The fixed footer keeps the `{provider}/{model} · effort <level>` identity on
its own wrapped line, followed by calm context and usage stats. Once a durable
session exists, its identity is shown on the next line when the full opaque ID
fits the terminal width. On orderly Ctrl-C exit,
the final mutable tail is committed to native terminal scrollback so this
status remains visible after tea returns to the shell. Transient notices occupy
subsequent wrapped rows; restoration and setup errors use the error color.
A saved provider/model identity remains visible even when
the provider cannot currently be configured (for example, when its API key is
missing), while prompting still requires a configured provider.
In that configuration-error state, a normal text submission is left untouched;
use `/models` to choose or repair the provider explicitly.
