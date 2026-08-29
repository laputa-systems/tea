Implement a focused hardening of Tea's process-execution semantics.

The goal is **not** to add more tools or build a terminal/session protocol. Tea already has the right model-facing abstraction. Keep it.

The goal is to make the trusted `tea.process.v1` capability substantially more rigorous underneath that tiny interface: finite execution, owned subprocess lifetime, truthful settlement, strong cancellation/timeout behavior, and one canonical implementation shared by the core and terminal host.

Do this to completion, with deterministic tests. No real model provider or inference backend is required.

# Core design

Tea's default coding surface must remain exactly:

```text
read -> bash -> edit -> find
```

The `bash` tool must remain conceptually:

```json
{
  "command": "...",
  "timeout": 123
}
```

where `timeout` remains optional.

Do **not** add any of:

* `terminal`
* `exec`
* `start`
* `wait`
* `signal`
* `kill`
* `process`
* `background`
* `session`
* separate filesystem tools
* persistent-session IDs
* process handles exposed to the model
* PTY lease/state machinery
* another model-facing tool

All lifecycle complexity belongs below the Luau/model boundary.

The intended architecture remains:

```text
model
  |
  +-- read
  +-- edit
  +-- find
  `-- bash { command, timeout? }
         |
         v
     Luau builtin
         |
         v
     tea.process.v1
         |
         v
     trusted process execution + settlement
         |
         v
     normal Tea runtime / supervisor
```

Tea's existing durable harness remains the durability mechanism. Do not invent a second process-specific persistence system.

# Start from the existing implementation

Do not broadly remap the repository before beginning.

Read only the immediately relevant context first:

* `AGENTS.md`
* `docs/default-coding-profile.md`
* `docs/durable-harness.md`
* `crates/tea-core/src/coding/host/contract.rs`
* `crates/tea-core/src/coding/host/local_operations.rs`
* `crates/tea-core/src/coding/capabilities.rs`
* `crates/tea-agent/src/app/nonblocking_operations.rs`
* `crates/tea-luau/builtins/bash/init.luau`
* `crates/tea-luau/builtins/bash/handler.luau`
* `crates/tea-luau/builtins/bash/prompts.luau`
* `crates/tea-core/tests/coding_capabilities.rs`

Search specifically for all `CodingOperations` implementations before changing that trait.

Do not turn this into a generalized repository refactor.

# Existing problems to fix

The current process implementation has several semantics that are too weak for a durable coding agent.

Today:

* `CommandOutput` only has `exit_code`, stdout and stderr.
* timeout is optional all the way down into `CodingOperations`.
* a timeout/cancellation kills the shell process itself, not the command's owned descendants.
* the local implementation explicitly allows descendants to survive.
* `tea-agent/src/app/nonblocking_operations.rs` contains a second process execution implementation separate from the canonical core implementation.
* timeout currently collapses to a generic `"command timed out"` `OperationError`, throwing away the distinction between:

  * failure before execution,
  * a command that definitely ran and timed out,
  * a process terminated by signal,
  * cancellation,
  * and an execution whose final state cannot be proven.
* a post-spawn host failure can therefore be represented like an ordinary retryable failure even though filesystem/network side effects may already have happened.

Fix these at the process capability boundary.

# Non-negotiable invariant 1: finite execution

`bash` may keep `timeout` optional at the model-facing schema.

But **there must no longer be an unbounded trusted process operation**.

Resolve an omitted timeout at the trusted capability boundary to a finite host default.

Use:

```text
default timeout: 300 seconds
```

Keep Tea's current explicit upper timeout limit unless there is a strong existing invariant requiring a different representation:

```text
2147.483647 seconds
```

An explicit `timeout` still overrides the 300-second default.

Prefer making this impossible to violate at the Rust trait boundary. In particular, change the trusted execution port away from:

```rust
timeout_seconds: Option<f64>
```

toward an already-resolved finite duration, e.g.:

```rust
timeout: Duration
```

The Luau/model surface remains optional; the trusted host surface does not.

Do not add timeout configuration machinery in this task. A constant is sufficient.

# Non-negotiable invariant 2: typed process settlement

Replace the weak `exit_code: Option<i32>` contract with a precise termination type.

Use a design along these lines:

```rust
pub enum CommandTermination {
    Exited { code: i32 },
    Signaled { signal: i32 },
    TimedOut,
    Cancelled,
    Indeterminate { reason: String },
}

