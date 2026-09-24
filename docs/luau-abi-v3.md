# Luau ABI v3

Tea accepts only closed-bundle `abi_version: 3` declarations. Older ABI
versions are rejected; there is no compatibility parser, migration, or fallback
that could reinterpret an immutable extension source tree.

A bundle has a manifest, declared entrypoint, and declared relative .luau
modules. The manifest's module set is closed; the runtime does not resolve
filesystem paths, packages, environment variables, network endpoints, or
ambient globals.

An ABI-v3 entrypoint returns a table with:

- required prompt_sections, an ordered array of named text sections;
- optional tools, capability-neutral tool declarations;
- optional before_tool and after_tool bounded hook declarations;
- optional context_projection;
- optional `commands`, an ordered array of bounded slash-command declarations
  (`name`, `help`, optional `allowed_while_active`, and a sandboxed `handler`);
- optional `on_idle`, a bounded callback evaluated only after a durable
  operation is terminal and its lane is idle;
- optional resume_hooks; and
- optional `state_version`, required exactly when the manifest requests
  `extension.state`.

A `state_version` without that capability is also rejected. The version is a
portable, immutable state-schema identity, not a request for host migration.

The command result can contain only a bounded notice, one whole state
replacement, and one bounded internal follow-up input. An `on_idle` result can
contain one state replacement and at most one follow-up input. The terminal
validates command names and help text, rejects duplicate command names and
native-command collisions while resolving the immutable harness, and uses the
resolved descriptions for completion and help. Command handlers have no
application handle or ambient authority.

before_tool may allow, block, terminate, or normalize through its typed bounded
result. The host validates all returned shapes, applies resource limits, and
commits lifecycle state through durable session facts. Plugins never receive raw
host handles, credentials, artifact-store access, session writes, provider
transport, or capability grants.

The canonical parser is `tea_luau::policy::parse_declaration`; the exact
manifest validation boundary is `tea_luau::bundle::BundleManifest`. Any
declaration outside its selected ABI shape is rejected.

Each tool may also declare the host-only execution policy fields
`requires_exclusive_batch` (default `false`) and
`cancellation_settlement_mode` (`drop_future`, the default, or `await_future`).
These fields control scheduling and cancellation settlement; they are not sent
to the provider and are included in the immutable host execution-policy
fingerprint instead.

## Default coding builtins

The first-party `read`, `bash`, `edit`, and `find` builtins are independent
closed v3 source trees. Each declares exactly one model-facing tool and one
distinct host capability grant. Handlers yield structured `arguments` tables;
the Luau runtime converts only finite JSON-compatible values before the request
crosses into Rust.

The host grants `tea.workspace.read.v1`, `tea.workspace.search.v1`,
`tea.workspace.mutate.v1`, and `tea.process.v1` independently. A source tree
may change its tool's descriptions, schemas, formatting, and ordinary behavior
in a future revision, but it cannot acquire another grant or select a different
workspace/process authority. Rust validates capability methods and arguments,
enforces workspace confinement and transaction safety, and retains process
lifecycle ownership.

## Bounded extension state

The optional generic `extension.state` capability is bound by the host to one
immutable extension identity and one pinned `state_version`. It exposes only:

- `get`, which returns that extension's latest complete JSON value (or `nil`
  when the namespace has never been written); and
- `replace`, whose arguments are `{ value = <JSON-compatible value> }` and
  which atomically replaces the complete value.

State is one private namespace for each `(lane, extension identity)`, not a
document store, task framework, key/value map, queue, or general persistence
surface. The canonical JSON value is at most 16,384 bytes. A command or idle
result returns a replacement as `{ state = { content_json = "<JSON>" } }`.
The host retains it as an immutable `ExtensionStateValueSetFact`; Luau receives
neither a session writer, artifact authority, filesystem path, another
extension's namespace, nor a way to patch individual keys.

At a settled-turn checkpoint, the session captures the exact version-pinned
state map. A user-facing fork inherits that snapshot and then diverges; a later
write on the source lane never leaks into the fork. The semantic leaf at the
same checkpoint carries the exact historical harness revision and configuration.

`state_version` is part of immutable bundle and harness-snapshot identity.
Candidate validation rejects changing or removing an existing stateful
extension rather than silently resetting, migrating, or reinterpreting private
state. A closed generation cannot use mutable registry state to write a new
namespace.

## Public helper example

[`crates/tea-luau/examples/run_counter`](../crates/tea-luau/examples/run_counter)
is a closed, optional session plugin. Its manifest requests only
`extension.state`. The `/run-count` command reads its lane's private count;
its `on_idle` hook increments that count only after a completed operation and
saturates at 1,000. It contributes no model-facing tools or ambient process,
filesystem, network, or provider access. Stage its two files and registry entry
through the immutable candidate flow described in
[harness self-extension](harness-self-extension.md). The
`run_counter_extension` integration test loads those exact public files and
checks the manifest, command, hook, and bounded state behavior.
