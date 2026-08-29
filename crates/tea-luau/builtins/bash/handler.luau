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
    local content = response.content or ""
    if response.truncated then content = content .. "\n[truncated]" end

    if response.termination == "exited" then
        if response.exitCode == 0 then
            if content == "" then content = "(no output)" end
            return { content = content }
        end
        if content == "" then
            content = "command exited with status " .. tostring(response.exitCode)
        end
        return { content = content, is_error = true }
    end

    if response.termination == "signaled" then
        local status = "command terminated by signal " .. tostring(response.signal)
        if content == "" then content = status else content = content .. "\n[" .. status .. "]" end
        return { content = content, is_error = true }
    end

    if response.termination == "timed_out" then
        if content == "" then content = "command timed out" else content = content .. "\n[command timed out]" end
        return { content = content, is_error = true }
    end

    if response.termination == "indeterminate" then
        local warning = "command termination is indeterminate; side effects may already exist; inspect state before retrying"
        local reason = response.reason
        if type(reason) ~= "string" or reason == "" then reason = warning end
        if not string.find(reason, warning, 1, true) then reason = reason .. "; " .. warning end
        if content == "" then content = reason else content = content .. "\n[" .. reason .. "]" end
        return { content = content, is_error = true }
    end

    return { content = "command returned an unrecognized termination receipt", is_error = true }
end
