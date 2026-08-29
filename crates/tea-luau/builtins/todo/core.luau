-- Durable nested todo semantics.
--
-- This module is the whole todo state machine: validation, Markdown
-- synchronization, recursive status transitions, automatic next-task
-- selection, canonical Markdown, and the bounded activity projection. The host
-- owns only generic extension persistence and generic presentation plumbing.
--
-- It is loaded twice from one checked-in source: `init.luau` requires it for
-- the read-only `/todos` command, and `builtins.rs` wraps the same bytes around
-- the executable handler tail so the tool VM shares these exact semantics.
-- Keep it free of host globals so both environments behave identically.

local core = {}

--- Durable state kind. State is a normalized snapshot, never an operation log.
core.STATE_KIND = "todo.state.v1"

core.MAX_ITEMS = 128
core.MAX_DEPTH = 8
core.MAX_TEXT_CHARS = 200
core.MAX_REASON_CHARS = 300
--- Encoded-state ceiling. The host rejects an extension state update above
--- 16384 bytes, so refuse slightly earlier with a message the model can act on
--- instead of surfacing an opaque capability failure.
core.MAX_STATE_BYTES = 15360
--- Largest number of todo rows in the human activity projection. The
--- model-facing read is never paginated; only this display is bounded.
core.MAX_ACTIVITY_ROWS = 12
--- Separator between a blocked task and its blocker. `sync_markdown` strips an
--- unchanged suffix so returned Markdown can be edited and sent straight back.
core.REASON_SEPARATOR = " — "

local MARKER_OF_STATUS = {
    pending = " ",
    in_progress = ">",
    done = "x",
    blocked = "!",
    dropped = "~",
}

local STATUS_OF_MARKER = {
    [" "] = "pending",
    [">"] = "in_progress",
    ["x"] = "done",
    ["!"] = "blocked",
    ["~"] = "dropped",
}

core.MARKER_OF_STATUS = MARKER_OF_STATUS
core.STATUS_OF_MARKER = STATUS_OF_MARKER

-- Multi-byte characters that can reorder or hide rendered output. Every one of
-- them starts with a lead byte in `FORMATTING_LEAD_BYTES`, so ordinary text
-- needs a single C-level search rather than a scan per pattern. Keeping all
-- validation at the C level also keeps per-character Lua loops, and therefore
-- interrupt-budget pressure, out of the hot paths.
local FORMATTING_LEAD_BYTES = "[\194\216\226\239]"
local FORMATTING_PATTERNS = {
    "\194[\128-\159]", -- C1 controls
    "\226\128[\142\143\168\169\170\171\172\173\174]", -- LRM/RLM, LS/PS, LRE..RLO
    "\226\129[\166\167\168\169]", -- LRI..PDI
    "\216\156", -- ALM
    "\239\187\191", -- BOM / zero-width no-break space
}

local function text_length(value: string): number
    return utf8.len(value) or #value
end

local function formatting_character(value: string): boolean
    if string.find(value, FORMATTING_LEAD_BYTES) == nil then
        return false
    end
    for _, pattern in ipairs(FORMATTING_PATTERNS) do
        if string.find(value, pattern) ~= nil then
            return true
        end
    end
    return false
end

--- Measure displayable text in one pass: returns `length` or `nil, diagnostic`.
local function measure_text(value: string)
    local length = utf8.len(value)
    if length == nil then
        return nil, "contains invalid UTF-8"
    end
    if string.find(value, "%c") ~= nil then
        return nil, "contains a control character"
    end
    if formatting_character(value) then
        return nil, "contains a bidirectional formatting character"
    end
    return length
end

-- Luau charges one interrupt per scanned position inside a Lua pattern, so a
-- pattern pass over a whole plan is the dominant cost in this module. Plain
-- (non-pattern) searches and `string.sub` are constant-cost, so the hot paths
-- below use them to skip pattern work that would almost always find nothing.

--- Collapse space runs and trim, for text already known to contain no
--- whitespace other than the space character. Conservative: it never removes
--- punctuation, changes case, or repairs structure.
local function normalize_text(value: string): string
    if string.sub(value, 1, 1) == " " or string.sub(value, -1, -1) == " " then
        value = string.match(value, "^ *(.-) *$")
    end
    if string.find(value, "  ", 1, true) ~= nil then
        value = string.gsub(value, " +", " ")
    end
    return value
end

