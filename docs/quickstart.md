# Quickstart

This guide runs an in-memory v1 core epoch with a caller-owned model provider
and Smol executor. The core never discovers a provider, workspace, or
credential for you. Use the durable harness when the run must survive process
loss or retain recovery evidence.

## Build the repository

The checked-in toolchain is required; do not substitute stable Rust.

```bash
git clone <repository-url> tea
cd tea
cargo +nightly-2026-07-24 test --workspace
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

For a live host projection that cannot tolerate event loss, subscribe before
starting the run:

```rust
let events = agent.subscribe_lossless();
let run = agent.start_prompt("Say hello.")?;
smol::block_on(run.drive())?;
while let Ok(event) = events.try_recv() {
    println!("{event:?}");
}
```

This subscription uses an explicitly unbounded standard-library queue. Unread
events retain caller-owned memory until drained or the subscription is dropped;
the existing `subscribe_nonblocking` API remains the bounded best-effort path.

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

## Add Tea's default coding profile

The default profile is optional. When selected, provide an existing workspace
explicitly; it never infers a working directory or reads ambient configuration.

```rust,no_run
use tea_core::Agent;
use tea_core::coding::{TeaCodingToolsV2, TeaDefaultCodingProfileV2};

let tools = TeaCodingToolsV2::new("/absolute/workspace")?;
let profile = TeaDefaultCodingProfileV2::pinned_default()?;
let registry = tools.registry();
profile.validate_registry(&registry)?;
let agent = Agent::builder()
    // Also configure .model_provider(...) before running.
    .system_prompt(profile.system_prompt_for_workspace(tools.workspace().as_path()))
    .tools(registry)
    .build();
# let _ = agent;
# Ok::<(), Box<dyn std::error::Error>>(())
```

The active tools are `read`, `bash`, transactional multi-file `edit`, and
`write`; every operation is replaceable through
`TeaCodingToolsV2::with_operations`. `DefaultCodingTools` remains available
only for explicit compatibility with the pinned Pi v1 profile. See the
[default-profile guide](default-coding-profile.md) before granting a real
filesystem or process capability.

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
[Luau ABI v1](luau-abi-v1.md); a scripting VM is not required for ordinary Rust
agents.

If another control plane already owns its session, first seed and resolve an
immutable harness with `HarnessSeedBuilder` and `HarnessResolver`, then pass the
`ResolvedHarness` to `RuntimeServices::prepare_hosted_epoch`. The returned
`HostedEpoch` exposes the caller-driven `Agent`, normalized `RunProvenance`, and
standard harness surface fingerprints without creating a Tea session or adding
implicit tools. This is the integration boundary for external durable authorities;
it is not a lightweight replacement for `SessionRuntime` recovery.
