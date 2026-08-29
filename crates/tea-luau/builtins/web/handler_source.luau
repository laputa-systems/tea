return [[
local MAX_QUERY_BYTES = 500
local MAX_URL_BYTES = 8192
local MAX_SOURCES = 8
local SOURCE_BUDGET = 16 * 1024
local TOTAL_BUDGET = 96 * 1024

local function byte_length(value)
    return #value
end

local function json_string(value)
    local result = { '"' }
    for index = 1, #value do
        local byte = string.byte(value, index)
        if byte == 34 then
            table.insert(result, '\\"')
        elseif byte == 92 then
            table.insert(result, "\\\\")
        elseif byte == 8 then
            table.insert(result, "\\b")
        elseif byte == 12 then
            table.insert(result, "\\f")
        elseif byte == 10 then
            table.insert(result, "\\n")
        elseif byte == 13 then
            table.insert(result, "\\r")
        elseif byte == 9 then
            table.insert(result, "\\t")
        elseif byte < 32 then
            table.insert(result, string.format("\\u%04x", byte))
        else
            table.insert(result, string.char(byte))
        end
    end
    table.insert(result, '"')
    return table.concat(result)
end

local function json_encode(value)
    local kind = type(value)
    if value == nil then return "null" end
    if kind == "boolean" then return value and "true" or "false" end
    if kind == "number" then
        if value ~= value or value == math.huge or value == -math.huge then error("JSON number must be finite") end
        return tostring(value)
    end
    if kind == "string" then return json_string(value) end
    if kind ~= "table" then error("unsupported JSON value") end
    local maximum = 0
    local array = true
    for key, _ in pairs(value) do
        if type(key) == "number" and key >= 1 and math.floor(key) == key then
            if key > maximum then maximum = key end
        else
            array = false
        end
    end
    if array then
        for index = 1, maximum do
            if value[index] == nil then array = false break end
        end
    end
    if array then
        local encoded = {}
        for index = 1, maximum do table.insert(encoded, json_encode(value[index])) end
        return "[" .. table.concat(encoded, ",") .. "]"
    end
    local keys = {}
    for key, _ in pairs(value) do
        if type(key) ~= "string" then error("JSON object keys must be strings") end
        table.insert(keys, key)
    end
    table.sort(keys)
    local encoded = {}
    for _, key in ipairs(keys) do
        table.insert(encoded, json_string(key) .. ":" .. json_encode(value[key]))
    end
    return "{" .. table.concat(encoded, ",") .. "}"
end

local function http_request(request)
    return coroutine.yield({
        kind = "capability",
        capability = "network.http",
        method = "request",
        arguments_json = json_encode({
            route = "firecrawl",
            method = "POST",
            path = request.path,
            json = request.json,
            response = "json",
        }),
    })
end

local function http_request_many(requests)
    return coroutine.yield({
        kind = "capability",
        capability = "network.http",
        method = "request_many",
        arguments_json = json_encode({
            requests = requests,
            response = "json",
        }),
    })
end

local function trim(value)
    if type(value) ~= "string" then return "" end
    return string.gsub(value, "^%s*(.-)%s*$", "%1")
end

local function has_content(value)
    return type(value) == "string" and #value > 0 and string.find(value, "%S") ~= nil
end

local function text(value, maximum)
    if type(value) ~= "string" then return "" end
    if #value <= maximum then return value end
    local end_at = maximum
    while end_at > 0 do
        local following = string.byte(value, end_at + 1)
        if following == nil or following < 128 or following >= 192 then break end
        end_at = end_at - 1
    end
    return string.sub(value, 1, end_at)
end

local function truncate_utf8(value, maximum, marker)
    if #value <= maximum then return value, false end
    local content_maximum = math.max(0, maximum - #marker)
    local end_at = content_maximum
    while end_at > 0 do
        local following = string.byte(value, end_at + 1)
        if following == nil or following < 128 or following >= 192 then break end
        end_at = end_at - 1
    end
    return string.sub(value, 1, end_at) .. marker, true
end

local function scrape_options()
    return {
        formats = { "markdown" },
        onlyMainContent = true,
        onlyCleanContent = false,
        maxAge = 3600000,
        removeBase64Images = true,
        blockAds = true,
        proxy = "basic",
        storeInCache = true,
    }
end

local function scrape_request(url)
    return {
        route = "firecrawl",
        method = "POST",
        path = "/v2/scrape",
        json = {
            url = url,
            formats = { "markdown" },
            onlyMainContent = true,
            onlyCleanContent = false,
            maxAge = 3600000,
            removeBase64Images = true,
            blockAds = true,
            proxy = "basic",
            storeInCache = true,
        },
    }
end

local function diagnostic(outcome)
    if type(outcome) ~= "table" then return "invalid response" end
    if outcome.kind == "transport_error" then
        return text(tostring(outcome.code or "transport") .. ": " .. tostring(outcome.message or "request failed"), 240)
    end
    local status = outcome.status
    if status == 429 then return "rate limited (HTTP 429)" end
    if status == 402 then return "quota unavailable (HTTP 402)" end
    if status == 408 then return "timed out (HTTP 408)" end
    if type(status) == "number" and status >= 500 then return "upstream failure (HTTP " .. tostring(status) .. ")" end
    if type(status) == "number" then return "HTTP " .. tostring(status) end
    return "invalid response"
end

local function provider_diagnostic(body)
    if type(body) ~= "table" then return nil end
    for _, field in ipairs({ "error", "message", "warning" }) do
        local value = body[field]
        if type(value) == "string" and trim(value) ~= "" then return text(value, 240) end
        if type(value) == "table" and type(value.message) == "string" and trim(value.message) ~= "" then
            return text(value.message, 240)
        end
    end
    return nil
end

local function response_json(outcome)
    if type(outcome) ~= "table" or outcome.kind ~= "response" then return nil end
    if outcome.status ~= 200 then return nil end
    if type(outcome.json) ~= "table" then return nil end
    return outcome.json
end

local function scrape_markdown(outcome)
    local body = response_json(outcome)
    if body == nil then return nil, diagnostic(outcome) end
    if body.success ~= true then return nil, provider_diagnostic(body) or diagnostic(outcome) end
    local data = body.data
    if type(data) == "table" and has_content(data.markdown) then return data.markdown, nil end
    if has_content(body.markdown) then return body.markdown, nil end
    return nil, "full page content unavailable"
end

local function source(title, url, markdown, description, category)
    return {
        title = trim(title) ~= "" and title or url,
        url = url,
        markdown = markdown,
        description = trim(description),
        category = trim(category),
        unavailable = nil,
    }
end

local function parse_search_results(body)
    if type(body) ~= "table" or body.success ~= true or type(body.data) ~= "table" or type(body.data.web) ~= "table" then
        return nil
    end
    local results = {}
    for _, result in ipairs(body.data.web) do
        if type(result) == "table" and trim(result.url) ~= "" then
            table.insert(results, source(result.title, result.url, result.markdown, result.description, result.category))
        end
    end
    return results
end

local function repair_missing_search_pages(results)
    local requests = {}
    local indices = {}
    for index, result in ipairs(results) do
        if not has_content(result.markdown) then
            table.insert(requests, scrape_request(result.url))
            table.insert(indices, index)
        end
    end
    if #requests == 0 then return end
    local outcomes = http_request_many(requests)
    for position, index in ipairs(indices) do
        local markdown, failure = scrape_markdown(outcomes[position])
        if markdown ~= nil then
            results[index].markdown = markdown
        else
            results[index].unavailable = failure
        end
    end
end

local function render_sources(prefix, sources)
    local lines = { prefix }
    local remaining = TOTAL_BUDGET - #prefix - 2
    for index, result in ipairs(sources) do
        if result.failed then
            table.insert(lines, "\n[" .. tostring(index) .. "] FAILED\nURL: " .. text(result.url, 4096) .. "\nFirecrawl: " .. text(result.reason, 240) .. "\n")
        else
        local title = text(result.title, 2048)
        local url = text(result.url, 4096)
        local header = "\n[" .. tostring(index) .. "] " .. title .. "\nURL: " .. url
        if result.category ~= "" then header = header .. "\nCategory: " .. text(result.category, 256) end
        if result.description ~= "" then header = header .. "\nDescription: " .. text(result.description, 512) end
        table.insert(lines, header)
        remaining = math.max(0, remaining - #header)
        if has_content(result.markdown) then
            local begin_marker = "\n--- BEGIN UNTRUSTED WEB CONTENT " .. tostring(index) .. " ---\n"
            local end_marker = "\n--- END UNTRUSTED WEB CONTENT " .. tostring(index) .. " ---\n"
            local truncation = "\n[content truncated; retrieve this URL explicitly if deeper inspection is needed]"
            local fair_share = math.floor(math.max(0, remaining - #begin_marker - #end_marker) / math.max(1, #sources - index + 1))
            local allowance = math.min(SOURCE_BUDGET, fair_share)
            local content, _ = truncate_utf8(result.markdown, allowance, truncation)
            local rendered = begin_marker .. content .. end_marker
            table.insert(lines, rendered)
            remaining = math.max(0, remaining - #rendered)
        else
            table.insert(lines, "\n[full page content unavailable]\n")
        end
        end
    end
    return table.concat(lines)
end

local function handle_query(arguments)
    local query = arguments.query
    if type(query) ~= "string" or trim(query) == "" or #query > MAX_QUERY_BYTES then
        return { content = "query must be a non-empty string no longer than 500 bytes", is_error = true }
    end
    local kind = arguments.kind or "developer"
    if kind ~= "developer" and kind ~= "web" then return { content = "kind must be developer or web", is_error = true } end
    local limit = arguments.limit or 5
    if type(limit) ~= "number" or math.floor(limit) ~= limit or limit < 1 or limit > MAX_SOURCES then
        return { content = "limit must be an integer from 1 through 8", is_error = true }
    end
    local request = {
        path = "/v2/search",
        json = {
            query = query,
            limit = limit,
            sources = { "web" },
            ignoreInvalidURLs = true,
            scrapeOptions = scrape_options(),
        },
    }
    if kind == "developer" then request.json.categories = { "developer" } end
    local outcome = http_request(request)
    local body = response_json(outcome)
    if body == nil then
        return {
            content = "Web search failed through Firecrawl: " .. diagnostic(outcome) .. ".\n\nIf you already know likely public documentation, GitHub, or API URLs, use the bash tool with curl for direct retrieval where practical.",
            is_error = true,
        }
    end
    local results = parse_search_results(body)
    if results == nil then
        return { content = "Web search failed through Firecrawl: " .. (provider_diagnostic(body) or "invalid response") .. ".\n\nIf you already know likely public documentation, GitHub, or API URLs, use the bash tool with curl for direct retrieval where practical.", is_error = true }
    end
    if #results == 0 then return { content = "No web results found for: " .. text(query, 500) } end
    repair_missing_search_pages(results)
    return { content = render_sources("Web results for: " .. text(query, 500) .. "\nMode: " .. kind, results) }
end

local function handle_urls(arguments)
    if type(arguments.urls) ~= "table" or #arguments.urls < 1 or #arguments.urls > MAX_SOURCES then
        return { content = "urls must contain from 1 through 8 URLs", is_error = true }
    end
    local seen = {}
    local requests = {}
    for _, url in ipairs(arguments.urls) do
        if type(url) ~= "string" or trim(url) == "" or #url > MAX_URL_BYTES then
            return { content = "each URL must be a non-empty string no longer than 8192 bytes", is_error = true }
        end
        if seen[url] then return { content = "urls must not contain duplicates", is_error = true } end
        seen[url] = true
        table.insert(requests, scrape_request(url))
    end
    local outcomes = http_request_many(requests)
    local sources = {}
    local success_count = 0
    for index, url in ipairs(arguments.urls) do
        local markdown, failure = scrape_markdown(outcomes[index])
        if markdown ~= nil then
            local body = response_json(outcomes[index])
            local data = body.data or {}
            table.insert(sources, source(data.title, url, markdown, data.description, data.category))
            success_count = success_count + 1
        else
            table.insert(sources, { failed = true, url = url, reason = failure or diagnostic(outcomes[index]) })
        end
    end
    if success_count == 0 then
        return {
            content = "Web retrieval failed through Firecrawl.\n\n" .. text(sources[1].reason, 240) .. "\n\nFor known public URLs or APIs, fall back to the bash tool with curl where practical.",
            is_error = true,
        }
    end
    return { content = render_sources("Web sources", sources) }
end

local function has_only_fields(arguments, allowed)
    for key, _ in pairs(arguments) do
        if not allowed[key] then return false end
    end
    return true
end

return function(call)
    local arguments = call.arguments
    if type(arguments) ~= "table" then return { content = "web arguments must be an object", is_error = true } end
    local has_query = arguments.query ~= nil
    local has_urls = arguments.urls ~= nil
    if has_query == has_urls then return { content = "provide exactly one of query or urls", is_error = true } end
    if has_query then
        if not has_only_fields(arguments, { query = true, kind = true, limit = true }) then
            return { content = "query form accepts only query, kind, and limit", is_error = true }
        end
        return handle_query(arguments)
    end
    if not has_only_fields(arguments, { urls = true }) then
        return { content = "urls form accepts only urls", is_error = true }
    end
    return handle_urls(arguments)
end
]]
