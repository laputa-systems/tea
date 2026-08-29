local prompt = require("./prompts.luau")

return {
    prompt_sections = {
        { id = "read", content = prompt },
    },
    tools = {
        {
            name = "read",
            description = "Inspect a regular workspace file. Use offset and limit for large files. Set includeDigest=true before a conditional edit when you need a complete-file BLAKE3 precondition.",
            capability = "tea.workspace.read.v1",
            execution_mode = "parallel",
            schema_json = [[{"type":"object","additionalProperties":false,"required":["path"],"properties":{"path":{"type":"string","maxLength":4096,"description":"In-workspace file path."},"offset":{"type":"integer","minimum":1,"description":"One-indexed line at which to begin."},"limit":{"type":"integer","minimum":1,"description":"Maximum number of lines."},"includeDigest":{"type":"boolean","description":"Include the complete-file BLAKE3 digest."}}}]],
            handler_module = "handler.luau",
        },
    },
}
