# Evaluation and quality gates

The maintained evaluation surfaces are under `evals/quality`:

- `fast` runs deterministic core cases through the Rust fixture adapter.
- `resources` records Rust allocation and process diagnostics.
- `multiedit-disabled` materializes, runs, and grades the repository-owned
  isolated hidden-runner design eval with an unavailable legacy batch-edit
  capability.
- `prepare-cache` may explicitly fetch the historical pinned Express source
  cache, but it never performs inference.

The core fixture corpus is owned by the crate at
`crates/tea-core/fixtures`. The quality harness lowers its declarative
case manifests to that fixture vocabulary and never invokes a host Pi
executable, reads ambient configuration, or requires an upstream checkout.

Run the provider-free gate with:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m evals.quality fast --out /tmp/pi-quality-fast
```

The former coding and full commands are retired: their Rust adapter no longer
exists, and an arbitrary model identifier cannot satisfy the repository's
guarded Codex verification contract. The live boundary is the feature-gated
Rust guard described in [`docs/verification.md`](../docs/verification.md).
It runs six synthetic scenarios with durable oracles and records each live
result separately from its offline counterpart.

`controller.py` and `baselines.example.json` remain a generic, caller-supplied
multi-baseline controller contract. The checked-in provider-specific manifests
and upstream adapters have been retired because their runner is no longer part
of this repository.
