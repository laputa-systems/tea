local core = require("./core.luau")
local prompt = require("./prompts.luau")

-- An extension command notice is bounded printable text. `/todos` is a
-- convenience view for an idle human; the model-facing read is never bounded.
local MAX_NOTICE_BYTES = 4000

local function bounded(text)
    if #text <= MAX_NOTICE_BYTES then
        return text
    end
    local cut = MAX_NOTICE_BYTES
    while cut > 0 and string.sub(text, cut, cut) ~= "\n" do
        cut -= 1
    end
    if cut == 0 then
        return string.match(text, "^[^\n]*")
    end
    return string.sub(text, 1, cut - 1) .. "\n… truncated; call the todo tool for the whole list"
end

--- Read-only projection of durable todo state. It has no subcommands: the live
--- activity presentation is the ambient todo UI, and structural or status
--- changes belong to the model-facing tool.
local function command(input)
    local state, unreadable = core.validate_state(input.state[core.STATE_KIND])
    if state == nil then
        return { notice = "Durable todo state is unreadable (" .. unreadable .. ")" }
    end
    return { notice = bounded(core.format_markdown(state)) }
end

local SCHEMA = table.concat({
    '{"type":"object","additionalProperties":false,"properties":{',
    '"markdown":{"type":"string","maxLength":32768},',
    '"updates":{"type":"array","minItems":1,"maxItems":128,"items":{',
    '"type":"object","required":["id","status"],"additionalProperties":false,"properties":{',
    '"id":{"type":"integer","minimum":1},',
    '"status":{"enum":["pending","in_progress","done","blocked","dropped"]},',
    '"reason":{"type":"string","maxLength":300}}}}}}',
})

return {
    prompt_sections = { { id = "todo", content = prompt } },
    commands = {
        {
            name = "/todos",
            help = "show the durable todo list",
            allowed_while_active = true,
            handler = command,
        },
    },
    tools = {
        {
            name = "todo",
            description = "Read or update the durable plan. No arguments reads the whole list. "
                .. "Send `updates` to report status changes; the extension handles hierarchy, recursive "
                .. "transitions, and the next actionable task. Send `markdown` only to create or restructure "
                .. "the plan, keeping existing #IDs and omitting the id for a new task. The complete document "
                .. "returned by a read can be edited and sent back unchanged as markdown.",
            capability = "extension.state",
            execution_mode = "sequential",
            schema_json = SCHEMA,
            handler_module = "handler.luau",
        },
    },
}