pub struct CommandOutput {
    pub termination: CommandTermination,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}
```

Names may vary slightly if repository conventions strongly prefer another spelling, but preserve these semantics.

The important distinction is:

### `Exited`

The process definitely exited normally with the supplied status.

### `Signaled`

The process definitely terminated because of a signal.

On Unix, use the real signal information from `ExitStatusExt`; do not turn it into exit code `-1`.

### `TimedOut`

The timeout won and Tea successfully established termination of the process scope that Tea owns.

This is a settled command outcome, not a generic pre-execution error.

Partial stdout/stderr should remain available.

### `Cancelled`

Cancellation won and Tea successfully established termination of the process scope that Tea owns.

At the `ProcessCapability` boundary, a clean `Cancelled` settlement should continue to become Tea's ordinary `ExtensionCapabilityError::Cancelled` so cancellation remains part of the runtime control flow rather than a normal model-visible tool error.

### `Indeterminate`

The command started, but Tea cannot honestly establish its final process state.

This is critical.

Examples include:

* failure while terminating an already-started process;
* failure while reaping or observing it where final state is uncertain;
* inability to establish that the owned process scope is gone after termination was requested;
* a post-spawn host failure where returning an ordinary `OperationError` would falsely imply that retrying is harmless.

The reason must be concise, bounded and actionable.

The model-facing result for an indeterminate process must clearly say, in substance:

```text
command termination is indeterminate; side effects may already exist; inspect state before retrying
```

Never silently map this to "command failed".

# Settlement precedence

Make the settlement ordering explicit.

Once `spawn()` succeeds, Tea has crossed the execution side-effect boundary.

After that point:

* do not casually return a generic `OperationError`;
* do not let a late cancellation overwrite an already-observed completed outcome;
* do not let cancellation/timeout claim success if cleanup itself is uncertain.

The conceptual precedence should be:

```text
known completed/signaled outcome
    beats later cancellation

indeterminate cleanup
    beats claiming clean cancellation or timeout

clean cancellation
    becomes runtime cancellation

clean timeout
    becomes a timed-out tool outcome
