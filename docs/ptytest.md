# PTY integration tests

`crates/tea-agent/tests/pty_streaming.rs` drives the compiled TUI through
`ptytest`; the streaming case uses a caller-selected local OpenAI-compatible
loopback fixture, so it never contacts or names an external provider. Run it
with `cargo test -p tea-agent --features pty-harness --test pty_streaming`.

The test uses a hermetic environment, an audited `xterm-minimal-v1` profile,
and semantic screen barriers for startup, the first streamed token, completion,
and exit. Settled checks pin the startup welcome/composer/status rows, cursor,
composer emphasis, inline slash-menu geometry, help-surface entry/exit, model
picker behavior, multiline paste, history, resize, and terminal restoration.
Keep timing control in the network fixture or a named domain event; do not
introduce reader-thread or settle-delay synchronization.

Failure bundles are under `target/ptytest-failures/` and retain exact PTY bytes
plus redacted text configuration. Future semantic snapshots live beside the
owning test and update only through `PTYTEST_UPDATE_SNAPSHOTS=1`.
