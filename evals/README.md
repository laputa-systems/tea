# Evaluation and quality gates

The maintained evaluation surfaces are under `evals/quality`:

- `fast` runs deterministic core cases through the Rust fixture adapter.
- `resources` records Rust allocation and process diagnostics.
- `multiedit-disabled` materializes, runs, and grades the repository-owned
  isolated hidden-runner design eval with Tea v2 multiedit unavailable to the
  candidate.
- `coding` is an explicit provider-opt-in check for the pinned Express cases.

The core fixture corpus is owned by the crate at
`crates/tea-core/fixtures`. The quality harness lowers its declarative
case manifests to that fixture vocabulary and never invokes a host Pi
executable, reads ambient configuration, or requires an upstream checkout.

Run the provider-free gate with:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m evals.quality fast --out /tmp/pi-quality-fast
```

The coding tier requires an explicit model, environment file, cache root, and
workspace root. See [`quality/README.md`](quality/README.md) for its setup and
scope.

[`pi_shootout`](pi_shootout/README.md) is a separate explicit one-task Pi SDK
versus durable Tea harness experiment. It is provider-opt-in, uses a caller
visible `vault OPENROUTER_API_KEY --` boundary, and is not part of the normal
quality suite or CI.

`controller.py` and `baselines.example.json` remain a generic, caller-supplied
multi-baseline controller contract. The checked-in provider-specific manifests
and upstream adapters have been retired because their runner is no longer part
of this repository.