```

This is analogous to Tea's existing mutation philosophy: after a side-effectful operation has crossed its commit/start boundary, settle truthfully rather than pretending cancellation means nothing happened.

Pre-spawn failures remain ordinary `OperationError`s because Tea can establish that the command never began.

Examples:

* cannot allocate capture resources: ordinary error
* `spawn()` fails: ordinary error
* process starts, then observation breaks: `Indeterminate`
* timeout occurs and owned processes are definitely gone: `TimedOut`
* cancellation occurs and owned processes are definitely gone: `Cancelled`
* process exits 2: `Exited { code: 2 }`

# Non-negotiable invariant 3: own the foreground process scope

On Unix, including macOS and Linux, execute every ordinary `bash` command inside its own process group.

Use the standard library where possible, e.g. the Unix `CommandExt` process-group facility. Do not add a process-management framework.

Tea should be able to terminate the **whole process group**, not merely the `bash` leader.

The important case is:

```bash
python slow_child.py
```

If Tea times out or cancels while `python` is running, killing only the parent shell is incorrect. The child must not continue executing after Tea has reported that the command stopped.

Also cover:

```bash
python slow_child.py; touch SHOULD_NOT_EXIST
```

After timeout:

* the child must not still be running;
* `touch SHOULD_NOT_EXIST` must never execute;
* Tea must not return until its owned process scope has settled or has been explicitly classified `Indeterminate`.

Use a bounded termination protocol.

Do not wait indefinitely for cleanup.

It is acceptable for timeout to use a stronger signal path than ordinary graceful cleanup if necessary to guarantee that the shell cannot handle a graceful signal and continue evaluating trailing statements.

Correctness matters more than giving an arbitrary timed-out build script unlimited cleanup time.

# Foreground commands are foreground commands

Remove the current implicit semantic that ordinary `bash` execution may leak a background process intentionally.

A command such as:

```bash
server &
```

must no longer mean "start an unmanaged process that Tea forgets about".

When the foreground shell completes, Tea should inspect its owned process group. If descendants remain, clean them up before returning the normal command result.

This intentionally makes `bash` a bounded foreground operation.

Do not infer "durable process" intent from:

* `&`
* `nohup`
* shell syntax
* output patterns
* command names

Do not add a persistent-process tool in this task.

If a future Tea design needs durable services, they deserve an explicit supervised lifecycle rather than escaping from ordinary foreground `bash`.

Before changing this behavior, search narrowly for current Tea production/tests that explicitly depend on `bash` descendants surviving successful shell exit.

If no real caller depends on it, remove the old behavior.

If an actual production caller does depend on it, separate that caller from ordinary `bash` with a trusted host-only lifecycle boundary. Do not preserve the ambiguous behavior globally and do not add a new default model-facing tool.

# Be precise about the guarantee

Do not write documentation claiming Tea can magically kill every possible Unix descendant under all circumstances if the implementation only owns a process group.

A descendant that explicitly creates a new session/process group can deliberately escape ordinary process-group containment.

For this task, a strong **owned process-group** contract on macOS/Linux is acceptable.

State precisely what is guaranteed.

Do not claim more than tests can prove.

The ordinary coding-agent cases—shell children, grandchildren, normal background jobs—must be covered.

Preserve compilation on other platforms already supported by the repository, but do not broaden this task into a major Windows job-object project.

If a platform cannot provide the same guarantee, keep its semantics truthful rather than pretending cleanup was proven.

# Non-negotiable invariant 4: one canonical implementation

There are currently two local process loops:

* core: `crates/tea-core/src/coding/host/local_operations.rs`
* terminal host: `crates/tea-agent/src/app/nonblocking_operations.rs`

Do not harden both independently.

That guarantees semantic drift.

Make `tea-core` own the canonical local process semantics.

The terminal host should use that implementation rather than maintaining a second timeout/cancellation/process cleanup algorithm.

The terminal host currently has useful behavior that the canonical implementation must retain: incremental `ToolUpdate` emission while a command runs.

Move/retain that behavior in the canonical implementation rather than losing live output.

A reasonable structure is to extract the process implementation from the already-large `local_operations.rs` into something like:

```text
crates/tea-core/src/coding/host/process.rs
```

with:

* command construction;
* process-group setup;
* finite timeout loop;
* cancellation observation;
* process-group termination;
* cleanup verification;
* typed settlement;
* capture reading;
* incremental update emission.

Keep filesystem/edit transaction machinery out of that module.

Then have `LocalCodingOperations::execute_command` use the canonical process runner.

Adapt `NonblockingCodingOperations::execute_command` to delegate rather than reimplement process semantics.

Do not duplicate the new process supervisor in `tea-agent`.

No new crate is necessary.

# Dependencies

Do not add a dependency unless absolutely unavoidable.

In particular, do not add:

* Tokio
* async-process
* process-control frameworks
* nix merely for convenience
* a terminal emulator
* PTY infrastructure

Prefer:

* `std::process`
* existing Tea scheduling primitives
* stable Unix `CommandExt`
* a tiny cfg-gated OS shim where the standard library lacks a process-group signal operation.

A small direct Unix FFI boundary is preferable to importing a broad process-management dependency if that is all that is required.

Keep `tea-core` executor-agnostic.

# Preserve output behavior

Do not casually rewrite Tea's command-output pipeline as part of this task.

The existing private capture-file strategy was chosen partly so descendants cannot keep an executor pipe open.

It is fine to retain private capture files.

However:

* partial captured output must be readable after a timeout;
* partial output should survive a settled non-cancellation failure such as timeout or signal;
* incremental `ToolUpdate` behavior from the terminal's current nonblocking implementation must still work;
* cleanup paths must remove Tea-owned capture files;
* a post-spawn capture/observation failure must respect the indeterminate-side-effect rule.

Avoid an unrelated artifact-storage redesign.

If you discover an obvious unbounded-memory bug while touching final capture reads, fix it narrowly, but do not turn this task into a new general command-output subsystem.

# `tea.process.v1` result contract

Give the trusted capability a structured termination result.

The exact JSON names can follow existing style, but a response should be able to represent at least:

```json
{
  "content": "...",
  "truncated": false,
  "termination": "exited",
  "exitCode": 0
}
```

and:

```json
{
  "content": "...partial output...",
  "truncated": false,
  "termination": "timed_out",
  "exitCode": null
}
```

and:

```json
{
  "content": "...",
  "truncated": false,
  "termination": "signaled",
  "signal": 15,
  "exitCode": null
}
```

and:

```json
{
  "content": "...",
  "truncated": false,
  "termination": "indeterminate",
  "reason": "...",
  "exitCode": null
}
```

Do not represent clean cancellation as an ordinary completed capability response. Map the canonical `Cancelled` receipt back into Tea's cancellation path.

Preserve `exitCode` for straightforward compatibility if useful, but it is no longer the authoritative lifecycle discriminator.

Reject impossible combinations in tests.

# Luau `bash` behavior

Keep the provider-facing schema tiny.

Do not expose the structured host protocol wholesale to the model.

Update `crates/tea-luau/builtins/bash/handler.luau` so it translates the trusted process receipt into concise model-facing text.

Expected behavior:

### exit 0

Success, existing output formatting.

### nonzero exit

`is_error = true`.

If no output exists:

```text
command exited with status N
```

### signaled

`is_error = true`.

Always make the signal visible, even when there is partial output.

### timed out

`is_error = true`.

Always make timeout visible.

If partial output exists, retain it and add a concise timeout status.

### indeterminate

`is_error = true`.

Always include a strong warning that side effects may exist and the command must not simply be retried unchanged.

### cancelled

Use the runtime cancellation path; do not fabricate a normal bash result.

Keep result text compact. Do not dump lifecycle JSON into the conversation.

The schema should remain only `command` plus optional `timeout`.

You may update its description/prompt to make this useful fact clear:

> omitted timeouts still receive a finite host default

but do not add another model-visible argument.

# Concurrent-agent and parallel-tool safety

The process implementation must be safe under Tea's existing concurrent execution model.

Tea may concurrently execute:

* root and child-agent lanes;
* several sibling child agents;
* parallel tool calls within one lane operation;
* several `bash` calls whose lifetimes overlap.

Design process ownership at the **individual `execute_command` invocation** level.

Every spawned bash invocation must receive its own independent process group and its own local lifecycle state:

```text
execute_command A -> pgid A
execute_command B -> pgid B
execute_command C -> pgid C
```

Termination, timeout, cancellation, output capture, cleanup and indeterminate settlement for one invocation must never affect another invocation.

## No global process ownership

Do not introduce:

* a singleton process manager;
* a session-wide process registry;
* an agent-wide process registry;
* a global current PID/PGID;
* a global process-cleanup mutex;
* a workspace-wide process sweeper;
* `killall`;
* `pkill`;
* process-name-based cleanup;
* cwd/worktree-based process discovery;
* `/proc` scanning used to guess which processes belong to an invocation;
* process-wide signal handlers for ordinary command lifecycle management.

A small global atomic counter used only to allocate collision-free capture filenames is fine. It must not represent lifecycle ownership or serialize execution.

Do not serialize `execute_command` globally as a shortcut.

Tea's existing parallel-command regression must continue to pass.

## Process groups are invocation-local capabilities

On Unix, establish the child process group as part of spawning that exact command.

Retain its PGID only inside that invocation's lifecycle object/stack frame.

All signals must target that exact owned group.

Conceptually:

```text
spawn bash A
    -> establish pgid A
    -> invocation A owns pgid A

