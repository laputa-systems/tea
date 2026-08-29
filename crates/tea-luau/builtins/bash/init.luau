local prompt = require("./prompts.luau")

return {
    prompt_sections = {
        { id = "bash", content = prompt },
    },
    tools = {
        {
            name = "bash",
            description = "Run a bash command in the workspace with the host-selected environment. Use this for commands and ordinary directory or text searches.",
            capability = "tea.process.v1",
            execution_mode = "parallel",
            schema_json = [[{"type":"object","additionalProperties":false,"required":["command"],"properties":{"command":{"type":"string","minLength":1,"description":"Bash command to run."},"timeout":{"type":"number","exclusiveMinimum":0,"maximum":2147.483647,"description":"Optional timeout in seconds."}}}]],
            handler_module = "handler.luau",
        },
    },
}
