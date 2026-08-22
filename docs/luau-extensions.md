# Writing Luau extensions

`tea-luau` is an optional, hermetic policy plane for
`tea`. It is for task- and world-specific policy, not a second
agent runtime: Rust retains control of model transport, the state machine,
tool scheduler, cancellations, tracing, resource ownership, and every side
effect. That includes schema validation, preparation, scheduling, updates,
ordering, result insertion, cancellation, and settlement for adapted tools; a
handler's single coroutine is a structural limit, not a replacement scheduler.

Use the checked-in nightly. The embedded engine is `mlua`'s `luau-jit`
backend; it is intentionally not LuaJIT 5.2. An embedding normally drives its
agent and any Luau futures using Smol. Tokio is unsupported.

## What a policy can do

A policy source or bundle entrypoint returns a declaration table. It can:

- append task-specific text to a host-owned system prompt;
- describe model-visible tools and their JSON schemas;
- allow, block, or terminate before a model tool call; and
- provide coroutine handler source for a host to adapt to an ordinary Rust
  `AgentTool`.

A policy cannot discover files, read environment variables, run processes,
open a network connection, load packages, use a wall clock, modify core
state, schedule an agent, or acquire a capability by naming it.

The shipped Luau surface is intentionally low-level. Ergonomic `@world`-style
modules remain host API designs and must use the same explicit yield and gate
boundary. Luau syntax, including annotations, is compiled by the embedded VM;
this repository does not provide a separate `luau-analyze` dependency or an
external static-type-check guarantee.

## Minimal declaration

```luau
return {
    system_prompt_append = [[
Inspect the world before acting. Keep world calls narrow and deliberate.
]],

    tools = {
        {
            name = "inspect_world",
            description = "Read a small host-provided world snapshot.",
            capability = "world",
            execution_mode = "sequential",
            schema_json = [[{"type":"object","additionalProperties":false}]],
        },
    },

    before_tool_call = function(call)
        if call.name == "inspect_world" then
            return "allow"
        end
        return { action = "block", reason = "this policy did not grant that tool" }
    end,
}
```

`system_prompt_append` is required. `tools` and `before_tool_call` are
optional. Each tool needs a unique non-empty `name`, `description`,
`capability`, `schema_json`, and `execution_mode` (`"sequential"` or
`"parallel"`). `schema_json` is parsed and validated by Rust before a model
can call the tool.

`before_tool_call` receives opaque `id`, model-facing `name`, and exact
`arguments_json`. It must return one of:

```luau
"allow"
{ action = "block", reason = "model-actionable explanation" }
{ action = "terminate", reason = "explain why the run must stop" }
```

The decision is made before the host hook and tool implementation. It cannot
rewrite arguments, fabricate a result, or invoke an effect directly.

## Write a capability-backed tool handler

Add `handler_source` when the model-facing tool should invoke an explicit host
capability. It is a string whose value evaluates to a function. The function
runs in a fresh VM for each tool invocation and receives:

```luau
{ id = "opaque-call-id", name = "tool-name", arguments_json = "{...}" }
```

The only suspension protocol is a yielded capability request:

```luau
local world_handler = [[
return function(call)
    local result = coroutine.yield({
        kind = "capability",
        capability = "world",
        method = "inspect",
        arguments_json = call.arguments_json,
    })
    return {
        content = result.content,
        details_json = result.details_json,
        is_error = result.is_error,
    }
end
]]

return {
    system_prompt_append = "",
    tools = {
        {
            name = "inspect_world",
            description = "Read a small host-provided world snapshot.",
            capability = "world",
            execution_mode = "sequential",
            schema_json = [[{"type":"object","additionalProperties":false}]],
            handler_source = world_handler,
        },
    },
}
```

The Rust embedding must construct `LuaToolHandler` with matching
`ToolHandlerSpec` and an explicit `CapabilityBindings` entry. The handler
rejects a yielded capability other than the declared one. The capability
implementation must validate `method` and parsed JSON itself; a shared
capability should additionally bind it to the outer model-visible tool name.
MCP capability manifests can scope that binding to exact server, method, and
target triples.

On success, return either a string or a result table containing `content` and
optional `details_json`, `is_error`, and `terminate`. `details_json` must be
valid JSON. A handler may make at most `HandlerLimits::max_capability_calls`
host calls (64 by default). Cancellation wakes a pending capability call,
drops its host future before settlement, and returns a typed cancellation.