--- Normalize and validate an arbitrary blocker reason.
---
--- Unlike Markdown rows, a reason arrives straight from tool arguments and has
--- never been screened, so every whitespace form is folded and the result is
--- checked before it can reach durable state.
--- Returns `reason` (possibly nil when empty) or `nil, diagnostic`.
function core.normalize_blocker(value: string)
    value = string.gsub(value, "%s+", " ")
    value = string.match(value, "^ *(.-) *$")
    if value == "" then
        return nil
    end
    local length, unsafe = measure_text(value)
    if length == nil then
        return nil, "reason " .. unsafe
    end
    if length > core.MAX_REASON_CHARS then
        return nil, "reason is longer than " .. integer(core.MAX_REASON_CHARS) .. " characters"
    end
    return value
end

--- Format an integer for display and for JSON, without float notation.
local function integer(value)
    return string.format("%d", value)
end

local function is_integer(value): boolean
    return type(value) == "number" and value == math.floor(value)
end

--- Validate one durable state document.
---
--- Returns `state` or `nil, diagnostic`. Corrupt state is always an error: it
--- never degrades into an empty list, because silently discarding a plan is
--- worse than reporting that it cannot be read.
function core.validate_state(raw)
    if raw == nil then
        return { next_id = 1, items = {} }
    end
    if type(raw) ~= "table" then
        return nil, "state is not an object"
    end
    local next_id = raw.next_id
    if not is_integer(next_id) or next_id < 1 then
        return nil, "next_id must be a positive integer"
    end
    local raw_items = raw.items
    if raw_items == nil then
        raw_items = {}
    end
    if type(raw_items) ~= "table" then
        return nil, "items must be an array"
    end
    local count = #raw_items
    if count > core.MAX_ITEMS then
        return nil, "state holds more than " .. integer(core.MAX_ITEMS) .. " items"
    end

    local items = {}
    local seen = {}
    local open = {}
    for position = 1, count do
        local raw_item = raw_items[position]
        if type(raw_item) ~= "table" then
            return nil, ("item " .. integer(position)) .. " is not an object"
        end
        local id = raw_item.id
        if not is_integer(id) or id < 1 or id >= next_id then
            return nil, ("item " .. integer(position)) .. " has an id outside 1.." .. integer(next_id - 1)
        end
        if seen[id] then
            return nil, "id " .. integer(id) .. " appears more than once"
        end
        seen[id] = true
        local text = raw_item.text
        if type(text) ~= "string" or text == "" then
            return nil, ("item " .. integer(position)) .. " has empty text"
        end
        local length, unsafe = measure_text(text)
        if length == nil then
            return nil, ("item " .. integer(position)) .. " text " .. unsafe
        end
        if length > core.MAX_TEXT_CHARS then
            return nil, ("item " .. integer(position)) .. " text is longer than " .. integer(core.MAX_TEXT_CHARS) .. " characters"
        end
        local status = raw_item.status
        if type(status) ~= "string" or MARKER_OF_STATUS[status] == nil then
            return nil, ("item " .. integer(position)) .. " has an unknown status"
        end

        -- Array order is canonical depth-first document order, so a parent is
        -- always an already-open ancestor of the previous row.
        local depth = 0
        local parent_id = raw_item.parent_id
        if parent_id ~= nil then
            if not is_integer(parent_id) then
                return nil, ("item " .. integer(position)) .. " has a non-integer parent_id"
            end
            local level = nil
            for candidate = #open, 1, -1 do
                if open[candidate] == parent_id then
                    level = candidate
                    break
                end
            end
            if level == nil then
                return nil, ("item " .. integer(position)) .. " is not in depth-first order under its parent"
            end
            for candidate = #open, level + 1, -1 do
                open[candidate] = nil
            end
            depth = level
        else
            for candidate = #open, 1, -1 do
                open[candidate] = nil
            end
        end
        if depth + 1 > core.MAX_DEPTH then
            return nil, ("item " .. integer(position)) .. " is nested deeper than " .. integer(core.MAX_DEPTH) .. " levels"
        end
        open[depth + 1] = id

        local reason = raw_item.reason
        if reason ~= nil then
            if type(reason) ~= "string" or reason == "" or status ~= "blocked" then
                return nil, ("item " .. integer(position)) .. " has a blocker reason without blocked status"
            end
            local blocker_length, unsafe_blocker = measure_text(reason)
            if blocker_length == nil then
                return nil, ("item " .. integer(position)) .. " blocker " .. unsafe_blocker
            end
            if blocker_length > core.MAX_REASON_CHARS then
                return nil, ("item " .. integer(position)) .. " blocker is longer than " .. integer(core.MAX_REASON_CHARS) .. " characters"
            end
        end

        items[position] = {
            id = id,
            text = text,
            status = status,
            parent_id = parent_id,
            reason = reason,
        }
    end
    return { next_id = next_id, items = items }
