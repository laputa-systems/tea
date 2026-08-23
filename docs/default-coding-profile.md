# Pinned default coding profile

`PiDefaultCodingProfile` is the core-owned v1 profile that reproduces the selected Pi coding-agent
inputs while keeping all authority replaceable. It is not a port of `pi-coding-agent` and it does
not load resources, sessions, settings, skills, extensions, or ambient configuration.

## Profile contract

`PiDefaultCodingProfile::pinned_default()` loads the captured prompt and definitions without
opening a workspace. `DefaultCodingTools` accepts the explicit workspace root and operation
adapter. The capture fixes `/fixture/workspace` only for reproducibility;
`system_prompt_for_workspace` substitutes that one declared placeholder with the caller's already
canonicalized workspace. For the ergonomic combined path, use
`PiDefaultCodingProfile::pinned_default()?`; compose its prompt and validated tool registry into
`Agent::builder()` rather than using a coding-specific builder shortcut. Follow that composition with `.tool(...)` to
replace/add a capability or `.remove_tool(...)` to omit one before `.build()`. Replacing a tool
under the same name preserves the captured prompt contract. If a caller removes or adds a
prompt-visible default capability, it must also supply a corresponding explicit system prompt;
the pinned bytes intentionally describe exactly the captured default active set. The builder
returns an agent configured with:

```text
ordered active AgentTool definitions
ordered system prompt bytes
profile version/pin metadata
```

The profile may be omitted entirely. A sterile profile can provide no tools and a caller can
replace `read`, `bash`, `edit`, `write`, `grep`, `find`, or `ls` independently. The runtime never
knows whether an operation is local, remote, sandboxed, logged, denied, or virtual.

The executable v1 factories live in `tea_core::coding::tools::DefaultCodingTools`. Constructing
one requires an existing workspace directory; the constructor canonicalizes that directory and
every operation rejects lexical, canonical, and symlink escapes. `coding_tools()` returns the
captured active order (`read`, `bash`, `edit`, `write`), `all_tools()` returns all seven pinned
factories, and `registry()` produces a replacement-friendly `ToolRegistry`. The local adapter is
`LocalCodingOperations`, while remote, VM, policy, or test hosts implement `CodingOperations` and
pass it to `with_operations`. The standard shell adapter starts with an empty environment; a host
must explicitly choose `CommandEnvironment::inherited()` or add variables. No factory consults
ambient cwd, home, `.pi`, sessions, credentials, or resource discovery.

## Core-owned profile fixture

The following hashes identify the source material used to produce the core-owned profile fixture.
They are review information only; runtime code and verification do not require an external source
checkout.

| Artifact | Source symbol | SHA-256 of source bytes |
| --- | --- | --- |
| `packages/coding-agent/src/core/system-prompt.ts` | `buildSystemPrompt`, `BuildSystemPromptOptions` | `e8b06a0f093c83fd7660a3eae331d5f8ee917702763b1e5044f5c5306e9c1d00` |
| `packages/coding-agent/src/core/tools/index.ts` | `createCodingTools`, `createCodingToolDefinitions`, `createAllTools` | `afeb406e0f5edde143aaac51b9c928cb497bfb8216c04ac9242bfd0f9241ffe1` |
| `core/tools/read.ts` | `createReadTool`, `createReadToolDefinition`, `readToolSystemPromptContribution` | `1020a1a8237ed20fae100b3d1602004ffeb7170f588f04a4dfa6ec6834a2d85b` |
| `core/tools/bash.ts` | `createBashTool`, `createBashToolDefinition`, `bashToolSystemPromptContribution` | `fda5085d875558be189f852de012301d2067ef919fb97149baab546a2c80e65d` |
| `core/tools/edit.ts` | `createEditTool`, `createEditToolDefinition`, `editToolSystemPromptContribution` | `3c66e14da0990b5f4f9b783747a31ee6dc3fc00f468e7b81b14de4cc5c1d000a` |
| `core/tools/write.ts` | `createWriteTool`, `createWriteToolDefinition`, `writeToolSystemPromptContribution` | `bf5ec70a331cb7f6a5f4078468bf5e49b322ea65d916c7ce30ddc1540d89b720` |
| `core/tools/grep.ts` | `createGrepTool`, `createGrepToolDefinition`, `grepToolSystemPromptContribution` | `102814acd9b220eb16e87f9f18b4e0957699a5188bd5cff80ac0af03634d7c24` |
| `core/tools/find.ts` | `createFindTool`, `createFindToolDefinition`, `findToolSystemPromptContribution` | `557ef716bcf3e26f7086153c4ff8fdac7ebc9f9a04026b666d05860c0592a022` |
| `core/tools/ls.ts` | `createLsTool`, `createLsToolDefinition`, `lsToolSystemPromptContribution` | `e03428bd7e7d26ee6fbbe981948e80b05a99536d35372d52aa21e2725a8aa263` |

