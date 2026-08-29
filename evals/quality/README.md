# Quality evaluation suite

This directory contains provider-free deterministic fixture checks and an
opt-in ecological coding check. The core gate executes the Rust fixture
runner; it does not require an upstream checkout, provider credentials, or a
recorded replay artifact.

## Deterministic core gate

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m evals.quality fast --out /tmp/pi-quality-fast
```

Each case manifest is lowered to the closed fixture vocabulary and run by
`crates/tea-core/src/bin/tea-fixtures.rs`. Artifacts retain the
manifest, fixture, Rust response, canonical trace, metrics, and process
diagnostics. Source fixtures live under
`crates/tea-core/fixtures/declarative/`.

## Resource diagnostics

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m evals.quality resources --out /tmp/pi-quality-resources.json
```

## Compaction contract gate

The provider-free compaction lane runs 70 named pressure, recovery, lifecycle,
cache-layout, and trace-compatibility coverage rows against focused Rust tests,
plus five independently executed continuation episodes. The episodes assert
critical-fact survival, latest-wins obsolete-state removal, retained-suffix
behavior, rework classification, headroom, and next-compaction distance. They
are deterministic evaluator evidence, not a claim about a provider's
semantic-summary skill. The lane has no network, provider, credential, or
ambient cache dependency and writes content-free artifacts:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m evals.quality compaction --out /tmp/tea-compaction-quality
```

The checked-in baseline guards the scenario contract, not a model-quality
score. Replacing it requires an audit reason:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m evals.quality compaction \
  --out /tmp/tea-compaction-quality --update-baseline \
  --reason "reviewed contract change"
```

The resource probe uses the Rust-only
`rustybench::AllocProfiler` benchmark from
`crates/tea-core/benches/quality_memory.rs`. Allocation and timing values
are diagnostic and do not gate fixture results.

## Disabled-tool multiedit design eval

`python3 -m evals.quality multiedit-disabled --out <dir>` materializes a
hermetic public task whose capability envelope contains only `read`, `bash`,
legacy `edit`, and `write`; the proposed batch capability is unavailable. The
copied task contains no reference schema or grader. To run a candidate through
the repository-owned runner instead of supplying a record:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m evals.quality multiedit-disabled \
  --out /tmp/tea-multiedit-disabled --run --command /path/to/candidate
```

For the ordinary hidden workspace and each stale/overlap/escape/non-regular,
fault-recovery, and cancellation phase, the runner starts the candidate with a
fresh workspace. It passes only `TEA_MULTIEDIT_TASK`,
`TEA_MULTIEDIT_WORKSPACE`, `TEA_MULTIEDIT_PHASE`, and
`TEA_MULTIEDIT_RESULT`; the result path must contain a JSON object naming the
same phase. The runner owns the task copy, sanitized process environment,
timeout/process-group cleanup, bounded output capture, initial files, workspace
snapshots, adversarial observations, design-document rubric, and efficiency
measurements. It writes `record.json` and `grade.json`; candidates never write
either artifact or receive the hidden-case path.

The runner deliberately does not claim an operating-system network sandbox: it
passes no credentials or ambient home configuration, and a production CI
deployment that needs network isolation must place this process in its own OS
sandbox. The checked-in runner is still the authority for the evaluation record,
not `evidence.json` or any candidate-supplied semantic boolean.

The grade remains 70 correctness / 15 design-and-proof / 15 efficiency. The
vector includes runner-observed phase count, wall-clock, bounded output, remote
round trips, and public-task context bytes. A lower invocation count cannot
compensate for stale, partial, escaping, overlapping, recovery, or cancellation
failures, and a candidate must meet the contract/proof/limitations threshold
independently of its efficiency score.

The runner ID and case digest are mix-up guards, not cryptographic
authentication. The evaluator must create the record outside the candidate
workspace and must never pass the candidate's `evidence.json` directly to the
grader.

## Live coding gate

The coding tier is an explicit provider-opt-in check for three pinned
`pi-bench` Express tasks. It runs the Rust coding adapter and the selected
validator from a fresh detached worktree; no upstream comparison or ambient
repository discovery is performed.

Populate the exact bare-repository cache first:

```sh
python3 -m evals.quality prepare-cache --cache-root /tmp/pi-quality-cache
```

Then provide an explicit model and env file:

```sh
python3 -m evals.quality coding --allow-provider \
  --model poolside/laguna-xs-2.1:free \
  --env-file .env \
  --cache-root /tmp/pi-quality-cache \
  --workspace-root /tmp/pi-quality-workspaces \
  --out /tmp/pi-quality-coding \
  --validator fast
```

The Rust adapter resolves the four checked-in Luau coding builtins under
`crates/tea-luau/builtins/{read,bash,edit,find}/`. Worktrees are removed after
each attempt and provider credentials are sourced only at the final adapter
process boundary.
