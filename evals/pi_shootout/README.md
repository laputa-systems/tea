# Pi shootout

This is a narrowly pinned, repeated comparison of the Pi and Tea coding
harnesses on the checked-in Express cases `express-3936-medium` and
`express-4205-hard`. It is not a broad benchmark or a provider comparison.
Both adapters use OpenRouter with
`deepseek/deepseek-v4-flash-0731`, high reasoning, unlimited output, the
ordered `read`, `bash`, `edit`, `find` capability set, the same isolated
baseline, and the same fast validator.

The medium case uses a 900-second per-attempt wall-clock limit. The harder case
uses a 1,800-second limit to leave headroom for its longer coding trajectories.
Pass `PI_SHOOTOUT_TIMEOUT_SECONDS=0` for an explicitly uncapped diagnostic
run only; an uncapped run is not a scoring result because model-generated shell
commands can themselves wait indefinitely. Tea aligns its provider request
timeout with the finite task budget (and uses a 24-hour transport guard for
the zero-budget diagnostic).

`make pi-shootout-check` is provider-free: it checks the result contract,
direct-request instrumentation, Pi SDK embedding, Tea's OpenRouter payload,
durable-session attribution, and the offline validator setup. It does not make
a model request.

## Run classes

Three repeats are a smoke/diagnostic workflow:

```sh
vault OPENROUTER_API_KEY -- make pi-shootout-static
```

The named serious workflow uses seven counterbalanced repeats:

```sh
vault OPENROUTER_API_KEY -- make pi-shootout-static-serious
```

Use `make pi-shootout` or `make pi-shootout-serious` only when the Tea JIT
condition is also in scope. Static commands run only `pi-static` and
`tea-static`.

To establish a Tea-only hard-case baseline, set the task and run the dedicated
target:

```sh
PI_SHOOTOUT_TASK=express-4205-hard \
PI_SHOOTOUT_REPEATS=1 \
PI_SHOOTOUT_PARALLEL_REPEATS=1 \
PI_SHOOTOUT_OUT=/tmp/tea-pi-shootout-hard-tea-baseline \
vault OPENROUTER_API_KEY -- make pi-shootout-tea-static
```

This writes a single-baseline `reports/tea-static.md`; use the attempt
`record.json` and `surface/` evidence as the authoritative result. It does not
produce a paired comparison report.

Repeats are safely parallel lanes by default: `PI_SHOOTOUT_REPEATS=2` starts
two complete repeats at once, while the counterbalanced Pi/Tea condition order
within each lane remains sequential. Set `PI_SHOOTOUT_PARALLEL_REPEATS=1` to
serialize lanes, or another value from one through the repeat count to bound
concurrency. Every lane gets fresh detached worktrees, evidence directories,
dependency trees, tool npm cache, HOME, and TMPDIR. The only shared setup
inputs are synchronized bare-repository and pre-populated npm content caches;
the short npm consumption step is locked before each lane receives its private
module tree.

Each attempt starts from a clean detached Express checkout. The historical
baselines have no lockfile, so each case carries a checked-in production-only
`package-lock.json`; explicit cache preparation may fetch its tarballs, while
scoring installs a fresh per-attempt dependency tree with `npm ci --offline`.
That tree is exposed through the controlled tool environment, never added to
the Git worktree, and its lock/module manifest is recorded with the attempt.

## Evidence and conclusions

The adapters retain `surface/wire-requests.json`, captured at the direct final
OpenRouter boundary before credentials are attached. It is the request-ground
truth: it contains sanitized canonical requests, exact ordered tool schemas,
model-affecting fields, and any observed OpenRouter route headers. The result
schema's normalized wire summary is derived from that evidence; it is not a
replacement for it.

After a run, render the provider-free analysis:

```sh
python3 -m evals.pi_shootout compare \
  --run-dir /tmp/tea-pi-shootout/runs/<run-id>
```

It writes `reports/comparison.json` and `reports/comparison.md`, with paired
Tea-minus-Pi observations and deterministic bootstrap intervals. It separates:

- controlled-condition mismatches, wire-shape bugs, and conflicting observed
  provider routes, which block a strict efficiency conclusion;
- native prompt/tool-schema/execution differences, which are reported as
  measured harness results rather than treated as parity gates; and
- unavailable observability, including any route or timeout field an adapter
  cannot honestly observe.

An analysis is only strict when required controls agree, direct wire evidence
is valid, observed routes do not conflict, and no required observation remains
unknown. The static adapters set the same zero temperature and fixed seed;
provider-default sampling is retained only for non-shootout reporter tests.

The adapter process receives `OPENROUTER_API_KEY` only through `vault`. Pi
clears inherited environment before its session and passes the explicit shell
allowlist to its bash tool. Tea receives that same allowlist. Neither harness
has a web-search, browser, or subagent tool; `curl` is available only through
the ordinary `bash` capability.
