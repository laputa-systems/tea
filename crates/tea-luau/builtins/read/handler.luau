return function(call)
    local response = coroutine.yield({
        kind = "capability",
        capability = "tea.workspace.read.v1",
        method = "read",
        arguments = {
            path = call.arguments.path,
            offset = call.arguments.offset,
            limit = call.arguments.limit,
            includeDigest = call.arguments.includeDigest,
        },
    })
    local content = response.content
    if response.truncated then content = content .. "\n[truncated]" end
    if response.digest ~= nil then
        content = content .. "\n[complete-file blake3: " .. response.digest .. "]"
    end
    return { content = content }
end
