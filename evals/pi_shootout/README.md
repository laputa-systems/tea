# Pi shootout

`make pi-shootout-plan` validates the fixed v0 configuration without a model
request. `make pi-shootout-check` runs only provider-free Python, TypeScript,
Rust, lifecycle, and oracle-isolation checks.

The live command is deliberately explicit:

```sh
vault OPENROUTER_API_KEY -- make pi-shootout
```

For the efficiency mission, use the static-only run:

```sh
vault OPENROUTER_API_KEY -- make pi-shootout-static
```

It runs only `tea-static` and `pi-static`; it never instantiates `tea-jit`.

It runs one oracle-isolated `express-3936-medium` workspace under `pi-static`,
`tea-static`, and `tea-jit` in seeded sequential order. v0 uses exactly
OpenRouter, `deepseek/deepseek-v4-flash-0731`, high thinking, no output-token
ceiling, and the same 900-second attempt timeout. The model ID is fixed for
this v0 run and is not silently substituted.

Both adapters receive the repository's closed `read`, `bash`, `edit`, `find`
coding-bundle contract. Their implementations remain intentionally different;
the shared bundle records the comparable model-facing surface.

Each adapter publishes `tea-coding-eval-result/v2`. The Python runner keeps
the complete patch and bounded process logs, runs the same external fast
validator, and writes `reports/static.md` and `reports/evolution.md` below a
unique `<out>/runs/<run-id>/` evidence directory. A valid failed model run is
benchmark data and still produces reports; a missing/invalid adapter result is
an infrastructure failure.

The provider key is introduced by `vault` only for an adapter process. Pi
removes it before creating its coding tools; Tea receives an explicit shell
allowlist. Both tool shells have `curl` through `bash`, but not a provider key,
web-search tool, browser, or subagent tool.
