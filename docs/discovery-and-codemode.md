# Deferred discovery and Luau codemode

Both features are optional. The default terminal experience keeps its ordinary
coding tools declared directly; nothing here replaces a direct call.

## Three sets of tools

| Set | Meaning | Where it lives |
| --- | --- | --- |
| **Authorized** | The run's executable registry. Nothing else can execute. | `AgentConfiguration::tools` plus resolved extension and host tools |
| **Discoverable** | Authorized tools with `ToolExposure::Deferred`: declared only after discovery loads them. `ToolExposure::Composition` tools are never declared and are callable only from composition tools. | `AgentTool::exposure` |
| **Declared** | What the model sees in a request: direct tools plus loaded deferred tools. | Transcript `System` configuration entries |

Discovery never grants authority. A tool result can name tools in
`added_tool_names`; the run declares only authorized `Deferred` tools from that
list, records the change as a configuration entry in transcript order, and
replays it on resume and in forks. The model cannot call an undeclared deferred
or composition tool directly.

## `tool_search`

`tea_core::tool_search` ports Pi's `extensions/tool-search/tool.ts` (Pi
`a13d35a742c6`): the same tokenizer (camelCase/snake_case splitting, stop
words, naive plural folding) and Okapi BM25 ranker (`k1 = 1.2`, `b = 0.75`)
over tool names, descriptions, and schema descriptions and property names.
`ToolSearchTool` searches undeclared deferred tools and loads the matches for
the next request. Its description never lists tools, so it stays stable while
optional tools (for example MCP tools) appear, keeping the request prefix
cacheable. It is not callable from scripts.

Ported tests: `tokenize`, BM25 relevance, limits, and unknown queries
(`cargo test -p tea-core --all-features tool_search`). Pi's namespace and
budgeted codemode catalog tests are not ported because tea has no namespaces
and codemode lists no tools.

## Composition facility (core)

A trusted tool opts in with `AgentTool::composition_access`:

- `Catalog`: receives `ToolContext::composition` with the authorized catalog
  and the current declarations (`tool_search` uses this);
- `Calls`: may also call other tools with `ToolComposition::call`.

Nested calls are serviced by the owning run while the composition tool
executes. Each goes through the same preparation as a model call: registry
lookup, JSON parsing, before-tool hooks, schema validation, an effect gate
`NestedToolExecution { parent_tool_call_id, call }`, cancellation, and
after-tool hooks. Results return to the composition tool and never join the
transcript; nested results cannot load tools for the model. Sequential tools
run alone; parallel tools may overlap. When the composition tool settles,
requests that never started fail without effects and started calls run to
settlement first, so nothing is detached. Composition tools and tools marked
`script_callable() == false` cannot be called from scripts, which rules out
recursion.

Durable sessions record `tea.nested-tool-effect.v1` custom facts: `started`
(with bounded arguments) before execution and `settled` (with `is_error` and
bounded content) afterwards. A started fact without settlement marks a call
interrupted by a crash. Intermediate results therefore stay out of the next
prompt without disappearing from durable evidence.

## Luau codemode

`tea_luau::codemode::CodemodeTool` adapts Pi's codemode concept to tea's
sandboxed Luau runtime (not QuickJS). The tool input is
`{"script": "<luau>"}`. Script globals:

| Global | Purpose |
| --- | --- |
| `call(name, args)` | Call a tool; returns its text or raises its error |
| `try_call(name, args)` | Returns `ok, text` |
| `parallel({{name, args}, ...})` | Concurrent calls; returns `{ok=, text=}` per call |
| `tools.<name>(args)` | Shorthand for `call` |
| `search_tools(query, limit)` | BM25 search over callable tools |
| `describe_tool(name)` | `{name, description, schema}` or `nil` |
| `print(...)`, top-level `return` | Output |
| `json.encode`, `json.decode` | JSON helpers |

There is no file, process, network, module loader, or extension-host access;
the only effects are tool calls through the composition facility. Limits (all
configurable through `CodemodeLimits`): 32 KiB source, 32 MiB VM memory, an
instruction budget between tool calls, 64 calls, 8 calls per `parallel`, and
16 KiB of output (longer output keeps its start and end).

Results start with `Script completed after N tool calls (M failed).` or
`Script failed after N tool calls (M failed). Calls that ran are not undone.`,
keep partial output, and end with `Script error:` on failure. Cancelling the run
cancels the script; started nested calls still settle.

Deliberately omitted from Pi's codemode: `store`/`load` persistent script
state, `models` (classifier and image models), images, the budgeted tool
catalog in the description, and `codemode.mode = only`.

### Enabling codemode in `tea`

```toml
# $TEA_HOME/config.toml
[features]
codemode = true
```

Library hosts insert `CodemodeTool::new(&listed, CodemodeLimits::default())`
into their registry; `listed` may name tools to describe (deferred tools are
best left out so the description stays stable).

## Evidence

```sh
cargo test -p tea-core --all-features --test composition
cargo test -p tea-core --all-features tool_search
cargo test -p tea-luau codemode
cargo test -p tea-agent --lib durable_codemode
```
