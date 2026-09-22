# Quickstart

This guide runs an in-memory v1 core epoch with a caller-owned model provider
and Smol executor. The core never discovers a provider, workspace, or
credential for you. Use the durable harness when the run must survive process
loss or retain recovery evidence.

## Build the repository

The checked-in toolchain is required; do not substitute stable Rust. Plain
`cargo` resolves the pin in `rust-toolchain.toml` through the rustup shim.

```bash
git clone <repository-url> tea
cd tea
cargo test --workspace
```

For an application in the same checkout, depend on the core and choose the
executor yourself:

```toml
[dependencies]
tea-core = { version = "1", path = "../tea/crates/tea-core" }
smol = "=2.0.2"
```

`smol` belongs to the application here, not to `tea-core`. Tokio is not a
supported runtime dependency in this project.

## Run one deterministic agent

This complete example uses the finite `ModelStream` test adapter. A production
provider implements the same `ModelProvider` port and returns an incremental
`ModelEventStream` instead.

```rust
use tea_core::scheduler::{
    CancellationToken, ModelEventStream, ModelFuture, ModelProvider, ModelStream,
    ModelStreamEvent,
};
use tea_core::state::{ModelDescriptor, StopReason};
use tea_core::Agent;
use std::sync::Arc;

struct DemoProvider;

impl ModelProvider for DemoProvider {
    fn stream<'a>(
        &'a self,
        _request: tea_core::scheduler::ModelRequest,
        _cancellation: CancellationToken,
    ) -> ModelFuture<'a> {
        let stream = ModelStream {
            events: vec![
                ModelStreamEvent::TextDelta("Hello from the model.".into()),
                ModelStreamEvent::End(StopReason::Stop),
            ],
        };
        Box::pin(std::future::ready(Ok(
            Box::new(stream) as Box<dyn ModelEventStream>
        )))
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let agent = Agent::builder()
        .system_prompt("Be concise.")
        .model(ModelDescriptor {
            provider: "example".into(),
            model: "demo".into(),
            revision: None,
        })
        .model_provider(Arc::new(DemoProvider))
        .build();

    smol::block_on(agent.start_prompt("Say hello.")?.drive())?;
    println!("{:#?}", agent.snapshot().messages);
    Ok(())
}
```

`start_prompt` reserves the one active run. Drive its returned `RunHandle` on
your executor. The same agent may be reused only after the run has settled.
Call `agent.abort()` from the host to request structured cancellation, then
await the run or `agent.wait_for_idle()`.

For a bounded live host projection, subscribe before starting the run and
handle lag by taking a fresh snapshot:

```rust
let events = agent.subscribe_nonblocking(std::num::NonZeroUsize::new(256).unwrap());
let run = agent.start_prompt("Say hello.")?;
smol::block_on(run.drive())?;
loop {
    match events.try_recv() {
        Ok(event) => println!("{event:?}"),
        Err(tea_core::agent::EventSubscriptionTryRecvError::Lagged) => {
            println!("Refreshed state: {:?}", agent.snapshot());
            break;
        }
        Err(_) => break,
    }
}
```

The queue is bounded and reports lag explicitly. The driven run's return value
is its completion mechanism; do not infer completion from an observation stream.

## Use durable input ownership

The runnable offline example uses immutable harness construction, the same
Agent engine, and an independent accepted-input completion handle:

```sh
cargo run -p tea-core --example session --locked
```

After constructing a `SessionSupervisor` from explicit services and persistence:

```rust,ignore
let input = runtime.submit_input("Say hello.")?;
runtime.drive_next_input(IdleAuthorization::UserInputOnly).await?;
let result = input.completion().wait().await;
```

The example uses `MemorySession` and makes no crash-durability claim. Supply a
fresh explicit `JsonlSession` and artifact store for file persistence. Reopen
only restores committed state; inspect `recovery_report()` before an explicit
`resume().await`. Queued inputs and goals never dispatch automatically on open.

A one-shot CLI invocation uses the same durable runtime:

```sh
tea --tea-home /tmp/tea-example-data --cwd /tmp/tea-example-workspace \
  --provider mock --prompt "Say hello."
```

Create fresh disposable directories before that example. `/continue` explicitly
resumes interrupted work in the terminal. A fork accepts a recorded settled-turn
checkpoint and does not roll back workspace files. An embedding selects that
durable identity rather than an arbitrary entry ID:

```rust,ignore
use tea_session::{LaneId, TurnCheckpointId};

let fork = runtime.fork_settled_turn(
    TurnCheckpointId::new("recorded-checkpoint")?,
    LaneId::new("review-alternative")?,
)?;
assert_eq!(fork.lane_id().as_str(), "review-alternative");
```

The root lane must be idle. The fork starts with the checkpoint's history,
historical harness configuration, and private extension-state snapshot, but no
queued input, effects, live handles, child ownership, or execution authority.

## Add manual compaction explicitly

