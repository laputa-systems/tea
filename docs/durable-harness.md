# Durable harness

The durable harness is Tea's only terminal execution path. It joins a
single-writer v1 session, immutable artifacts, a resolved harness revision, a
model-harness profile, and the executor-neutral core into one auditable
operation boundary.

The relevant layers are intentionally narrow:

~~~text
tea-agent terminal
  -> tea_core::runtime::SessionRuntime
      -> tea-session semantic log and immutable objects
      -> tea_core::harness::HarnessResolver catalog, snapshots, candidates, revisions
      -> tea-core provider/tool execution
      -> redacted trace artifact
~~~

Before an effect crosses into the core, `SessionRuntime` records the durable
operation and epoch. Provider requests and tool calls use the effect gate, so
intent and settled outcome are represented in session state. Reconnect state is
derived from one atomic session snapshot plus post-commit live events.

Every epoch resolves the committed revision through `HarnessResolver`. The active
runtime services, prompt sections, tools, hooks, capability bindings, artifact policy,
and model-harness profile are all immutable for that epoch. A candidate can
change the next boundary only after validation and durable activation.

Runtime policy identities are owned by `RuntimeServices`. `HarnessSeedBuilder`
copies those identities into the immutable snapshot, and `HarnessResolver`
checks them again before both `SessionRuntime` and `HostedEpoch` construct an
agent. This keeps the executable hook, automatic-compaction, tool-projection,
and tool-failure policies paired with the snapshot metadata that names them;
the built-in defaults have stable identities as well.

Tool names, descriptions, and schemas form the provider surface. Scheduling and
cancellation controls (`execution_mode`, exclusive-batch, and cancellation
settlement) are host-only execution policy: they have a separate immutable
digest, are retained in snapshots and catalogs, and produce a distinct candidate
surface diff without changing the provider-surface digest.

The supervisor persists a content-redacted v1 trace artifact before it records
the epoch's terminal outcome. Trace provenance joins the operation, epoch,
core-run ID, revision, snapshot, and profile. See [trace](trace.md) and
[artifact recovery](artifact-recovery.md).

There is no terminal direct-core run path, separate extension registry, or
shadow session store.
