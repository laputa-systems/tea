# Default coding builtins

Tea's default coding harness exposes exactly four model-facing tools, in this
order:

```text
read -> bash -> edit -> find
```

Each tool is an independent checked-in Luau builtin, in
[`crates/tea-luau/builtins/read/`](../crates/tea-luau/builtins/read/),
[`bash/`](../crates/tea-luau/builtins/bash/),
[`edit/`](../crates/tea-luau/builtins/edit/), or
[`find/`](../crates/tea-luau/builtins/find/), not a Rust `AgentTool`
implementation. Each source tree owns its tool's name, description, schema,
prompt text, scheduling metadata, ordinary argument policy, and result
formatting. The terminal resolves all four through the normal immutable
harness path, so a validated future harness revision may change a builtin's
behavior only at an epoch boundary.

Rust retains the trusted host boundary in `tea_core::coding::CodingHost`:

| Luau tool | Host capability | Trusted responsibility |
| --- | --- | --- |
| `read` | `tea.workspace.read.v1` | workspace confinement, regular-file checks, size bound, digest |
| `find` | `tea.workspace.search.v1` | confined cancellable glob traversal; 1000-result and 50 KiB output bounds |
| `edit` | `tea.workspace.mutate.v1` | complete validation, revalidation, staged transaction, rollback reporting |
| `bash` | `tea.process.v1` | fixed cwd/environment, process lifecycle, timeout, cancellation, updates |

Capability grants are selected by the host and recorded in the harness. The
host fixes each builtin's singleton tool-to-capability map, so editing Luau
cannot add an ungranted capability, remap `read` to `bash` authority, or widen
a capability's method. The host validates every yielded request independently
of the provider-facing schema. A child lane is bound to its leased workspace
before it can use these capabilities and fails closed if that binding is
absent.

A future builtin requires an explicit host catalog policy for its own plugin
identity and fixed tool grant. It may reuse an existing host capability only
when that policy explicitly selects the reuse; a new mutation or process
capability requires new authority as well.

## Unified mutation

`edit` is the only first-class workspace mutation tool. Its `files[]` entries
use exactly one of:

- `edits`: precise, unique, non-overlapping replacements on an existing file;
- `content`: complete content that creates an absent file or replaces an
  existing regular file.

All entries share one trusted transaction. Precondition failure prevents every
publication; a publication failure reports either an established rollback or an
indeterminate state that must be inspected before retrying. Parent directories
must already exist.

`write` was merged into `edit`. `grep` and `ls` were intentionally removed:
ordinary text search and directory inspection belong behind `bash`, while
`find` remains because its bounded workspace traversal is a useful optimized
primitive. Its glob pattern is limited to 4096 bytes; `*`, `**`, and `?` are
matched by a finite-state host matcher. The default and maximum result count
is 1000, and the host stops at a 50 KiB aggregate path-output budget before a
result can enter Luau. Its receipt identifies a complete result, a count
truncation, or a byte-budget truncation.

## Historical Pi capture

[`crates/tea-core/profile/default-profile.json`](../crates/tea-core/profile/default-profile.json)
are retained only as frozen Pi-parity evidence. They are not Tea's production
coding configuration and do not cause historical tools to be registered.

For verification, see the direct capability tests in
`crates/tea-core/tests/coding_capabilities.rs` and the checked-in Luau builtin
tests in `crates/tea-luau/src/builtins.rs`.
