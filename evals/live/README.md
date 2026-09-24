# Guarded live verification

This directory holds sanitized model-selection evidence only. Raw reports,
request ledgers, credentials, workspaces, prompts, and model outputs belong
outside the repository.

`codex-luna-model-evidence.json` pins the deliberate `codex/gpt-5.6-luna` model,
low reasoning effort, and the Tea Codex subscription endpoint. It was checked
against [official OpenAI Codex model documentation](https://learn.chatgpt.com/docs/models)
on 2026-09-24 UTC. Availability depends on the account, rollout, and client;
the evidence record does not assert that the model is enabled for Tea's honest
originator. Refresh the record on the UTC date of a live run.

`tea_agent::verification::RestrictedCodexFactory` accepts an explicit
installed Codex client `auth.json` or Tea-owned `auth/codex.json` path. The
client path is read-only and its access token is reloaded for each request;
Tea-owned credentials may refresh. No ambient API key or model fallback is
used. Every request must use the exact descriptor and low reasoning effort.
The aggregate ledger records every request before transport and retains
attempts across continued runs. It imposes no request-count, wall-time, or
concurrent-stream limit. Codex subscription transport has no reliable
output-token request cap, so the ledger does not claim one.
The v2 ledger tags each attempt with its exact model and retains rejected
`gpt-6-luna` attempts in the same aggregate ledger after the
user-selected switch to `gpt-5.6-luna`.

`codex-luna-verification` runs six synthetic live scenarios and six offline
counterparts. Every live result is checked through a durable oracle. All six
pairs must pass for the suite to pass; missing evidence remains `BLOCKED`, and
failed evidence is `FAILED`. The offline command and credential setup are in
[`docs/verification.md`](../../docs/verification.md).