The generated-prompt hash is a separate fixture value. Never substitute a source-file hash for a
generated prompt hash. The checked-in canonical fixture
[`crates/tea-core/profile/default-profile.json`](../crates/tea-core/profile/default-profile.json) has fixed
workspace/documentation inputs and currently records prompt SHA-256
`856e7855dcf14420a8433611a65c55427f1fe4dfa614780dfaea2e06331b3d3e`.

## Active-tool order

The captured profile constructs this order:

```text
read -> bash -> edit -> write
```

`buildSystemPrompt` defaults `selectedTools` to the same four names. `grep`, `find`, and `ls` are
standard factories and are included in the read-only/all helper sets, but are not silently added to
the default coding set. The fixture must capture factory output and prompt-visible order rather
than relying on this source reading.

| Position | Tool | Factory | Definition factory | Default coding active? |
| ---: | --- | --- | --- | --- |
| 1 | `read` | `createReadTool` | `createReadToolDefinition` | yes; fixture required |
| 2 | `bash` | `createBashTool` | `createBashToolDefinition` | yes; fixture required |
| 3 | `edit` | `createEditTool` | `createEditToolDefinition` | yes; fixture required |
| 4 | `write` | `createWriteTool` | `createWriteToolDefinition` | yes; fixture required |
| — | `grep` | `createGrepTool` | `createGrepToolDefinition` | no in `createCodingTools`; available to explicit profile |
| — | `find` | `createFindTool` | `createFindToolDefinition` | no in `createCodingTools`; available to explicit profile |
| — | `ls` | `createLsTool` | `createLsToolDefinition` | no in `createCodingTools`; available to explicit profile |

## Tool-definition ledger

The canonical fixture must serialize each definition's name, label, description, prompt snippet,
guideline array, and JSON Schema. The field table below is the readable ledger; the fixture's
canonical JSON is authoritative for exact descriptions and TypeBox defaults.

| Tool | Schema fields (required unless marked optional) | Prompt snippet | Prompt guidelines |
| --- | --- | --- | --- |
| `read` | `path: string`; `offset: number?`; `limit: number?` | `Read file contents` | `Use read to examine files instead of cat or sed.` |
| `bash` | `command: string`; `timeout: number?` | `Execute bash commands (ls, grep, find, etc.)` | `You can inspect PI_* environment variables for current model and session details.` when `exposeSessionEnvironment` is enabled; otherwise no tool guidelines |
| `edit` | `path: string`; `edits: array` of `{oldText: string, newText: string}` | `Make precise file edits with exact text replacement, including multiple disjoint edits in one call` | Four exact-text/one-call/overlap/minimal-oldText guidelines from `editToolSystemPromptContribution` |
| `write` | `path: string`; `content: string` | `Create or overwrite files` | `Use write only for new files or complete rewrites.` |
| `grep` | `pattern: string`; `path: string?`; `glob: string?`; `ignoreCase: boolean?`; `literal: boolean?`; `context: number?`; `limit: number?` | `Search file contents for patterns (respects .gitignore)` | none |
| `find` | `pattern: string`; `path: string?`; `limit: number?` | `Find files by glob pattern (respects .gitignore)` | none |
| `ls` | `path: string?`; `limit: number?` | `List directory contents` | none |

Schema fixture requirements:

- Capture canonical JSON after TypeBox serialization, including `type`, `properties`, `required`,
  descriptions, array item schema, and `additionalProperties` behavior.
- Preserve property names, requiredness, defaults expressed in descriptions, and field order where
  the comparator treats JSON object order as presentation data.
- Hash the canonical UTF-8 schema bytes separately from the generated prompt.
- Test valid input, invalid input/schema failure, and host-operation failure for each active tool.

## System-prompt contract

`buildSystemPrompt` has two modes:

1. With no `customPrompt`, it emits the ordered default coding prompt, visible-tool list, derived
   guidelines, Pi documentation paths, optional append/context/skills sections, and explicit
   current working directory.
2. With `customPrompt`, it uses the custom text, append/context/skills rules, and explicit working
   directory; skills are included only when `read` is selected.

V1 uses the default mode for `PiDefaultCodingProfile`, with all path-like inputs made explicit by
the profile adapter. The profile does not call `getReadmePath`, `getDocsPath`, `getExamplesPath`,
resource loaders, or skills discovery. Fixture inputs substitute canonical fixed paths so the
output is reproducible.

The default template has these ordered sections:

```text
You are an expert coding assistant ...

Available tools:
- <visible tool>: <snippet>

In addition to the tools above, ...

Guidelines:
- <derived/custom guideline, de-duplicated in insertion order>
- Be concise in your responses
- Show file paths clearly when working with files

Pi documentation ...
<optional append section>
<optional project context>
<optional skills section>

Current working directory: <canonical slash-normalized workspace>
```

If only `bash` is active among exploration tools, the derived guideline
`Use bash for file operations like ls, rg, find` is inserted before caller guidelines. Tool
snippets are listed only when a selected tool has a snippet. Guideline duplicates are removed by
exact string after trimming; blank caller guidelines are ignored.