spawn bash B
    -> establish pgid B
    -> invocation B owns pgid B

timeout A
    -> signal pgid A only

cancel B
    -> signal pgid B only
```

Never derive a kill target from:

* agent ID;
* lane ID;
* workspace path;
* executable name;
* command string;
* another command's process state.

Never persist an invocation PGID for delayed cleanup after the invocation has settled. There must be no later sweeper that can signal a stale/reused PGID.

Once the owned group is established gone, forget its PGID.

## Timeout is local; cancellation is external

An individual command timeout must **not call `cancel()` on Tea's supplied `CancellationToken`**.

That token may represent a larger operation containing sibling parallel tools.

A timeout belongs only to the current process invocation:

```text
command A times out
    -> terminate process group A
    -> return TimedOut for A

command B running concurrently
    -> unaffected
```

Conversely, when the supplied Tea cancellation token becomes cancelled, every affected running tool invocation may observe it independently and must clean up only its own process group before settling.

Do not propagate process timeout by mutating shared runtime cancellation state.

## Subagent interruption

Tea's `interrupt_agent` already owns child-operation cancellation and joining.

Do not add subagent-specific process-management logic.

The expected composition is:

```text
interrupt_agent
    -> child operation cancellation
        -> running bash capability observes cancellation
            -> bash invocation terminates its own process group
            -> capability settles Cancelled
        -> child operation joins
    -> existing subagent finalization continues
