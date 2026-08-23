# Terminal host

The tea terminal is a presentation layer over one managed SessionRuntime. It
does not own a transcript format, an extension registry, or a second agent
loop.

## Scrollback-native presentation

The terminal keeps the semantic typed transcript in `AppState`, but it no
longer keeps a rendered transcript viewport. The presentation projection has a
small explicit frontier:

```text
SessionRuntime/session
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

Help, model/custom-model/session pickers, and tool detail are true temporary
full-screen surfaces. They borrow the alternate screen only while open; closing
one restores the untouched main screen and its current live tail. Slash
completion remains inline rather than becoming a modal surface.

## Startup

The terminal accepts explicit workspace, provider, model, thinking, compaction,
and Tea-home options. It creates a fresh durable session only when it needs one
and writes the initial model, thinking, harness catalog, snapshot, and revision
before a prompt can start.

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

- /session opens the durable session picker.
- /resume reopens a selected durable session.
- /new creates a new durable session.
- /model selects the provider/model; the footer continuously shows context, cost, and token accounting.
- /thinking selects the reasoning effort for future prompts.
- /quit exits the terminal.

Normal composer input always starts or continues the managed harness. During an
active operation, the terminal projects durable session and live harness events
into the transcript without owning their state.

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
its own wrapped line, followed by calm context and usage stats. Transient notices
occupy subsequent wrapped rows; restoration and setup errors use the error color.
A saved provider/model identity remains visible even when
the provider cannot currently be configured (for example, when its API key is
missing), while prompting still requires a configured provider.
In that configuration-error state, a normal text submission is left untouched;
use `/model` to choose or repair the provider explicitly.
