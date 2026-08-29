return function(call)
    local response = coroutine.yield({
        kind = "capability",
        capability = "tea.workspace.mutate.v1",
        method = "commit",
        arguments = { files = call.arguments.files },
    })
    local parts = {}
    if response.replacements > 0 then
        table.insert(parts, "Applied " .. tostring(response.replacements) .. " precise replacements")
    end
    if response.replaced > 0 then
        table.insert(parts, "replaced " .. tostring(response.replaced) .. " complete files")
    end
    if response.created > 0 then
        table.insert(parts, "created " .. tostring(response.created) .. " files")
    end
    if #parts == 0 then return { content = "No files changed." } end
    return { content = table.concat(parts, "; ") .. "." }
end
