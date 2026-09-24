-- Optional public helper. The host supplies only this extension's private,
-- bounded JSON value; this source has no filesystem or provider authority.
local MAX_COUNT = 1000

local function count(value)
    if value == nil then
        return 0
    end
    assert(type(value) == "number" and value >= 0 and value % 1 == 0 and value <= MAX_COUNT,
        "run_counter state must be an integer from 0 to 1000")
    return value
end

return {
    state_version = "run_counter.v1",
    prompt_sections = {},
    commands = {
        {
            name = "/run-count",
            help = "show the number of completed operations on this lane",
            allowed_while_active = true,
            handler = function(input)
                return { notice = "Completed operations: " .. tostring(count(input.state)) }
            end,
        },
    },
    on_idle = function(input)
        if input.outcome ~= "completed" then
            return nil
        end
        return {
            state = { content_json = tostring(math.min(count(input.state) + 1, MAX_COUNT)) },
        }
    end,
}
