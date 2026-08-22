# Prompt cache-friendliness

Prompt cache behavior has two different evidence levels:

1. `measure_prompt_cacheability` compares adjacent `ModelRequest` values at the core boundary.
   It records system-prompt, ordered tool-definition, and converted-context bytes, plus the
   longest common context prefix. This is a deterministic cacheability proxy.
2. Provider usage may report `cache_read_tokens` and `cache_write_tokens`. Those fields are the
   only evidence treated as an actual provider cache hit or write. A proxy prefix must never be
   presented as a hit.

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
