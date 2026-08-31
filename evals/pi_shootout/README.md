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
module tree. If a run controller is interrupted, it terminates only the
interrupted attempt's adapter group and its nested model-tool groups. Nested
groups are stopped before the adapter is allowed to finalize, so a canceled
lane cannot leave a shell command or adapter process running beside other
lanes.

For an intentional early stop of a Tea-only diagnostic, do not send a raw
signal to the adapter. The runner prints its evidence directory as soon as it
creates it; request one keyed controller stop instead:

```sh
python3 -m evals.pi_shootout stop \
  --attempt-dir /tmp/tea-pi-shootout/runs/<run-id>/attempts/<attempt-dir> \
  --reason diagnostic-bounded
```

The controller validates the target identity, performs the ordinary
attempt-group TERM/KILL cleanup, writes `exclusion.json`, and retains that
lane under `excluded_lanes` rather than `attempts`. Raw signals and malformed
requests remain infrastructure failures. The command is rejected for paired
runs, and excluded lanes never participate in reports, pairs, or efficiency
gates.

On macOS, an explicitly Tea-only diagnostic may also pass
`--tool-child-sandbox macos-seatbelt-v1` or `macos-seatbelt-v2`. Each wraps
Tea `bash` children and their descendants in a fail-closed Seatbelt profile:
the profile permits the attempt workspace, private attempt directories, and
fixed OS toolchain read roots while blocking other data paths and outbound
network access. V2 additionally blocks reads and writes beneath the
workspace’s `.git` directory, so history commands fail while ordinary
workspace directory listing and source work continue. The networked provider
adapter stays outside the child boundary. This is a tool-child
sandbox, not a complete attempt sandbox; it is deliberately rejected for
paired Pi/Tea comparisons until Pi has an identical policy. The mode and
profile hash are retained in Tea’s surface evidence.

An explicitly Tea-only invalid-edit diagnostic may additionally pass
`--edit-recovery-projection canonical-v1`. When the immediately preceding
assistant turn has a rejected `edit` call with the known top-level
`path`/`edits` envelope, Tea preserves that raw rejected call and schema error
in durable state, then appends one canonical-envelope reminder only to the
latest matching tool result in the cloned next provider context. It never
accepts, normalizes, or rewrites the model arguments. This model-visible
continuation policy changes token use, so it is rejected for paired
comparisons; its mode, correction hash, and distinct hook-backed profile are
retained in Tea evidence.

Static prompt composition is also explicit: `--static-prompt-profile` accepts
the default `builtin-v1`, `no-history-v1`, `prefix-guard-v1`, or
`prefix-guard-focused-v1`. The `no-history-v1` profile replaces only the
generic Bash section’s Git/history invitation with workspace-local build and
validation guidance. The `prefix-guard-v1` diagnostic retains that replacement
and appends an explicit RegExp mount-prefix semantic guard derived from the
task’s observed validator failure. The focused variant further requires an
`index.js`-only guard at the existing trim boundary and forbids repro-file and
matching-internal edits, so it is an intentionally task-specific control.
Both prefix-guard profiles are Tea-only and never paired evidence. No profile
changes Tea’s ordered `read`, `bash`, `edit`, `find` definitions or authority.
The selected profile, projected Bash-section hash, and full system-prompt hash
are retained with the attempt.

`--pre-edit-tool-gate direct-edit-v1` is an explicit fresh-static workflow
condition. It requires `--static-only` (and therefore never invokes
`tea-jit`). Pi and Tea retain the same ordered `read`, `bash`, `edit`, `find`
definitions and apply the same model-visible policy: before a *prior*
successful `edit` result, `bash` and `find` are unavailable while `read` and
`edit` remain available; a successful edit opens focused validation. A
same-batch edit does not open a sibling shell call, and failed edits do not
open either blocked tool. The policy never rewrites model arguments. Both
adapters record the same mode, blocked-tool order, unlock rule, same-batch
rule, and block-reason hash under `surface.pre_edit_tool_gate` and
`effective_policy.controlled.pre_edit_tool_gate`; comparison rejects an
adapter-to-adapter or run-metadata mismatch. Its equivalence is scoped to
fresh static shootout attempts, not resumable sessions. A Tea-only invocation
is still single-baseline diagnostic evidence because it has no Pi counterpart.

`--pre-edit-tool-gate source-local-v1` is a distinct generic fresh static
*paired* condition: it requires `--static-only`, rejects `--tea-only`, and
never invokes `tea-jit`. Before a successful target-local edit, `bash` and
`find` are blocked and `read`/`edit` are allowed only for paths in the task's
checked-in `source_local_v1` declaration
(`tea-coding-eval-source-local/v1`). The runner verifies every declared target
appears literally in the task prompt and is a regular file in the clean
baseline worktree. Both adapters return the same generic policy error for
every blocked pre-edit call, retain ordered target paths in both policy
surfaces and run metadata, and unlock only when a successful result carries
the ID of an admitted target-local edit. Calls in that edit's batch remain
pre-edit. Pi observes arguments without mutating them; Tea derives the same
relationship from durable assistant and tool-result context.

`--post-edit-validation-gate unmasked-evidence-v1` is a second, shared
fresh-static workflow condition. It requires the paired `pi-static` and
`tea-static` run and `--pre-edit-tool-gate source-local-v1`; it rejects
Tea-only and JIT runs. After each successful declared-target native `edit`,
only a later direct foreground `bash` child with the content-free
`"exited-zero"` process witness qualifies. Generic tool success is neither
recorded as nor sufficient for that witness. Pipelines and status-suppression
wrappers do not qualify, a same-batch shell call is too early, and any later
successful native `edit` result (including a non-target edit after the
source-local prerequisite) resets prior evidence. Bash filesystem effects do
not reset the condition. At most one completion reminder is issued when the
latest edit has no qualifying evidence. Both adapters retain the identical
policy object in `surface.post_edit_validation_gate` and
`effective_policy.controlled.post_edit_validation_gate`, plus content-free
`validation_evidence` at result root. This condition does not identify,
invoke, or expose the host validator, and it does not prove that the chosen
workspace-local check was the right test. Comparison checks the shared policy
and run metadata, not matching outcome evidence or native-harness parity.

Fresh adapters emit `tea-coding-eval-result/v4`, which requires these roots
even when the gate mode is `none`. The comparison reader retains a read-only
compatibility path for complete enriched-v3 artifacts and for wholly legacy v3
artifacts that predate every post-edit root. Legacy analysis records the
migration and treats the absent witness as unknown, so it cannot support a
strict efficiency conclusion. Partial or mixed v3/v4 artifacts are rejected;
the reader never defaults a fresh artifact that omits required evidence.

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
model-affecting fields, explicit adapter timeout controls, and any observed
OpenRouter route values. Each adapter retains only
`x-openrouter-provider` and `x-openrouter-model`, never arbitrary response
headers; missing values remain unknown. The result
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
- unavailable observability, including any route or timeout policy an adapter
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