## Bundle-local modules

For a multi-file policy, build `bundle::Bundle` in the embedding from explicit
source records and call `LuaPolicy::load_bundle`. There is deliberately no
filesystem bundle loader in the crate.

```luau
-- main.luau
local prompt = require("./parts/prompt.luau")
return { system_prompt_append = prompt }
```

Only `./...` and `../...` imports are accepted, and they must stay inside the
declared bundle. Bare names, absolute paths, drive paths, package registries,
and virtual modules are denied. Each VM has its own module cache. `Bundle` is
an ABI-v1 value whose deterministic source hash covers its manifest and every
canonical module; that hash is an identity, not a cryptographic digest.

## Host capability manifests

`capability::CapabilityManifest` is the host-facing, serializable ABI-v1
authority description. Its typed modules are `@agent`, `@world`, `@trace`,
`@task`, `@json`, and `@time`; an MCP permission can be scoped to an exact
server, method, and tool/resource target. Matching is exact; omitting a target
is not a wildcard. Use `CapabilityGate` before an effectful provider. A
manifest does not install globals or effects into Luau; the embedding still
chooses a concrete `LuauCapability` binding.

This separation is intentional. Do not invent `require("@world")` or other
ambient capability modules in a policy unless its embedding documents and
installs that exact interface. The baseline bundle loader rejects it.

## Async work and cancellation

`async_runtime` is available to an embedding that needs a generic Luau
coroutine outside the normal tool scheduler. It installs
`await(capability, arguments_json)` and returns a caller-polled `LuauTask`.
The host's `HostAwaiter` owns the future and uses the supplied
`CancellationToken`; cancellation drops a pending host future and settles the
task as a typed cancellation. It neither starts an executor nor spawns a
thread.

Tool handlers already use the core scheduler and should normally be preferred
for model-visible effects.

## Tea minimal extension host

The minimal Tea host is an embedding convention around this crate, not an
ambient resource layer in `tea-luau`. Tea may own a user-facing
extension registry, but Rust still receives explicit source records, a closed
bundle, and explicit capability bindings.

### Host-only `~/.tea` ownership

`~/.tea` belongs to the Tea host if that host elects to use it. The core,
`tea-luau`, and a policy VM must never discover, read, write, watch, or
interpret that directory. A host may choose a different root, an in-memory
registry, or no persistent registry at all. Path permissions, file formats,
symlink handling, atomic writes, and user approval are host responsibilities.

Reading a file from `~/.tea` is therefore an input step performed by Tea. The
host converts the selected bytes into an explicit source registry before
calling the bundle API; it does not pass a path or a promise of ambient
discovery across the boundary.

### Explicit source registry and order

Before evaluating extensions, Tea constructs an ordered source registry. Each
record has a stable extension/source identity, a canonical module path, and
the exact source bytes (plus the entry module and any host trust metadata
needed by the host). The registry order is part of the input and must be
stable across runs. Duplicate identities or module paths, an absent entry
module, and an order that cannot be reproduced are load errors.

There is no directory walk, implicit alphabetical order, search-path fallback,
or last-writer-wins replacement. Adding, removing, or reordering an extension
is an explicit host operation. The registry is also the source of composition
order and must be included in any host-level snapshot or review record.

Each registry entry is converted to a closed `bundle::Bundle` from those
records. The loader accepts only `./...` and `../...` imports that resolve to a
declared module inside that entry's bundle. Bare package names, absolute or
drive paths, undeclared virtual modules, missing modules, and cycles are
rejected. A fresh VM receives a fresh module cache. The loader never consults
`~/.tea`, the current directory, environment variables, a package registry,
the network, or the host filesystem. Bundle source hashes are deterministic
content identities; they are not signatures or cryptographic trust proofs.

### Extension composition

Composition happens in the host after each extension has independently passed
bundle, declaration, and resource-limit validation. It is deterministic and
has no implicit authority transfer:

1. `system_prompt_append` values are concatenated in source-registry order.
   The host supplies the separator and must preserve the resulting order.
2. Tool names are a single model-facing namespace. A duplicate name is a
   composition error; an extension cannot replace or silently wrap another
   extension's declaration or handler.
3. A missing `before_tool_call` hook abstains. Present hooks run in registry
   order; the first `terminate` or `block` result wins. A call is allowed only
   when no hook blocks or terminates it.
4. A handler remains owned by the tool declaration that supplied it and runs
   with that declaration's fresh VM and limits. Extensions do not share Lua
   globals, module caches, coroutines, or mutable declaration tables.

