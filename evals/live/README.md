# Guarded live verification

This directory records only sanitized, reviewable evidence for the optional
real-model verification lane. It contains no credentials, prompts, model
outputs, session logs, source checkouts, or workspaces. Raw reports and the
mutable request-budget ledger belong in an explicit directory outside the
repository.

`zen-free-catalog-evidence.json` is a source-bound record, not a permanent
price assertion. On September 22, 2026, OpenCode's official [Zen
documentation](https://opencode.ai/docs/zen) listed `muse-spark-1.3-contributor-free`
on `https://opencode.ai/zen/v1/responses`, with Free input, output, and cached
read charges per 1M tokens and no cached-write price. The same page says the
Contributor Free route permits using prompts and completions to train future
Meta models. It may only receive disposable synthetic or deliberately public
fixtures.

Refresh and review this record from the official document on the current UTC
date before every live suite. If the identifier, endpoint, any charge,
availability, or data-use term is uncertain, do not substitute another model:
mark the live suite `BLOCKED`.
The feature-gated `tea_agent::verification::RestrictedZenFactory` accepts only
the exact fields above, limits an aggregate suite to 40 attempts, 100,000
requested output tokens, 30 minutes, and two concurrent streams, and persists
each reservation before it calls provider transport.

The Rust verification example can execute the six required offline counterparts
with `--run-counterparts`; its headless fixtures execute the repository's real
synthetic fixture runner and compare the result with checked-in oracles, while
its coding case uses the real coding-capability tests. With an explicit
`--live` acknowledgement, credential, temporary home, and temporary workspace,
the example injects guarded consumers into a feature-only headless terminal
harness. It executes a synthetic workspace edit plus host-side exit oracle, an
exact one-shot completion oracle, and a controlled provider-admission
cancellation followed by passive reopen and a fresh exact continuation. It
records only booleans and bounded state, never model output.

The suite is deliberately not a whole semantic live pass. Forced compaction,
Luau activation/rollback, and isolated-child cases remain `BLOCKED` without
transport until dedicated drivers exercise their real runtime boundaries. Do
not claim a live pass from a scripted provider, a role-labelled root prompt, or
a partial report.
