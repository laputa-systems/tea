return function(call)
    local response = coroutine.yield({
        kind = "capability",
        capability = "tea.workspace.mutate.v1",
        method = "commit",
        arguments = { files = call.arguments.files },
    })
    local function count(value, singular)
        return tostring(value) .. " " .. singular .. (value == 1 and "" or "s")
    end
    local changed = response.modifiedExistingFiles
    local precise = response.preciseReplacements
    local created = response.createdFiles
    local parts = {}
    if changed > 0 then
        local message = "Changed " .. count(changed, "existing file")
        if precise > 0 then
            message = message .. " with " .. count(precise, "precise replacement")
        end
        table.insert(parts, message)
    end
    if created > 0 then
        table.insert(parts, (changed > 0 and "created " or "Created ") .. count(created, "file"))
    end
    if #parts == 0 then return { content = "No files changed." } end
    return { content = table.concat(parts, "; ") .. "." }
end