The host must reject an invalid composed declaration before exposing it to a
model. Composition is not a second policy language: it cannot rewrite core
state, scheduler order, cancellation, tool results, or event settlement.

### Zero default effect authority

The default Tea host starts with zero effect authority. A declaration, prompt
suffix, tool schema, handler source, capability-shaped yield, or capability
manifest entry is data and never an effect. Authority has two separate parts:

- a `CapabilityManifest` grants an exact operation to a selected policy; and
- a `CapabilityBindings` entry supplies the Rust implementation that can carry
  out that operation.

Both are required. A manifest without a binding is inert, and a binding that
is not explicitly granted is unreachable. Composition does not union grants,
inherit authority from another extension, or turn a model-visible tool into a
capability. Tea must choose and install each grant and binding explicitly;
otherwise the request fails closed. Credentials and other effectful state
remain in the host and never enter source text or the policy VM.

### Trusted dynamic extensions: handbook and authoring tool

“Dynamic” means that Tea selected new source records at a host-defined
boundary; it does not mean that an extension can install itself or approve its
own authority. A Tea distribution that supports trusted dynamic extensions
should provide a host handbook and an authoring tool. The handbook should
define the registry record, canonical module and bundle rules, composition
order, resource budgets, grant review, source identity, reload lifecycle, and
the distinction between trusted host diagnostics and model-visible data.

The authoring tool may scaffold a closed bundle, validate imports and
declarations, canonicalize the registry order, calculate a review identity,
and show the exact capability-grant diff. It may write host-owned state only
after an explicit operator action. It must not silently install, execute,
approve, or grant an extension. Neither the handbook nor the tool expands the
crate ABI or creates a package marketplace, remote loader, or ambient module.

If a host binds grants to source, it must treat the current bundle source hash
as an identity/checkpoint only: the implementation deliberately uses a
non-cryptographic deterministic hash. A source-sensitive trust decision needs
an independently defined cryptographic digest of the canonical source,
manifest, entry module, and registry order, plus an explicit trust record.
Changing any of those inputs must invalidate or re-review the grant; matching
an extension name or path is not sufficient.

### Idle-only immutable reload snapshots

Reload is an idle-only host transaction. `Idle` means that the Agent has no
active run and its terminal observers have settled. If a reload is requested
while a run is active, Tea rejects or defers the request; it never mutates the
policy underneath that run.

To reload, Tea builds and validates a complete candidate snapshot containing
the explicit source registry, its ordered closed bundles, the composed
declaration, and the separately selected grants/bindings. Only after every
part succeeds does the host atomically replace the current snapshot. A failed
reload leaves the previous snapshot untouched. A run captures one immutable
snapshot at start, including its authority selection, and all later turns in
that run use it. The next run sees the newly accepted snapshot. There is no
in-place module-cache mutation, partial reload, or mid-run source/grant swap.

## Limits and review checklist

Policies and handlers have host-selected finite source, memory, and Luau
interrupt budgets. Handler calls also have a finite host-call budget. The
current defaults are 64 KiB source, 1 MiB VM memory, 10,000 interrupt checks,
and 64 capability calls per handler invocation. A fresh VM per handler call
means a handler cannot leak a coroutine or mutable global into another call.

- Treat `arguments_json` as hostile model input and validate it structurally.
- Grant the smallest model tool set and exact host methods/targets.
- Make block reasons useful to the model but free of secrets.
- Test an allowed call, denied method, invalid arguments, host error, and
  cancellation for every new binding.
- Keep host effects in Rust. A policy declaration or manifest string is never
  authority.
- Do not place credentials in policy text, a prompt suffix, handler source, or
  tool environment.

For crate ownership and benchmark/test evidence see
[architecture](architecture.md) and [verification](verification.md).

### Tea host limitations

The minimal host design does not make `~/.tea` a repository-wide convention or
add it to the Luau crate. It has no ambient extension discovery, package
registry, marketplace, remote source loader, signature/trust store, automatic
grant approval, or active-run hot reload. It also has no cross-extension
mutable state, implicit dependency graph, session persistence, TUI/approval
surface, agent spawning, or world authority in the policy plane. Those are
host/application work for a separately specified contract. The durable policy
guarantees remain closed host-supplied bundles, fresh isolated VMs, finite
budgets, explicit grants and bindings, and Rust ownership of mechanism and
effects.
