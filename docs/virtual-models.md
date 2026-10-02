# Virtual models

A virtual model is a selection that routes each request to one physical model
from a **host-approved set**. It is adapted from upstream Pi's
`core/virtual-models.ts` and `examples/extensions/jev-router.ts` (Pi
`a13d35a742c6`), with tea's stricter authority and immutable-extension
boundaries.

## Contract (`tea_core::routing`)

- The selection is a descriptor under the `virtual` provider, such as
  `virtual/plan-build`. Below the routing step everything is physical:
  `ModelRequest::model` is the dispatched model and `selected_model` records
  the selection. Provider capabilities, context limits, reasoning support,
  provider-private continuation data, cache eligibility, and accounting all
  follow the physical model, and the assistant message records it as its
  `origin`. The durable provider request material records both identities.
- A `ModelRouter` is deterministic and cheap: no classifier calls, speculative
  dispatch, or I/O. It sees the selection, reasoning level, reason
  (`user`, `continuation`, `retry`), the previous physical model, the approved
  targets, its persisted state, and the conversation.
- **Approved targets.** The host approves physical models
  (`RuntimeServices::approved_models`); an extension may narrow them. A route
  outside the approved set, a virtual selection without a router, or a virtual
  model with no approved targets fails the run with `CoreError::ModelRouting`
  before any provider request. Nothing silently falls back to another provider.
- **Sticky continuations.** Tool continuations and retries stay on the
  physical model their turn started on, without calling the router, unless the
  virtual model explicitly declares `continuations = "routed"`.
- **State.** A router's state is its extension's bounded per-lane
  `extension.state` value (at most 16 KiB of JSON). Each change is committed
  through the durable effect gate as an `ExtensionStateValueSet` fact, so it
  follows settled-turn forks and reopen exactly like other extension state.
  An in-memory agent keeps it for its lifetime.
- **Cache warming** replays the admitted physical request and never calls the
  router again.

## Luau declaration

An ABI-v3 extension may declare:

```lua
virtual_models = {
  {
    id = "plan-build",              -- selected as virtual/plan-build
    name = "Plan, then build",
    targets = { "anthropic/claude-opus-5-5" }, -- optional narrowing
    continuations = "sticky",       -- default; or "routed"
    route = function(request)
      -- request.reason, .selected, .thinking_level, .previous, .targets,
      -- .state, .last_user_text (<= 4 KiB), .tool_results and
      -- .previous_turn_tool_results ({ name, is_error }, <= 64 each)
      return { target = request.targets[1], state = { content_json = '{"phase":"plan"}' } }
    end,
  },
}
```

The route function runs in the extension's sandboxed policy VM with its usual
memory and instruction limits and no capabilities. Returning `state` requires
the extension's `extension.state` contract (`state_version`).

## Example: plan, then build

[`crates/tea-luau/examples/plan_build_router`](../crates/tea-luau/examples/plan_build_router)
plans on the first approved model and implements on the second. After a turn
in which an `edit` or `write` succeeded, the next user turn moves to the build
phase and stays there; a prompt starting with `plan:` returns to planning.
Continuations stay sticky.

The terminal offers it when routing is configured:

```toml
# $TEA_HOME/config.toml
[routing]
approved_models = ["anthropic/claude-opus-5-5", "anthropic/claude-haiku-4-5"]
```

Then pick `Virtual · plan-build` in `/models` or start with
`tea --provider virtual --model plan-build`. Sessions created with routing
enabled seed the router into their immutable harness. The terminal checks
credentials for every approved target before admitting a prompt, dispatches
each request to the routed physical adapter, uses the smallest approved
context window for automatic compaction, and sends direct requests (such as
compaction summaries) to the first approved model.

## Evidence

```sh
cargo test -p tea-core --all-features --test routing
cargo test -p tea-luau --test plan_build_router
cargo test -p tea-agent --lib durable_virtual_model
cargo test -p tea-agent --lib router_state_is_branch_local
cargo test -p tea-agent --lib routing_approves
```
