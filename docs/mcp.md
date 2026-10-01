# Local MCP servers

`tea` can use tools from explicitly configured **local stdio** MCP servers.
Protocol handling, server processes, configuration, and credentials live in
the terminal host (`crates/tea-agent/src/app/mcp/`); `tea-core` only sees
ordinary trusted tools through `RuntimeServices::dynamic_tools`.

Configuration and a runnable example: [examples/mcp](../examples/mcp/README.md).

## Behavior

- **Explicit only.** Servers come from `[mcp.servers.<name>]` in
  `$TEA_HOME/config.toml`. There is no ambient discovery, remote HTTP
  transport, OAuth, or marketplace. A server inherits only a small
  environment allowlist; credentials must be passed explicitly with `env`.
- **Non-blocking startup.** Servers start in background threads when the
  session runtime first needs them. Each epoch takes a snapshot of the tools of
  ready servers, waiting at most five seconds after startup for servers still
  initializing. A slow or broken server never blocks unrelated work.
- **Deferred by default.** MCP tools are authorized but not declared; the
  host adds `tool_search` so the model loads only what it needs. Loading is a
  durable configuration entry, so resume and forks reconstruct exposure while
  the server is connected. `exposure = "direct"` declares a server's tools
  from the first request; `"composition"` makes them callable only from
  codemode.
- **Naming.** Tools are named `mcp__<server>__<tool>` (restricted to
  `[A-Za-z0-9_-]`, at most 64 bytes).
- **Schemas.** Input schemas are reduced to the vocabulary tea validates:
  local `$ref`s into `$defs`/`definitions` are inlined (cycles become
  unconstrained) and keywords tea does not enforce (`format`, `pattern`, …)
  are dropped. The server still validates its own input.
- **Results.** Text content is kept; `structuredContent` is kept in result
  details and used as the text when no text content exists; `isError` becomes
  a tool error. Images, audio, and binary resources are named with their type
  and size (`[unsupported image content omitted: image/png, 12 base64 bytes]`)
  rather than dropped silently or inlined as base64.
- **Cancellation and timeouts.** A cancelled run or an expired
  `call_timeout_seconds` sends `notifications/cancelled` for the request.
- **Lifecycle.** The runtime owns the processes, not the renderer. A crash
  fails in-flight calls with a clear error, removes the server's tools from
  the next epoch, and shows a notice once. `tea` exit closes server input,
  waits two seconds, kills what remains, and reaps it. There is no resident
  MCP supervisor and no automatic restart.
- **Protocol.** MCP `2025-06-18` over newline-delimited JSON-RPC. The client
  answers `ping`, rejects other server requests with "method not found",
  re-lists tools after `notifications/tools/list_changed`, follows
  `tools/list` pagination, and ignores (but records) non-JSON output lines.

## Verification

The fake server (`tea-mcp-fixture`, feature `mcp-fixture`) exercises text,
structured, unsupported, error, slow/cancelled, paginated, `$ref`/`format`
schemas, list changes, server pings, stray output, and crashes.

```sh
cargo test -p tea-agent --lib app::mcp                       # in-memory pipes
cargo test -p tea-agent --features mcp-fixture app::mcp      # + real processes
cargo test -p tea-agent --features mcp-fixture durable_discovery
cargo test -p tea-agent --features mcp-fixture --test mcp_fixture
```
