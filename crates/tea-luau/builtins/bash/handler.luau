return function(call)
    local response = coroutine.yield({
        kind = "capability",
        capability = "tea.process.v1",
        method = "run",
        arguments = {
            command = call.arguments.command,
            timeout = call.arguments.timeout,
        },
    })
    local content = response.content
    if response.exitCode == nil or response.exitCode ~= 0 then
        if content == "" then
            content = "command exited with status " .. tostring(response.exitCode or -1)
        end
        return { content = content, is_error = true }
    end
    if response.truncated then content = content .. "\n[truncated]" end
    if content == "" then content = "(no output)" end
    return { content = content }
end