## Prompt fixture and byte hash

Use this exact declarative fixture shape. The `expected_prompt_utf8` field is the complete generated
prompt with no implicit trailing newline; `expected_prompt_sha256` hashes those UTF-8 bytes.

```json
{
  "scenario": "profile/default-prompt",
  "inputs": {
    "workspace": "/fixture/workspace",
    "selected_tools": ["read", "bash", "edit", "write"],
    "tool_snippets": {
      "read": "Read file contents",
      "bash": "Execute bash commands (ls, grep, find, etc.)",
      "edit": "Make precise file edits with exact text replacement, including multiple disjoint edits in one call",
      "write": "Create or overwrite files"
    },
    "prompt_guidelines": [],
    "append_system_prompt": null,
    "context_files": [],
    "skills": [],
    "documentation_paths": {
      "readme": "/fixture/pi/README.md",
      "docs": "/fixture/pi/docs",
      "examples": "/fixture/pi/examples"
    }
  },
  "expected_active_tool_order": ["read", "bash", "edit", "write"],
  "expected_prompt_utf8": "<complete bytes represented as JSON string>",
  "expected_prompt_sha256": "<64 lowercase hex>",
  "hash_command": "printf %s \"$PROMPT\" | shasum -a 256",
  "normalization": ["workspace substitution only"]
}
```

The fixture suite must add variants for no tools, read-only tools, custom prompt, append/context,
duplicate/blank guidelines, and workspace paths containing backslashes. Only the explicitly named
workspace substitution may be normalized in a fixture comparison; prompt whitespace and section
order are semantic.

## Operation and capability boundary

Each standard factory has an operation adapter. The adapter receives an explicit normalized
workspace and caller cancellation signal. The profile may expose policy hooks that permit, deny,
log, sandbox, or replace an operation. A denied operation produces the same typed tool-result error
path as any other host failure; it does not invoke a hidden approval UI.

| Tool | Minimum explicit adapter surface |
| --- | --- |
| `read` | Read bytes, readability check, optional image MIME/detection; no implicit cwd/home |
| `bash` | Execute command in workspace with output callback, cancellation, timeout, and explicit environment policy |
| `edit` | Read/write bytes and access check; deterministic exact replacement/diff behavior |
| `write` | Write bytes and explicit recursive-directory operation |
| `grep` | Directory/file inspection and search process or equivalent explicit search capability |
| `find` | Existence and glob/search capability |
| `ls` | Existence, stat and directory-entry capability |

The profile must reject path escapes according to its chosen host policy and record that policy in
the fixture manifest. It must not recreate Pi's session/resource discovery as a hidden fallback.

## Profile behavior fixtures

These profile fixture IDs are owned by `tea-core`; the canonical capture is
`crates/tea-core/profile/default-profile.json`, and the provider-free Rust
runner is `crates/tea-core/fixtures/run.sh`.

```text
profile/default-prompt             byte prompt + active order + snippets/guidelines
profile/definitions                canonical schemas/descriptions/factory order
profile/read                       success, invalid input, host error
profile/bash                       success, invalid timeout, host error/cancel
profile/edit                       exact replacement success, invalid/ambiguous edit, host error
profile/write                      success, invalid input, host error
profile/grep                       success, invalid pattern/input, host error
profile/find                       success, invalid pattern/input, host error
profile/ls                         success, invalid path/input, host error
profile/replacement                replace/remove/wrap every standard tool
profile/sterile                    no default tools and caller-supplied prompt
profile/workspace-isolation        two explicit workspaces cannot cross authority
```

Every profile fixture uses an isolated temporary or virtual fixture workspace. No fixture consults
the repository cwd, credentials, sessions, or a live provider. The Rust factories run through
virtual in-memory operation adapters and never consult a live Pi installation or invoke Pi.

The executable factory smoke/evidence suite is
[`crates/tea-core/tests/default_tools_behavior.rs`](../crates/tea-core/tests/default_tools_behavior.rs).
It creates a unique temporary workspace per test and covers successful calls, invalid arguments
that must stop before host dispatch, and explicit host-operation failures for all seven standard
tools. The test also locks in head truncation for `read`, explicit empty-success output for
`bash`, and the two-workspace capability boundary. It is intentionally separate from the
declarative core runner fixtures: it verifies the Rust factories' concrete capability boundary
without consulting a live Pi installation or the repository workspace.

The Rust suite is the profile check. Run `bash crates/tea-core/fixtures/run.sh` to execute the complete
deterministic corpus. It proves the explicit capability boundary for all seven tools; hosts
needing full ripgrep behavior may replace `CodingOperations::grep_files` explicitly.

## Profile update procedure

When the default profile changes, update the captured factories, schemas, snippets, guidelines,
active order, generated prompt bytes/hash, operation behavior, and the corresponding Rust tests
and fixtures together. A changed default profile is a deliberate contract-version change.
