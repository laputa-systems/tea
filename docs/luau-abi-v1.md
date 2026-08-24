# Luau ABI v1 and v2

Tea accepts closed-bundle `abi_version: 1` and `abi_version: 2` declarations.
Version 2 is additive at the bundle level, not a reinterpretation of v1: a v1
declaration that names a v2-only field is rejected.

A bundle has a manifest, declared entrypoint, and declared relative .luau
modules. The manifest's module set is closed; the runtime does not resolve
filesystem paths, packages, environment variables, network endpoints, or
ambient globals.

An ABI-v1 entrypoint returns a table with:

- required prompt_sections, an ordered array of named text sections;
- optional tools, capability-neutral tool declarations;
- optional before_tool and after_tool bounded hook declarations;
- optional context_projection;
- optional resume_hooks.

ABI v2 retains every v1 field and adds:

- optional `commands`, an ordered array of bounded slash-command declarations
  (`name`, `help`, optional `allowed_while_active`, and a sandboxed `handler`);
- optional `on_idle`, a bounded callback evaluated only after a durable
  operation is terminal and its lane is idle.

The v2 command result can contain only a bounded notice, one append-only
extension-local state update, and one bounded internal follow-up input. An
`on_idle` result can contain one state update and at most one follow-up input.
The terminal validates command names and help text, rejects duplicate command
names and native-command collisions while resolving the immutable harness, and
uses the resolved descriptions for completion and help. Command handlers have
no application handle or ambient authority.

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

The optional generic `extension.state` capability is bound by the host to one
immutable extension identity. It exposes only `get` of that extension's latest
local values and `append` of one bounded value. The runtime writes those values
as external-only, session-retained `PluginMemory` entries and never gives Luau a
session writer, artifact authority, filesystem path, or another extension's
state namespace.
