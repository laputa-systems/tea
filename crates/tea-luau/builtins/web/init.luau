local prompt = require("./prompts.luau")
local handler_source = require("./handler_source.luau")

return {
    prompt_sections = {
        { id = "web", content = prompt },
    },
    tools = {
        {
            name = "web",
            description = "Retrieve web evidence. Pass query to discover relevant sources and receive their cleaned content in the same call. Pass urls to retrieve known sources together; batch independent known URLs in one urls array rather than making sequential calls.",
            capability = "network.http",
            execution_mode = "parallel",
            schema_json = [[{"oneOf":[{"type":"object","additionalProperties":false,"required":["query"],"properties":{"query":{"type":"string","minLength":1,"maxLength":500},"kind":{"enum":["developer","web"]},"limit":{"type":"integer","minimum":1,"maximum":8}}},{"type":"object","additionalProperties":false,"required":["urls"],"properties":{"urls":{"type":"array","minItems":1,"maxItems":8,"uniqueItems":true,"items":{"type":"string","minLength":1,"maxLength":8192}}}}]}]],
            handler_source = handler_source,
        },
    },
}
