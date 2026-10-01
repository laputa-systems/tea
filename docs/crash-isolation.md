# Headful crash isolation

A headful `tea` is split so that losing the terminal never cancels admitted
work.

```text
 terminal ── relay (tea) ══ private Unix socket ══ runtime (tea) ── session
            raw mode, bytes,                        App + SessionSupervisor,
            window size                             providers, tools, MCP
```

- The **runtime** is the ordinary terminal application, unchanged in
  authority and semantics. It renders into a virtual terminal whose input,
  output, and size come from the relay. It owns the durable harness (the one
  writer), providers, tools, subagents, and MCP servers.
- The **relay** is what the user starts. It puts the real terminal in raw
  mode and forwards input bytes and window size; it writes runtime output to
  the terminal. It holds no semantic state, authority, or protocol of its own:
  the presentation contract is still the application's own snapshot/update
  rendering, carried as terminal bytes.
- The runtime is started in its own process group with no terminal
  descriptors. Terminal hangup, job-control signals, and a kill of the
  terminal's process group reach only the relay. `bash` keeps its own
  per-command process groups and bounded cleanup; subagent cancellation and
  explicit shutdown are unchanged because they are runtime behavior.

Headless (`--prompt`) runs, `tea session …` commands, and Rust embeddings stay
in one process. `TEA_IN_PROCESS=1`, or a non-terminal stdin/stdout, also keeps
an interactive session in one process.

Code: `crates/tea-agent/src/detach.rs` (framing, runtime link, relay,
attachment record) and `crates/tea-agent/src/cli/headful.rs` (startup and
reattach).

## Boundary

- **One-to-one.** A runtime accepts one attached terminal at a time; a second
  `tea --attach` is refused ("already attached").
- **Session-local.** While attached to a session the runtime publishes
  `runtime.json` (pid, socket, session id) in that session's directory and
  withdraws it when it switches sessions or exits. The socket lives in a
  private `0700` directory under `/tmp` (session paths can exceed the
  platform's socket-path limit) and is never listed.
- **Exact reattachment.** `tea --attach SESSION_ID` reads that session's record
  and connects to that runtime, which confirms the session in the handshake.
  There is no broker, global listener, or enumeration of live runtimes.
- **No second agent protocol.** Frames carry handshake, input bytes, window
  size, output bytes, and close status only.

## Lifecycle

| Event | Behavior |
| --- | --- |
| Normal quit (Ctrl+C) | Unchanged application semantics: active work is cancelled and joined, then the runtime closes the relay with its exit status. |
| Terminal loss (relay killed, terminal closed, SSH drop) | The runtime detaches and keeps driving admitted work, including queued inputs. It is **not** a cancellation. |
| Detached and idle | The runtime exits (and withdraws its record) as soon as no terminal is attached and nothing is running or queued. It never lingers as a hidden service. |
| Reattach to a live runtime | `tea --attach SESSION_ID` attaches a fresh terminal; the runtime re-presents the whole view (transcript and live tail) from its state. Nothing is replayed. |
| Reattach after the runtime exited | The record is gone or stale, so `tea --attach` reopens the session (`--resume`) from durable state. A stale record (dead pid, missing socket) is detected and ignored. |
| `/new`, `/resume` | The same runtime switches its one session: the record moves to the new session directory. No root runtime accumulates. |
| Runtime failure | The relay sees the connection end without a close, restores the terminal, and reports the failure. Reopening uses tea's existing recovery: an interrupted provider request is an ambiguous effect that requires `/continue`, never an implicit replay. |
| Competing ownership | A second terminal is refused while one is attached; the durable session's single-writer lock still prevents a second runtime from executing it. |

Use `tea --resume SESSION_ID` to open a saved session directly at startup.

## Evidence

Real-binary PTY tests (`cargo test -p tea-agent --features pty-harness --test pty_isolation`):

- killing the terminal's process group mid-generation keeps the runtime alive;
  the generation settles durably with no terminal; the detached runtime exits;
  reattaching shows the turn with exactly one provider request, one input,
  and one answer;
- a terminal hangup mid-generation, live reattachment to the still-working
  runtime, refusal of a competing terminal, completion in the reattached view;
- an idle detached runtime exits promptly; a stale record falls back to
  reopening;
- killing the runtime restores the terminal, reports the failure, and leaves
  explicit recovery with no replayed request;
- `/new` moves the attachment record within the same runtime.

The existing PTY suite (`--test pty_streaming`) runs through the relay and
runtime unchanged, including byte-exact startup output and terminal-mode
restoration.
