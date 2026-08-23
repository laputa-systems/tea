# Terminal host

The tea terminal is a presentation layer over one managed DurableHarness. It
does not own a transcript format, an extension registry, or a second agent
loop.

## Startup

The terminal accepts explicit workspace, provider, model, thinking, compaction,
and Tea-home options. It creates a fresh durable session only when it needs one
and writes the initial model, thinking, harness catalog, snapshot, and revision
before a prompt can start.

A one-shot prompt uses the same durable harness boundary as an interactive
prompt:

~~~text
tea --provider <provider> --model <model> --prompt <text>
~~~

## Sessions

Sessions live below the explicit Tea home, scoped by a normalized workspace
identity. Each is a v1 session directory with its colocated object store.

- /session opens the durable session picker.
- /resume reopens a selected durable session.
- /new creates a new durable session.
- /model selects the provider/model; the footer continuously shows context, cost, and token accounting.
- /thinking selects the reasoning effort for future prompts.
- /steer and /followup queue explicit input for an active durable operation.
- /quit exits the terminal.

Normal composer input always starts or continues the managed harness. During an
active operation, the terminal projects durable session and live harness events
into the transcript without owning their state.

## Presentation boundary

AppState, render, and the terminal decoder are local UI code. They may be
rebuilt at any time from the durable snapshot and live event subscription.
They cannot modify an operation, synthesize a completed effect, or change a
harness revision.

The fixed footer keeps the `{provider}/{model} · effort <level>` identity on
its own wrapped line, followed by calm context and usage stats. Transient notices
occupy subsequent wrapped rows; restoration and setup errors use the error color.
A saved provider/model identity remains visible even when
the provider cannot currently be configured (for example, when its API key is
missing), while prompting still requires a configured provider.
