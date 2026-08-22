# Compaction fixture inventory

The provider-free compaction lane has two complementary evidence surfaces.
Neither makes a provider call.

`evals/quality/compaction.py` writes 70 named coverage rows. Each row names
the focused Rust contract that owns the behavior: automatic threshold and
overflow handling, tool-pair boundaries, lifecycle/trace compatibility,
request-domain observation, or transactional cancellation. The report records
the exact target result instead of inventing an independent model run.

`evals/quality/cases/compaction/continuation.json` contains five independently
executed continuation episodes:

- five successive checkpoint generations with latest-wins constraints;
- stale-decision removal;
- unchanged rereads and failed-command repetition versus retry after an edit;
- retained-suffix/split-turn continuation;
- failed provider attempt and verified follow-up state.

Each episode defines stable fact IDs, critical facts, forbidden obsolete facts,
ledger count, a scripted continuation, headroom, and next-compaction distance.
`evals/quality/continuation.py` computes the raw facts/rework metrics and
writes one content-free artifact per episode. It validates the evaluator,
checkpoint merge contract, and ledger classification; it does not measure the
semantic quality of a model-generated checkpoint.

Run both surfaces with:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m evals.quality compaction --out /tmp/tea-compaction-quality
```

The summary distinguishes `scenario_count`, `executed_target_count`, and
`fixture_case_count`. A scenario row is never mislabeled as a provider call or
as a measured compaction attempt.

`baseline.json` additionally pins the baseline strategy/schema, its checked-in
prompt fingerprint, policy/estimator description, trace schema, fixture corpus
version, source commit, and expected evidence gaps. The explicit update command
requires a reason and preserves that reviewed manifest rather than fabricating
new provenance from a test run.
