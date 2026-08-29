local prompt = require("./prompts.luau")

return {
    prompt_sections = {
        { id = "coding", content = prompt },
    },
    tools = {
        {
            name = "read",
            description = "Inspect a regular workspace file. Use offset and limit for large files. Set includeDigest=true before a conditional edit when you need a complete-file BLAKE3 precondition.",
            capability = "tea.workspace.read.v1",
            execution_mode = "parallel",
            schema_json = [[{"type":"object","additionalProperties":false,"required":["path"],"properties":{"path":{"type":"string","maxLength":4096,"description":"In-workspace file path."},"offset":{"type":"integer","minimum":1,"description":"One-indexed line at which to begin."},"limit":{"type":"integer","minimum":1,"description":"Maximum number of lines."},"includeDigest":{"type":"boolean","description":"Include the complete-file BLAKE3 digest."}}}]],
            handler_module = "tools/read.luau",
        },
        {
            name = "bash",
            description = "Run a bash command in the workspace with the host-selected environment. Use this for commands and ordinary directory or text searches.",
            capability = "tea.process.v1",
            execution_mode = "parallel",
            schema_json = [[{"type":"object","additionalProperties":false,"required":["command"],"properties":{"command":{"type":"string","minLength":1,"description":"Bash command to run."},"timeout":{"type":"number","exclusiveMinimum":0,"maximum":2147.483647,"description":"Optional timeout in seconds."}}}]],
            handler_module = "tools/bash.luau",
        },
        {
            name = "edit",
            description = "Create, replace, or precisely modify workspace files in one validated transaction. Each files[] entry uses exactly one of edits or content. Exact edits require an existing file; content creates an absent file or completely replaces an existing regular file. A new file's parent directory must already exist; use bash to create directories first.",
            capability = "tea.workspace.mutate.v1",
            execution_mode = "parallel",
            requires_exclusive_batch = true,
            cancellation_settlement_mode = "await_future",
            schema_json = [[{"type":"object","additionalProperties":false,"required":["files"],"properties":{"files":{"type":"array","minItems":1,"maxItems":32,"items":{"type":"object","additionalProperties":false,"required":["path"],"properties":{"path":{"type":"string","minLength":1,"maxLength":4096,"description":"In-workspace target path."},"expectedDigest":{"type":"string","minLength":64,"maxLength":64,"description":"Optional complete-file BLAKE3 precondition for an existing target."},"edits":{"type":"array","minItems":1,"maxItems":64,"items":{"type":"object","additionalProperties":false,"required":["oldText","newText"],"properties":{"oldText":{"type":"string","minLength":1},"newText":{"type":"string"}}}},"content":{"type":"string","description":"Complete desired content; creates an absent file only when its parent directory already exists, or replaces an existing file."}},"oneOf":[{"required":["edits"]},{"required":["content"]}]}}}}]],
            handler_module = "tools/edit.luau",
        },
        {
            name = "find",
            description = "Efficiently locate workspace files with Tea's bounded, deterministic glob search. It is faster and more predictable than repeatedly spawning shell find commands.",
            capability = "tea.workspace.search.v1",
            execution_mode = "parallel",
            schema_json = [[{"type":"object","additionalProperties":false,"required":["pattern"],"properties":{"pattern":{"type":"string","minLength":1,"maxLength":4096,"description":"Glob such as *.rs or src/**/*.luau."},"path":{"type":"string","description":"In-workspace directory to search; defaults to the workspace root."},"limit":{"type":"integer","minimum":1,"maximum":1000,"description":"Maximum returned paths; defaults to and cannot exceed 1000."}}}]],
            handler_module = "tools/find.luau",
        },
    },
}
