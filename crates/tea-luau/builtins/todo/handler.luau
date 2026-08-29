-- Executable handler for the model-facing `todo` tool.
--
-- All todo semantics live in the shared `core` module. This file is the only
-- place that talks to the host: it reads durable state, applies exactly one
-- requested operation, persists a normalized snapshot when state changed, and
-- republishes the human activity projection on every call.

local core = require("./core.luau")

return function(call)
    local arguments = call.arguments or {}
    local markdown = arguments.markdown
    local updates = arguments.updates

    local stored = coroutine.yield({
        kind = "capability",
        capability = "extension.state",
        method = "get",
        arguments = {},
    })
    local state, unreadable = core.validate_state((stored or {})[core.STATE_KIND])
    if state == nil then
        -- Corrupt durable state must never look like an empty plan.
        return {
            content = "Durable todo state is unreadable (" .. unreadable .. "). It was left unchanged.",
            is_error = true,
        }
    end

    -- Every call republishes the current list, so the human activity view is
    -- correct even when the requested mutation is refused.
    local function publish()
        coroutine.yield({ kind = "update", activity = core.format_activity(state) })
    end

    local function refuse(message)
        publish()
        return { content = message, is_error = true }
    end

    local function commit(next_state)
        local oversized = core.state_size_error(next_state)
        if oversized ~= nil then
            return oversized
        end
        coroutine.yield({
            kind = "capability",
            capability = "extension.state",
            method = "append",
            arguments = { kind = core.STATE_KIND, content = next_state },
        })
        state = next_state
        return nil
    end

    if markdown ~= nil and updates ~= nil then
        return refuse(
            "Send markdown or updates, not both: markdown restructures the plan and updates report status changes."
        )
    end

    if markdown == nil and updates == nil then
        publish()
        return { content = core.format_markdown(state) }
    end

    if markdown ~= nil then
        local rows, invalid = core.parse_markdown(markdown)
        if rows == nil then
            return refuse("Todo markdown was rejected: " .. invalid .. ". The plan is unchanged.")
        end
        local next_state, conflict = core.sync_markdown(state, rows)
        if next_state == nil then
            return refuse("Todo markdown was rejected: " .. conflict .. ". The plan is unchanged.")
        end
        core.auto_promote(next_state)
        local oversized = commit(next_state)
        if oversized ~= nil then
            return refuse("Todo markdown was rejected: " .. oversized .. ". The plan is unchanged.")
        end
        publish()
        -- Structural results return the whole list so new ids are visible.
        return { content = core.format_markdown(state) }
    end

    if type(updates) ~= "table" or #updates == 0 then
        return refuse("updates must be a non-empty array of {id, status}.")
    end
    local next_state = core.clone_state(state)
    local entries = {}
    for index = 1, #updates do
        local update = updates[index]
        local position = "update " .. string.format("%d", index) .. ": "
        if type(update) ~= "table" then
            return refuse(position .. "each update must be an object with id and status.")
        end
        local id = update.id
        if type(id) ~= "number" or id ~= math.floor(id) or id < 1 then
            return refuse(position .. "id must be a positive integer.")
        end
        local status = update.status
        if type(status) ~= "string" or core.MARKER_OF_STATUS[status] == nil then
            return refuse(position .. "status must be pending, in_progress, done, blocked, or dropped.")
        end
        local reason = update.reason
        if reason ~= nil then
            if type(reason) ~= "string" then
                return refuse(position .. "reason must be a string.")
            end
            local normalized, invalid_reason = core.normalize_blocker(reason)
            if invalid_reason ~= nil then
                return refuse(position .. invalid_reason .. ".")
            end
            reason = normalized
        end
        -- The whole array is atomic: nothing is persisted unless every update applies.
        local changed, rejected = core.apply_status(next_state, id, status, reason)
        if changed == nil then
            return refuse(position .. rejected .. ". No status changes were applied.")
        end
        entries[index] = { id = id, status = status, changed = changed }
    end
    core.auto_promote(next_state)
    local oversized = commit(next_state)
    if oversized ~= nil then
        return refuse(oversized)
    end
    publish()
    return { content = core.update_receipt(entries, state) }
end