The core never invents a summary prompt. If an embedding supplies a
`Compactor`, it can reserve an idle agent and drive a validated transaction on
the same executor:

```rust
let compaction = agent.start_compaction()?;
smol::block_on(compaction.drive())?;
```

The compactor receives an owned, versioned context and proposes replacement
messages. Core rejects duplicate message IDs and malformed tool-result links,
does not modify history on failure or cancellation, and emits
`compaction_start`, `compaction_result`, then `compaction_end`. An agent
without a configured compactor returns `CoreError::MissingCompactor` rather
than silently selecting a provider or summary policy.

For long-running loops, opt in on the builder with an explicit capacity and
the same compactor. `AutomaticCompactionPolicy` has no provider or prompt
field: the host owns both summary policy and capacity authority. It compacts
after a completed assistant/tool turn, before the next request; typed overflow
recovery accepts only `ModelStreamEvent::ContextOverflow` and retries an
incomplete continuation at most at the configured limit.

```rust,no_run
use tea_core::{AutomaticCompactionPolicy, ContextBudgetSource, OverflowRecovery};
use std::num::NonZeroU64;

let policy = AutomaticCompactionPolicy {
    enabled: true,
    context_budget: ContextBudgetSource::ContextWindow(NonZeroU64::new(128_000).unwrap()),
    reserved_tokens: 8_000,
    minimum_headroom_tokens: 8_000,
    recent_tokens: 16_000,
    overflow_recovery: OverflowRecovery::CompactAndRetry,
    max_compactions_per_run: 4,
    max_overflow_retries_per_run: 1,
};
// let agent = Agent::builder().compactor(my_compactor).automatic_compaction(policy)?.build();
# let _ = policy;
```

## Add Tea's default coding builtins

The repository-owned terminal resolves four checked-in Luau coding builtins
through its durable harness. Its model-facing surface is exactly `read`,
`bash`, transactional `edit`, and optimized `find`; Rust binds the narrow
workspace and process capabilities beneath them. A library embedding that wants
these builtins should use the same `HarnessSeedBuilder`/capability-catalog path
rather than constructing Rust coding tools directly. See the
[default coding builtins guide](default-coding-profile.md) before granting a
real filesystem or process capability.

## Enable durable subagents explicitly

Library integrations opt in by supplying both workspace and task-runtime host
ports when constructing the durable supervisor. All supervisor inputs are
explicit, including the automatic-harness rollover budget:

```rust,ignore
let enabled_supervisor = SessionSupervisor::create(SessionSupervisorInput {
    session,
    resolver,
    root_identity,
    root_services,
    artifacts,
    rollover_budget: 0,
    subagents: Some(SubagentServices {
        policy,
        host: Arc::new(my_host),
        tasks: Arc::new(my_task_runtime),
    }),
})?;
```

The ordinary disabled form supplies the same durable inputs but creates no
coordinator, child provider factory, or collaboration tools:

```rust,ignore
let disabled_supervisor = SessionSupervisor::create(SessionSupervisorInput {
    session: disabled_session,
    resolver: disabled_resolver,
    root_identity: disabled_root_identity,
    root_services: disabled_root_services,
    artifacts: disabled_artifacts,
    rollover_budget: 0,
    subagents: None,
})?;
```

`~/.tea/config.toml` belongs only to the repository-owned terminal application;
it is not an SDK configuration source. See [durable subagents](subagents.md)
for host-port, persistence, isolation, cancellation and recovery obligations.

## Connect a real model and world

Implement `ModelProvider::stream` outside the core. Return a stream source as
soon as transport setup succeeds, emit text/tool-call deltas incrementally,
and race any I/O with the supplied `CancellationToken`. Implement tools as
`AgentTool` values with narrow schemas and explicit authority. Do not put
provider credentials in the core, a system prompt, a tool environment, or a
Luau policy.

For two narrow, optional HTTP adapters, see [provider adapters](provider-adapters.md).
They require explicit Cargo features and caller-supplied configuration; the
default build remains provider-free.

For durable host integration, begin with [the durable harness](durable-harness.md)
and [harness recovery](harness-recovery.md). For the pure core request, tool,
queue, hook, and terminal contracts, read [runtime semantics](semantics.md).
For an optional capability-scoped Luau policy, start with
[Luau ABI v3](luau-abi-v3.md); a scripting VM is not required for ordinary Rust
agents.

If another control plane already owns its session, first seed and resolve an
immutable harness with `HarnessSeedBuilder` and `HarnessResolver`, then pass the
`ResolvedHarness` to `RuntimeServices::prepare_hosted_epoch`. The returned
`HostedEpoch` exposes the caller-driven `Agent`, normalized `RunProvenance`, and
standard harness surface fingerprints without creating a Tea session or adding
implicit tools. This is the integration boundary for external durable authorities;
it is not a lightweight replacement for `SessionSupervisor` recovery.
