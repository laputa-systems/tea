# Luau ABI v1

Tea accepts exactly one closed-bundle ABI: abi_version: 1.

A bundle has a manifest, declared entrypoint, and declared relative .luau
modules. The manifest's module set is closed; the runtime does not resolve
filesystem paths, packages, environment variables, network endpoints, or
ambient globals.

The entrypoint returns a table with:

- required prompt_sections, an ordered array of named text sections;
- optional tools, capability-neutral tool declarations;
- optional before_tool and after_tool bounded hook declarations;
- optional context_projection;
- optional resume_hooks.

before_tool may allow, block, terminate, or normalize through its typed bounded
result. The host validates all returned shapes, applies resource limits, and
commits lifecycle state through durable session facts. Plugins never receive raw
host handles, credentials, artifact-store access, session writes, provider
transport, or capability grants.

The canonical parser is tea_luau::policy::parse_declaration; the exact manifest
validation boundary is tea_luau::bundle::BundleManifest. Any declaration
outside this v1 shape is rejected.

Each tool may also declare the host-only execution policy fields
`requires_exclusive_batch` (default `false`) and
`cancellation_settlement_mode` (`drop_future`, the default, or `await_future`).
These fields control scheduling and cancellation settlement; they are not sent
to the provider and are included in the immutable host execution-policy
fingerprint instead.
