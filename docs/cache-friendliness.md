# Prompt cache-friendliness

Prompt cache behavior has two different evidence levels:

1. `measure_prompt_cacheability` compares adjacent `ModelRequest` values at the core boundary.
   It records system-prompt, ordered tool-definition, and converted-context bytes, plus the
   longest common context prefix. This is a deterministic cacheability proxy.
2. Provider usage may report `cache_read_tokens` and `cache_write_tokens`. Those fields are the
   only evidence treated as an actual provider cache hit or write. A proxy prefix must never be
   presented as a hit.

Each live `SessionRuntime` shares one volatile `PromptLayoutLedger` across the fresh core agents
created for successive operations. The ledger compares the exact `ModelRequest` after context
projection and hooks, immediately before the provider effect. Its predecessor is process-local
and is never persisted; the `PromptLayoutObserved` lifecycle event contains only fingerprints,
lengths, joined prefix evidence, changed components, and continuity classification. The first
request is explicitly unavailable (`FirstRequest` with no prefix), while an append is
`ExactExtension`; an annotation or projection that changes an earlier byte is `Rebased`. A
provider/model/tool-domain transition is separately classified as `DomainChanged`.
Reopening a runtime creates a fresh volatile ledger, so the first request after reopen is again
unavailable rather than being joined to a prior process lifetime.

The Luau policy plane cannot write, clear, or replace this ledger. Candidate hooks run first, and
the kernel measures their final provider-facing result at the same request boundary used for
dispatch. A self-modification that changes prompt-visible tools or instructions is therefore a
`DomainChanged` transition; one that rewrites an earlier converted-context byte is `Rebased` or
`Discontinuous`. Candidate code cannot suppress the lifecycle observation or opt itself out of a
host-selected rejection policy.

The ledger's scope is an opaque equality-only serving/cache scope. A scope or harness change is
represented by changed component/domain evidence, not by inferring provider cache behavior. The
initial policy is observe-only (`PromptLayoutPolicy::Observe`): hosts may warn on
`Rebased`/`Discontinuous` evidence while provider usage remains the authority for hit/write
claims. A host that needs same-domain layout enforcement can opt into
`PromptLayoutPolicy::RejectUnexpectedRebase`; it rejects only `Rebased` and `Discontinuous`
requests before provider dispatch. A stricter `PromptLayoutPolicy::RequireExactExtension` also
rejects `DomainChanged` after the first request, while still allowing the first request and
exact append-only extensions. Enforcement compares complete core-owned prompt, tool, model, and
thinking components; compact fingerprints are telemetry only and are never trusted as equality
proof.

The baseline fixture in `crates/tea-core/tests/cache_friendliness.rs` drives three text
turns through the real run loop and prints the measurements. On the current pinned profile it
reports a stable prompt domain and a 100% common context prefix for both adjacent turns:

```text
cache baseline: requests=3, context_bytes=[48, 220, 391], common_prefix_bytes=[48, 220], ratios_ppm=[1000000, 1000000]
```

That result is encouraging for normal append-only turns, but it says nothing about compaction.
The compaction path is a separate prompt domain: its summary request must either preserve the
active provider context prefix or explicitly fall back to a standalone request when the source
does not fit. The TUI's automatic compactor now receives the exact provider-visible context built
by the core projection and hook pipeline. It appends one summary instruction to that context only
when the context, reserve, and a 4096-token safety margin fit the configured budget; otherwise it
uses the standalone summary prompt.

Run the focused baseline with:

```bash
rustup run nightly-2026-07-24 cargo test -p tea-core --test cache_friendliness -- --nocapture
```

The measurement intentionally excludes provider-native envelopes and tokenizer-specific token
counts. Adapters should pair it with their own payload capture and reported cache usage before
claiming a cost or latency improvement.