```

The process layer must not know about `AgentId`, `LaneId`, `WorkspaceLeaseId`, or the subagent graph.

This separation is intentional.

## CodingHost isolation remains authoritative

Each child `CodingHost` remains bound to its leased child worktree.

Do not move process cwd/workspace authority into global process-runner state while consolidating the core and terminal implementations.

The canonical runner should receive all invocation-specific state explicitly:

```text
command
cwd
resolved finite timeout
environment
cancellation
ToolUpdateSink
```

and return one typed outcome.

It should not need to know which agent invoked it.

## ToolUpdate isolation

Incremental command updates must remain bound to the `ToolUpdateSink` supplied for that exact invocation.

Never use a process-global output buffer or broadcaster.

Parallel commands must not interleave bytes into one another's logical tool result, even though terminal presentation may naturally display asynchronous updates according to the existing event model.

Each command retains independent stdout/stderr capture files and offsets.

## Test seams must also be concurrency-safe

Any low-level test seam introduced to force:

* termination failure;
* observation failure;
* indeterminate settlement;

must be injection-local.

Do not use a mutable global "fail next kill" flag or environment variable that could perturb another concurrently running test/tool invocation.

Prefer a tiny private process-control interface or function-local injected controller whose instance belongs to the command under test.

## Required concurrency regressions

Add deterministic regressions beyond the existing parallel-start test.

### Parallel invocation isolation

Start two real local commands concurrently.

Command A:

* records its child PID;
* remains running;
* receives a short timeout.

Command B:

* records its child PID;
* remains running longer than A's timeout;
* is then explicitly allowed to complete successfully.

Prove:

* A settles `TimedOut`;
* A's child is dead before A settles;
* B is still alive after A cleanup;
* B ultimately settles successfully;
* A cleanup never signals B.

This should use the same `CodingOperations` instance if possible, proving the implementation itself is safe for sharing.

### Cancellation isolation

Start two commands concurrently.

Cancel only the execution context associated with A while B remains uncancelled.

Prove:

* A's owned process group is cleaned;
* B remains alive;
* B completes normally.

If Tea's existing cancellation topology naturally shares one token among parallel calls from a single operation, perform this test directly against the canonical process runner with independent tokens. The purpose is to freeze the process layer's invocation-local semantics.

### Separate CodingHost isolation

Construct two `CodingHost`s backed by the same canonical operations implementation but rooted in different temporary workspaces, analogous to root/child or sibling child lanes.

Run one long command through each concurrently.

Timeout/cancel host A's command.

Prove host B's command and child process are unaffected.

No real model or subagent provider is needed for this regression.

### Subagent interruption integration

If the existing deterministic subagent test harness makes this reasonably small, add one integration regression:

```text
spawn child
child enters long-running bash with an ordinary descendant
interrupt_agent
```

Prove the child operation joins without leaving that descendant alive.

Do not construct a new integration framework solely for this test. The process-level isolation regressions above are the fundamental contract; use the existing subagent harness only if it can exercise this composition cleanly.


# Tests: write these first

Treat this as lifecycle-contract work. Add failing tests before changing the implementation.

The test suite must cover at least the following.

## 1. Finite default

Call `tea.process.v1` without `timeout`.

Prove the trusted operation receives/uses the finite default rather than `None`.

You do not need to actually sleep 300 seconds. Test the resolution boundary directly or use an injected operation adapter that observes the resolved duration.

Assert exactly 300 seconds.

## 2. Explicit timeout override

Provide a small explicit timeout.

Prove it replaces the 300-second default.

Keep the existing maximum validation.

## 3. Ordinary successful execution

```bash
printf success
```

must produce:

```text
Exited { code: 0 }
```

with correct output.

## 4. Nonzero execution

```bash
printf failure >&2; exit 7
```

must settle as:

```text
Exited { code: 7 }
```

not as a host execution failure.

## 5. Signal termination

On Unix, deliberately signal the command shell/process.

Prove the result is `Signaled` with the actual signal rather than `exit_code = None` / `-1`.

## 6. Timeout blocks trailing statements

Create a command equivalent to:

```bash
printf started
sleep 30
touch post-timeout-marker
```

with a very short timeout.

After settlement:

* termination is `TimedOut`;
* partial `started` output is retained;
* `post-timeout-marker` does not exist.

Do not use flaky timing assumptions; use filesystem/process synchronization where useful.

## 7. Timeout kills child processes, not only bash

Start a child that would live for a long time and record its PID in the temporary workspace.

Let Tea time out.

Prove the child is no longer alive before the process operation settles.

This must exercise the real local process implementation.

## 8. Successful shell exit does not leak background descendants

Run something equivalent to:

```bash
sleep 30 &
echo $! > child.pid
```

Allow the shell itself to exit normally.

Tea must clean up the remaining owned process group before returning.

Verify the recorded child is dead.

This freezes the new foreground-only contract.

## 9. Cancellation kills the owned process group

Start a shell and child, wait until the fixture proves both exist, trigger `CancellationToken`, and prove:

* process group cleanup occurs;
* the child does not survive;
* the operation settles through Tea cancellation;
* a trailing post-cancellation marker cannot appear.

## 10. Late cancellation does not erase a completed result

Create a deterministic race or test adapter proving that once command completion has been observed, a later cancellation does not rewrite it into `"cancelled"`.

This is an important settlement invariant.

## 11. Indeterminate outcome

Test this deterministically.

Do not rely on producing a naturally flaky kernel failure.

Refactor the low-level process-control boundary enough that a test can inject a cleanup/observation failure after process start.

Prove that it becomes:

```text
Indeterminate
```

with an inspect-before-retry warning.

Do not use a global production environment variable test hook unless there is no cleaner local seam.

## 12. Pre-spawn error remains ordinary error

Prove that a failure before successful spawn does not become `Indeterminate`.

The distinction matters.

## 13. Luau result formatting

Add/update builtin tests proving:

* zero exit is successful;
* nonzero exit is an error;
* signal is rendered truthfully;
* timeout is rendered truthfully with partial output;
* indeterminate always includes the no-blind-retry warning;
* no new bash arguments exist.

## 14. Parallelism remains intact

Retain the existing regression that two independent commands can begin before either finishes.

Do not serialize all process execution globally as a shortcut for correctness.

## 15. Live updates remain intact

Preserve the terminal host's incremental process-output update behavior after consolidating the two implementations.

Add a focused regression if current tests don't already prove it.

# Process cleanup tests must be real

For Unix process tests, verify actual lifecycle state rather than only checking Tea's returned enum.

Useful assertions include:

* child PID existed before cancellation/timeout;
* process group termination was requested;
* child PID no longer exists before settlement;
* trailing filesystem side-effect marker never appears.

Build the test helpers carefully so failed tests do not leak long-running processes.

Use RAII cleanup guards where appropriate.

Tests themselves must not leave children behind on assertion failure.

# Avoid PID-race lies

Be careful when verifying process death.

Do not write a test that simply sleeps and assumes a PID now refers to the same process.

Keep tests short, synchronized, and use the owned process-group identity where practical.

Likewise, production code must not kill arbitrary unrelated processes because a stale identifier was reused.

Use process-group semantics established at spawn time.

# Update all `CodingOperations` adapters

Changing the execution port will affect more than `LocalCodingOperations`.

Search for every:

```rust
impl CodingOperations for ...
```

and update them deliberately.

At current main this includes application mocks/wrappers in `tea-agent` as well as the core implementation.

Mocks should model the new typed termination contract rather than retaining `Option<i32>` compatibility internally.

Do not add compatibility shims that keep both old and new process-result models alive.

This is the current v1 design; cleanly migrate it.

# Documentation

Update `docs/default-coding-profile.md` to describe the process contract accurately.

It should communicate:

* default tools remain exactly `read -> bash -> edit -> find`;
* `bash` is a bounded foreground execution primitive;
* omitted model timeout receives a finite host default;
* foreground commands own a process scope;
* timeout/cancellation settle that scope before claiming success;
* ordinary `bash` does not create unmanaged durable background workers;
* indeterminate execution state must be inspected before retrying;
* durable process management is not part of the default coding surface.

Update another semantics document only if the process settlement rule belongs there naturally. Do not scatter duplicate prose across many docs.

Delete obsolete comments claiming descendants intentionally survive.

# Keep these things unchanged

Do not change:

* the four-tool default coding surface;
* `read`;
* `edit`;
* `find`;
* the unified mutation transaction design;
* subagent architecture;
* provider adapters;
* compaction;
* persistence;
* the TUI layout;
* the durable session format;
* MCP support/non-support;
* web tooling;
* harness revision semantics.

Do not add a backwards-compatibility layer for the old internal `CommandOutput` shape.

Migrate current code and tests directly.

# Implementation quality

This should feel like Tea, not like a transplanted terminal subsystem.

Prefer:

* small explicit types;
* a narrow process module;
* one execution implementation;
* precise state transitions;
* no hidden fallback;
* no dependency growth;
* deterministic tests;
* bounded waits;
* honest uncertainty;
* host complexity hidden from the model.

Avoid:

* sprawling generic process abstractions;
* a public process-manager framework;
* clever shell parsing;
* heuristics for detecting daemon intent;
* new persistent state;
* polling registries visible to the model;
* one enum with dozens of terminal actions.

A useful mental model is:

```text
bash is tiny;
tea.process.v1 is strict;
the durable harness remains authoritative.
```

# Suggested implementation sequence

Use TDD.

1. Add typed process termination/output contracts in `tea-core`.
2. Change the `CodingOperations::execute_command` boundary to require a finite resolved timeout.
3. Add capability tests for the 300-second default and explicit override.
4. Extract/consolidate canonical local process execution in `tea-core`.
5. Add Unix process-group ownership.
6. Add timeout descendant/trailing-side-effect regressions.
7. Add cancellation descendant regressions.
8. Add normal-completion background cleanup regression.
9. Add signal and indeterminate settlement.
10. Preserve/move incremental `ToolUpdate` streaming into the canonical implementation.
11. Delete the duplicate lifecycle implementation from `tea-agent`.
12. Update `tea.process.v1` structured response.
13. Update the Luau `bash` handler and tests.
14. Update documentation.
15. Run focused tests.
16. Run the complete deterministic repository suite.

Commit in coherent increments if the environment/workflow permits it. Do not create a plan document unless the repository's normal workflow specifically requires one.

# Verification

Start narrow.

At minimum run focused tests for:

```text
tea-core coding host/process tests
tea-core coding_capabilities tests
tea-luau builtin tests
tea-agent process/nonblocking tests
```

Then run the repository's deterministic suite:

```bash
make test
```

This includes the workspace tests and PTY harness. No real provider should be required.

Because process-group behavior matters on both Darwin and Linux, also run:

```bash
make test-linux
```

when Docker is available.

Run formatting/linting through the repository's existing workflow as appropriate:

```bash
make lint
```

Do not paper over a failing existing PTY test by updating visual expectations unless this change genuinely changes user-visible semantics. Process lifecycle hardening should not alter TUI layout.

# Final acceptance criteria

The work is complete only when all of these are true:

* default model-facing tools are still exactly `read`, `bash`, `edit`, `find`;
* `bash` still exposes only `command` and optional `timeout`;
* omitted timeout becomes a finite 300-second trusted timeout;
* no trusted process execution path accepts an unbounded timeout;
* the process result has a typed lifecycle outcome;
* normal nonzero exit is distinct from infrastructure failure;
* Unix signal termination is represented accurately;
* timed-out commands preserve available partial output;
* timeout kills ordinary child/grandchild processes in the owned process group;
* timeout cannot allow trailing shell statements to execute afterward;
* cancellation cleans up the owned process group before reporting clean cancellation;
* a late cancellation cannot erase an already-observed completion;
* post-spawn uncertainty becomes `Indeterminate`, never a misleading generic failure;
* indeterminate model-facing output explicitly warns against blind retry;
* successful `bash` completion does not leak ordinary background descendants;
* `tea-core` contains the canonical process execution algorithm;
* `tea-agent` no longer owns a divergent timeout/process cleanup implementation;
* incremental tool-output updates still work;
* parallel command execution still works;
* no new dependency was added unless truly unavoidable and explicitly justified;
* no new model-facing process/terminal tool exists;
* docs describe the new foreground-process contract accurately;
* `make test` passes;
* Linux process tests pass via the repository's existing Linux path when available.

When finished, give me a concise implementation summary covering:

1. the final process lifecycle contract;
2. the files changed;
3. the process-group/termination strategy;
4. how indeterminate outcomes work;
5. how the duplicate core/terminal process implementations were unified;
6. the exact focused and broad test commands run and their results;
7. any platform limitation that remains.

Do not stop after writing a plan. Implement the complete change and verify it.

## Acceptance criteria additions

The implementation is not complete unless:

* every concurrent `bash` invocation has independent lifecycle ownership;
* timing out one command cannot cancel sibling tools;
* cancelling one invocation cannot signal another invocation's process group;
* root and child coding hosts can execute commands simultaneously;
* sibling child coding hosts can execute commands simultaneously;
* no global lock serializes process execution;
* no global process registry is required for correctness;
* no process cleanup is selected by command name, workspace or agent identity;
* no process-wide signal handler creates cross-agent interference;
* process test fault injection is invocation-local;
* the existing `bash` `execution_mode = "parallel"` remains unchanged;
* existing subagent concurrency limits and lane semantics remain unchanged.

