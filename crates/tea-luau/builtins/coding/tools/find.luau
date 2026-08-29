return function(call)
    local response = coroutine.yield({
        kind = "capability",
        capability = "tea.workspace.search.v1",
        method = "find",
        arguments = {
            pattern = call.arguments.pattern,
            path = call.arguments.path,
            limit = call.arguments.limit,
        },
    })
    if #response.matches == 0 then
        return { content = "No files found matching pattern" }
    end
    local content = table.concat(response.matches, "\n")
    if #response.matches >= response.limit then
        content = content .. "\n\n[" .. tostring(response.limit) .. " results limit reached]"
    end
    return { content = content }
end