end

function core.clone_state(state)
    local items = {}
    for position, item in ipairs(state.items) do
        items[position] = {
            id = item.id,
            text = item.text,
            status = item.status,
            parent_id = item.parent_id,
            reason = item.reason,
        }
    end
    return { next_id = state.next_id, items = items }
end

--- Derive positions, depths, and leaf information in one pass over canonical order.
function core.analyze(state)
    local position_by_id = {}
    local depth = {}
    local has_children = {}
    for position, item in ipairs(state.items) do
        position_by_id[item.id] = position
        local level = 0
        if item.parent_id ~= nil then
            has_children[item.parent_id] = true
            level = depth[position_by_id[item.parent_id]] + 1
        end
        depth[position] = level
    end
    return { position_by_id = position_by_id, depth = depth, has_children = has_children }
end

--- Last position of the contiguous subtree rooted at `position`.
local function subtree_end(analysis, count: number, position: number): number
    local base = analysis.depth[position]
    local last = position
    for index = position + 1, count do
        if analysis.depth[index] <= base then
            break
        end
        last = index
    end
    return last
end

local function set_status(item, status: string)
    item.status = status
    if status ~= "blocked" then
        -- A blocker reason describes why open work stopped; it is meaningless
        -- once the item is no longer blocked.
        item.reason = nil
    end
end

--- Reopen closed or blocked ancestors so an active leaf is never stranded
--- beneath finished work. Returns the number of ancestors changed.
local function reopen_ancestors(state, analysis, position: number): number
    local changed = 0
    local parent_id = state.items[position].parent_id
    while parent_id ~= nil do
        local ancestor = state.items[analysis.position_by_id[parent_id]]
        if ancestor.status == "done" or ancestor.status == "dropped" or ancestor.status == "blocked" then
            set_status(ancestor, "pending")
            changed += 1
        end
        parent_id = ancestor.parent_id
    end
    return changed
end

