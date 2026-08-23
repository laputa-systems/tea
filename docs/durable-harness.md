# Durable harness

The durable harness is Tea's only terminal execution path. It joins a
single-writer v1 session, immutable artifacts, a resolved harness revision, a
model-harness profile, and the executor-neutral core into one auditable
operation boundary.

The relevant layers are intentionally narrow:

~~~text
tea-agent terminal
  -> DurableHarness
      -> tea-session semantic log and immutable objects
      -> HarnessManager catalog, snapshots, candidates, revisions
      -> tea-core provider/tool execution
      -> redacted trace artifact
~~~

Before an effect crosses into the core, DurableHarness records the durable
operation and epoch. Provider requests and tool calls use the effect gate, so
intent and settled outcome are represented in session state. Reconnect state is
derived from one atomic session snapshot plus post-commit live events.

Every epoch resolves the committed revision through HarnessManager. The active
template, prompt sections, tools, hooks, capability bindings, artifact policy,
and model-harness profile are all immutable for that epoch. A candidate can
change the next boundary only after validation and durable activation.

The supervisor persists a content-redacted v1 trace artifact before it records
the epoch's terminal outcome. Trace provenance joins the operation, epoch,
core-run ID, revision, snapshot, and profile. See [trace](trace.md) and
[artifact recovery](artifact-recovery.md).

There is no terminal direct-core run path, separate extension registry, or
shadow session store.
