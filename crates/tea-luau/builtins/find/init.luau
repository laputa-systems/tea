local prompt = require("./prompts.luau")

return {
    prompt_sections = {
        { id = "find", content = prompt },
    },
    tools = {
        {
            name = "find",
            description = "Efficiently locate workspace files with Tea's bounded, deterministic glob search. It is faster and more predictable than repeatedly spawning shell find commands.",
            capability = "tea.workspace.search.v1",
            execution_mode = "parallel",
            schema_json = [[{"type":"object","additionalProperties":false,"required":["pattern"],"properties":{"pattern":{"type":"string","minLength":1,"maxLength":4096,"description":"Glob such as *.rs or src/**/*.luau."},"path":{"type":"string","description":"In-workspace directory to search; defaults to the workspace root."},"limit":{"type":"integer","minimum":1,"maximum":1000,"description":"Maximum returned paths; defaults to and cannot exceed 1000."}}}]],
            handler_module = "handler.luau",
        },
    },
}
