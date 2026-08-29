local prompt = require("./prompts.luau")

return {
    prompt_sections = {
        { id = "edit", content = prompt },
    },
    tools = {
        {
            name = "edit",
            description = "Create, replace, or precisely modify workspace files in one validated transaction. Each files[] entry uses exactly one of edits or content. Exact edits require an existing file; content creates an absent file or completely replaces an existing regular file. A new file's parent directory must already exist; use bash to create directories first.",
            capability = "tea.workspace.mutate.v1",
            execution_mode = "parallel",
            requires_exclusive_batch = true,
            cancellation_settlement_mode = "await_future",
            schema_json = [[{"type":"object","additionalProperties":false,"required":["files"],"properties":{"files":{"type":"array","minItems":1,"maxItems":32,"items":{"type":"object","additionalProperties":false,"required":["path"],"properties":{"path":{"type":"string","minLength":1,"maxLength":4096,"description":"In-workspace target path."},"expectedDigest":{"type":"string","minLength":64,"maxLength":64,"description":"Optional complete-file BLAKE3 precondition for an existing target."},"edits":{"type":"array","minItems":1,"maxItems":64,"items":{"type":"object","additionalProperties":false,"required":["oldText","newText"],"properties":{"oldText":{"type":"string","minLength":1},"newText":{"type":"string"}}}},"content":{"type":"string","description":"Complete desired content; creates an absent file only when its parent directory already exists, or replaces an existing file."}},"oneOf":[{"required":["edits"]},{"required":["content"]}]}}}}]],
            handler_module = "handler.luau",
        },
    },
}