--- Apply one status change and its recursive consequences.
---
--- The named target always takes the requested status: a status report is
--- authoritative for the id it names. Descendants follow the transition tables,
--- so explicitly dropped work is never silently re-scoped and finished work is
--- never silently reopened.
---
--- Returns `descendants_changed` or `nil, diagnostic`.
function core.apply_status(state, id: number, status: string, reason: string?)
    if MARKER_OF_STATUS[status] == nil then
        return nil, "unknown status"
    end
    if reason ~= nil and status ~= "blocked" then
        return nil, "reason is only accepted with status blocked"
    end
    local analysis = core.analyze(state)
    local position = analysis.position_by_id[id]
    if position == nil then
        return nil, "todo #" .. integer(id) .. " does not exist"
    end
    local items = state.items
    local last = subtree_end(analysis, #items, position)
    local target = items[position]
    local changed = 0

    if status == "done" or status == "dropped" then
        set_status(target, status)
        for index = position + 1, last do
            local item = items[index]
            if item.status ~= "done" and item.status ~= "dropped" then
                set_status(item, status)
                changed += 1
            end
        end
    elseif status == "blocked" then
        target.status = "blocked"
        target.reason = reason
        for index = position + 1, last do
            local item = items[index]
            if item.status == "pending" or item.status == "in_progress" then
                set_status(item, "blocked")
                changed += 1
            end
        end
    elseif status == "pending" then
        set_status(target, "pending")
        for index = position + 1, last do
            local item = items[index]
            if item.status ~= "pending" and item.status ~= "in_progress" then
                set_status(item, "pending")
                changed += 1
            else
                item.reason = nil
            end
        end
        reopen_ancestors(state, analysis, position)
    else
        set_status(target, "in_progress")
        reopen_ancestors(state, analysis, position)
    end
    return changed
end

--- Whether an item can be worked on without first resolving an ancestor.
local function ancestors_actionable(state, analysis, position: number): boolean
    local parent_id = state.items[position].parent_id
    while parent_id ~= nil do
        local ancestor = state.items[analysis.position_by_id[parent_id]]
        if ancestor.status == "blocked" or ancestor.status == "dropped" then
            return false
        end
        parent_id = ancestor.parent_id
    end
    return true
end

--- Promote the first actionable pending leaf when nothing is active.
---
--- Returns the promoted id, or nil when work is already active or nothing is
--- actionable. Blocked, dropped, done, and non-leaf work is never promoted.
function core.auto_promote(state): number?
    local analysis = core.analyze(state)
    local candidate = nil
    for position, item in ipairs(state.items) do
        if item.status == "in_progress" then
            return nil
        end
        if candidate == nil and item.status == "pending" and not analysis.has_children[item.id] then
            candidate = position
        end
    end
    while candidate ~= nil and candidate <= #state.items do
        local item = state.items[candidate]
        if
            item.status == "pending"
            and not analysis.has_children[item.id]
            and ancestors_actionable(state, analysis, candidate)
        then
            set_status(item, "in_progress")
            reopen_ancestors(state, analysis, candidate)
            return item.id
        end
        candidate += 1
    end
    return nil
end

function core.counts(state)
    local counts = { pending = 0, in_progress = 0, done = 0, blocked = 0, dropped = 0 }
    for _, item in ipairs(state.items) do
        counts[item.status] += 1
    end
    return counts
end

--- First active item in canonical order, which is what the model should work on next.
function core.first_active(state)
    for _, item in ipairs(state.items) do
        if item.status == "in_progress" then
            return item
        end
    end
    return nil
end

function core.header(state): string
    local counts = core.counts(state)
    return "TODO · "
        .. integer(counts.in_progress)
        .. " active · "
        .. integer(counts.pending)
        .. " pending · "
        .. integer(counts.blocked)
        .. " blocked · "
        .. integer(counts.dropped)
        .. " dropped · "
        .. integer(counts.done)
        .. " done"
end

local function row_suffix(item): string
    if item.reason ~= nil then
        return core.REASON_SEPARATOR .. item.reason
    end
    return ""
end

--- Canonical Markdown for the model and for `/todos`. Always complete: the
--- model-facing read is never paginated.
function core.format_markdown(state): string
    local header = core.header(state)
    if #state.items == 0 then
        return header .. "\n\nThe todo list is empty."
    end
    local analysis = core.analyze(state)
    local lines = { header, "" }
    for position, item in ipairs(state.items) do
        lines[#lines + 1] = string.rep("  ", analysis.depth[position])
            .. "- ["
            .. MARKER_OF_STATUS[item.status]
            .. "] #"
            .. integer(item.id)
            .. " "
            .. item.text
            .. row_suffix(item)
    end
    return table.concat(lines, "\n")
end

local ACTIVITY_PRIORITY = { in_progress = 1, blocked = 2, pending = 3, done = 4, dropped = 5 }

--- Bounded human projection of the same state.
---
--- This is display policy only. It never reaches the model tool schema, so the
--- model still sees exactly one unpaginated todo list.
function core.format_activity(state): string
    local counts = core.counts(state)
    if #state.items == 0 then
        return "Todo · empty"
    end
    local parts = {}
    if counts.in_progress > 0 then
        parts[#parts + 1] = integer(counts.in_progress) .. " active"
    end
    if counts.pending > 0 then
        parts[#parts + 1] = integer(counts.pending) .. " pending"
    end
    if counts.blocked > 0 then
        parts[#parts + 1] = integer(counts.blocked) .. " blocked"
    end
    if #parts == 0 then
        parts[1] = integer(counts.done) .. " done"
        if counts.dropped > 0 then
            parts[2] = integer(counts.dropped) .. " dropped"
        end
    end

    local visible = {}
    local total = #state.items
    if total <= core.MAX_ACTIVITY_ROWS then
        for position = 1, total do
            visible[position] = true
        end
    else
        -- Retain active work first, then blocked and pending work, before any
        -- completed or abandoned history.
        local remaining = core.MAX_ACTIVITY_ROWS
        for rank = 1, 5 do
            for position, item in ipairs(state.items) do
                if remaining == 0 then
                    break
                end
                if visible[position] == nil and ACTIVITY_PRIORITY[item.status] == rank then
                    visible[position] = true
                    remaining -= 1
                end
            end
            if remaining == 0 then
                break
            end
        end
    end

    local analysis = core.analyze(state)
    local lines = { "Todo · " .. table.concat(parts, " · ") }
    local omitted = 0
    for position, item in ipairs(state.items) do
        if visible[position] then
            lines[#lines + 1] = string.rep("  ", analysis.depth[position])
                .. "- ["
                .. MARKER_OF_STATUS[item.status]
                .. "] "
                .. item.text
                .. row_suffix(item)
        else
            omitted += 1
        end
    end
    if omitted > 0 then
        lines[#lines + 1] = "  … " .. integer(omitted) .. " more"
    end
    return table.concat(lines, "\n")
end

--- Compact receipt for status reports, so a mutation never re-injects the
--- whole plan into model context.
function core.update_receipt(entries, state): string
    local lines = {}
    if #entries == 1 then
        local entry = entries[1]
        local verb = ({
            done = "Completed",
            blocked = "Blocked",
            dropped = "Dropped",
            pending = "Reopened",
            in_progress = "Started",
        })[entry.status]
        local line = verb .. " #" .. integer(entry.id)
        if entry.changed == 1 then
            line ..= " and 1 descendant"
        elseif entry.changed > 1 then
            line ..= " and " .. integer(entry.changed) .. " descendants"
        end
        lines[1] = line .. "."
    else
        lines[1] = "Updated " .. integer(#entries) .. " todos."
    end
    local active = core.first_active(state)
    if active ~= nil then
        lines[#lines + 1] = "Next: #" .. integer(active.id) .. " " .. active.text
    end
    lines[#lines + 1] = core.header(state)
    return table.concat(lines, "\n")
end

local JSON_ESCAPES = { ["\\"] = "\\\\", ['"'] = '\\"' }

--- Escape one already display-safe string for JSON. The plain searches avoid a
--- pattern pass over text that almost never contains either character.
local function escape_json(value: string): string
    if string.find(value, '"', 1, true) == nil and string.find(value, "\\", 1, true) == nil then
        return value
    end
    return (string.gsub(value, '[\\"]', JSON_ESCAPES))
end

--- Encode canonical state. Text and blockers are already free of control
--- characters, so only backslash and quote need escaping.
--- Returns the JSON document or `nil, diagnostic` when it is too large to store.
function core.encode_state(state)
    local parts = { '{"next_id":', tostring(state.next_id), ',"items":[' }
    for position, item in ipairs(state.items) do
        if position > 1 then
            parts[#parts + 1] = ","
        end
        parts[#parts + 1] = '{"id":'
            .. item.id
            .. ',"text":"'
            .. escape_json(item.text)
            .. '","status":"'
            .. item.status
            .. '"'
        if item.parent_id ~= nil then
            parts[#parts + 1] = ',"parent_id":' .. item.parent_id
        end
        if item.reason ~= nil then
            parts[#parts + 1] = ',"reason":"' .. escape_json(item.reason) .. '"'
        end
        parts[#parts + 1] = "}"
    end
    parts[#parts + 1] = "]}"
    local encoded = table.concat(parts)
    if #encoded > core.MAX_STATE_BYTES then
        return nil, "the todo plan is too large to store; shorten task text or remove finished items"
    end
    return encoded
end

--- Parse structural Markdown into ordered rows.
---
--- Returns `rows` or `nil, diagnostic`. Nothing is silently repaired: an
--- ambiguous tree is rejected so the previous durable state survives exactly.
function core.parse_markdown(markdown)
    if type(markdown) ~= "string" then
        return nil, "markdown must be a string"
    end
    if utf8.len(markdown) == nil then
        return nil, "markdown contains invalid UTF-8"
    end
    -- Validate display safety once for the whole document rather than once per
    -- row: the checks are identical and the rows are substrings.
    local body = markdown
    if string.find(body, "\r", 1, true) ~= nil then
        body = string.gsub(body, "\r\n", "\n")
    end
    if string.find(body, "\t", 1, true) ~= nil then
        return nil, "the plan uses a tab; indent with two spaces per level"
    end
    if string.find(body, "[\1-\8\11-\31\127]") ~= nil or string.find(body, "\0", 1, true) ~= nil then
        return nil, "the plan contains a control character"
    end
    if formatting_character(body) then
        return nil, "the plan contains a bidirectional formatting character"
    end
    if string.sub(body, -1) ~= "\n" then
        body ..= "\n"
    end

    local rows = {}
    local number = 0
    local previous_level = -1
    for line in string.gmatch(body, "([^\n]*)\n") do
        number += 1
        if string.match(line, "^ *$") == nil then
            local indent, marker, text = string.match(line, "^( *)%- %[(.)%] (.*)$")
            if indent == nil then
                return nil,
                    "line "
                        .. integer(number)
                        .. " is not a todo row; use '- [ ] task' with two spaces per level"
            end
            if #indent % 2 ~= 0 then
                return nil, "line " .. integer(number) .. " has odd indentation; use two spaces per level"
            end
            local level = #indent // 2
            if level > previous_level + 1 then
                return nil,
                    "line " .. integer(number) .. " indents more than one level deeper than the row above it"
            end
            if level + 1 > core.MAX_DEPTH then
                return nil,
                    "line "
                        .. integer(number)
                        .. " is nested deeper than "
                        .. integer(core.MAX_DEPTH)
                        .. " levels"
            end
            local status = STATUS_OF_MARKER[marker]
            if status == nil then
                return nil,
                    "line " .. integer(number) .. " has an unknown marker; use [ ], [>], [x], [!], or [~]"
            end

            local id = nil
            local id_text, remainder = string.match(text, "^#(%d+) (.*)$")
            if id_text == nil then
                id_text = string.match(text, "^#(%d+)$")
                if id_text ~= nil then
                    remainder = ""
                end
            end
            if id_text ~= nil then
                id = tonumber(id_text)
                text = remainder
            end

            text = normalize_text(text)
            if text == "" then
                return nil, "line " .. integer(number) .. " has empty task text"
            end
            if text_length(text) > core.MAX_TEXT_CHARS then
                return nil,
                    "line "
                        .. integer(number)
                        .. " text is longer than "
                        .. integer(core.MAX_TEXT_CHARS)
                        .. " characters"
            end
            if #rows == core.MAX_ITEMS then
                return nil, "the plan has more than " .. integer(core.MAX_ITEMS) .. " tasks"
            end
            rows[#rows + 1] = { line = number, level = level, id = id, status = status, text = text }
            previous_level = level
        end
    end
    return rows
end

--- Rebuild the tree so it structurally matches the submitted document.
---
--- Indentation determines parents, document order determines siblings, listed
--- ids keep their identity and status, omitted ids are removed, and id-less
--- rows are allocated new ids in document order. Ids are never reused.
--- Returns `state` or `nil, diagnostic`; nothing is allocated until every row
--- has been accepted, so a rejected synchronization is fully atomic.
function core.sync_markdown(state, rows)
    local analysis = core.analyze(state)
    local used = {}
    local items = {}
    local open = {}
    local next_id = state.next_id
    for position, row in ipairs(rows) do
        local id = row.id
        local status = "pending"
        local reason = nil
        local text = row.text
        if id ~= nil then
            local existing_position = analysis.position_by_id[id]
            if existing_position == nil then
                return nil,
                    "line "
                        .. integer(row.line)
                        .. " names todo #"
                        .. integer(id)
                        .. ", which does not exist; omit the id to create a task"
            end
            if used[id] then
                return nil, "line " .. integer(row.line) .. " repeats todo #" .. integer(id)
            end
            used[id] = true
            local existing = state.items[existing_position]
            status = existing.status
            reason = existing.reason
            -- A rendered blocker suffix is display text, not part of the task.
            if reason ~= nil then
                local suffix = core.REASON_SEPARATOR .. reason
                if #text > #suffix and string.sub(text, -#suffix) == suffix then
                    text = string.sub(text, 1, #text - #suffix)
                end
            end
            if text == "" then
                return nil, "line " .. integer(row.line) .. " has empty task text"
            end
        else
            if row.status ~= "pending" then
                return nil,
                    "line "
                        .. integer(row.line)
                        .. " is a new task, so it must use [ ]; report status separately with updates"
            end
            id = next_id
            next_id += 1
        end

        for level = #open, row.level + 1, -1 do
            open[level] = nil
        end
        local parent_id = nil
        if row.level > 0 then
            parent_id = open[row.level]
            if parent_id == nil then
                return nil, "line " .. integer(row.line) .. " has no parent row above it"
            end
        end
        open[row.level + 1] = id
        items[position] = { id = id, text = text, status = status, parent_id = parent_id, reason = reason }
    end
    return { next_id = next_id, items = items }
end

return core
