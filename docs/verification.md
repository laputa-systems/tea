# Verification and completed contract evidence

V0 is complete. Its implementation is the pure Rust agent kernel, pinned
default coding profile, deterministic in-process fixture corpus, structured
cancellation, optional trace observer, and Rust coding evaluation. The optional
Luau extension layer is separately verified without
changing the core's provider-, executor-, or world-agnostic boundary.

## Required local checks

Run the repository's pinned nightly toolchain:

```bash
make test
git diff --check
```

`make test` runs the locked workspace suite, the normalized UI fixture check,
and the real-binary PTY suite. All three use deterministic fixtures and do not
contact a live provider.

On a host with Docker, `make test-linux` repeats the same checks in a Linux
AArch64 Alpine/musl container (`linux/arm64`), including the PTY visual suite.
The image uses LLVM compiler-rt and libc++; it does not install glibc, GCC, or
libstdc++ in the final image. The Python fixture checker is installed only
while the image is being built and is removed before the image is tagged.
The `libgcc_s.so.1` name is a compatibility symlink to Alpine's LLVM
`libunwind`; it is not the GCC runtime.

For a profile or contract change, also run the Rust fixture check in
[`crates/tea-core/fixtures/README.md`](../crates/tea-core/fixtures/README.md)
and the profile tests described in
[`crates/tea-core/profile/README.md`](../crates/tea-core/profile/README.md).
A new supported
behavior requires a deterministic fixture before implementation.

For the trace-first quality gate, run:

```bash
PYTHONDONTWRITEBYTECODE=1 python3 -m evals.quality fast
PYTHONDONTWRITEBYTECODE=1 python3 -m evals.quality resources
PYTHONDONTWRITEBYTECODE=1 python3 -m evals.quality compaction --out /tmp/tea-compaction-quality
```

The live three-Express-task check is manual and requires explicit provider
authorization. Its Rust adapter, cache procedure, full validator, and resource
interpretation are documented in
[`evals/quality/README.md`](../evals/quality/README.md).

For prompt-cacheability and compaction-prefix evidence, run the deterministic fixture described
in [`docs/cache-friendliness.md`](cache-friendliness.md). Its common-prefix values are a proxy;
provider-reported cache usage remains the authoritative hit/write measurement.

The compaction matrix is the provider-free CI gate. It writes 70 content-free
coverage reports for pressure, suffix integrity, cancellation, cache-layout,
trace, and deterministic replay contracts, plus five independent continuation
episodes for facts, stale-state removal, ledger/rework, and cumulative
generations. Its baseline is updated only with an explicit audit reason; see
[`docs/compaction.md`](compaction.md).

## Completion evidence

- The core fixture corpus runs the Rust runner against checked-in canonical
  results.
  It covers streams, tool updates/errors, hooks, queues, observer settlement,
  cancellation, reuse, default profile bytes and tool behavior.
- The core has no Pi CLI/runtime dependency, no ambient configuration/session
  behavior, no Tokio, and no unsafe Rust. Providers and world side effects are
  explicit host ports.
- Deterministic hardening covers lifecycle balance and cleanup, completion
  ordering, profile composition, concurrent run claims, workspace isolation,
  non-blocking observer overflow, and one thousand isolated agents.
- Automatic policy regression coverage (`tests/automatic_policy.rs`) pins
  threshold ordering, zero-usage checkpoint retention, overflow compact/retry,
  and transactional failure behavior. `tests/circuit_breaker.rs` pins fatal and
  repeated retryable capability failures, while `tests/tool_projection.rs`
  pins raw-versus-model-facing result curation. These cases are also the local
  compaction contract surface; they preserve the canonical in-memory transcript
  without importing session-storage semantics.
- The Rust coding tier is provider-opt-in, uses an explicit credential source
  boundary, and writes provider-specific reports outside the repository. The
  controller contract lives in [`evals/README.md`](../evals/README.md).

## Luau extension evidence

- `tea-luau` has unit and integration coverage for deterministic bundle
  paths/hashes, closed relative imports and per-VM caches, typed capability
  manifests/gates (including exact MCP target matching), raw coroutine request
  validation, cancellation/drop of pending host futures, handler host-call
  limits, and policy-bundle loading.
- The adversarial suite verifies the absence of ambient OS/file/package/debug
  authority, immutable globals, source/memory/interrupt containment, loop and
  recursion termination, failure recovery, deterministic declarations, and
  two-policy isolation.
- `cargo +nightly-2026-07-24 run -p tea-luau --example
  v1_luau_benchmark --release` records startup/teardown, hook, and 256-policy
  isolation costs without brittle timing thresholds.
- The exact end-user and host contracts are in
  [`docs/luau-extensions.md`](luau-extensions.md). The lower-level source
  modules are `bundle`, `bundle_runtime`, `capability`, `async_runtime`, and
  `tool_handler` in `tea-luau`.

## Runebench integration evidence

Runebench is an embedding, not part of the transport-free core. Its hard
cutover uses the Rust host, pinned default profile, explicit OpenRouter
adapter, capability-scoped Rust `rs-agent` MCP client, and LuauJIT policy.

On 2026-08-14, the credential-injected `tasks/woodcutting-xp-5m` acceptance completed
cleanly with `completed=1`, `errored=0`, peak **228 XP/min**, **88,750 XP**,
and Woodcutting level **64**. The Rust MCP client loaded its API documentation
and the Luau policy loaded all five declared `rs-agent` tools. The trajectory
had 17 balanced tool starts/ends and one terminal `agent_end`; the only failed
tool result was the expected `Operation aborted` when the host's 390-second
deadline cancelled a still-running game loop. The host owns that structured
deadline, leaving cleanup margin before Harbor's 420-second task limit;
foreground shell, provider, and MCP children are cancellation-aware while
intentionally detached world workers are not reaped by the agent host.

Re-run this acceptance after changing the Runebench host, profile binding,
world policy, or process-cancellation boundary. It is not a substitute for the
deterministic core fixture suite.
