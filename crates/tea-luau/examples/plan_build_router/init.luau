--!strict
-- Plan, then build: an explicit two-phase virtual model.
--
-- Selecting `virtual/plan-build` plans on the first host-approved model and
-- implements on the second (or stays on the first when only one is approved).
-- A session starts in the planning phase. After a turn in which an `edit` or
-- `write` call succeeded, the next user turn moves to the build phase and the
-- session stays there. A prompt that starts with "plan:" returns to planning.
--
-- The phase is this extension's bounded per-lane state, so it follows branch
-- forks and reopen. Continuations stay sticky (the default): tool follow-ups
-- and retries keep the physical model their turn started on.

local PLAN = '{"phase":"plan"}'
local BUILD = '{"phase":"build"}'
local EDIT_TOOLS = { edit = true, write = true }

local function edited(results)
	for _, result in ipairs(results) do
		if EDIT_TOOLS[result.name] and not result.is_error then
			return true
		end
	end
	return false
end

local function route(request)
	local planner = request.targets[1]
	local builder = request.targets[2] or planner
	local phase = if request.state then request.state.phase else nil
	local text = string.lower(request.last_user_text or "")

	if string.sub(text, 1, 5) == "plan:" then
		if phase == "plan" then
			return { target = planner }
		end
		return { target = planner, state = { content_json = PLAN } }
	end
	if phase == "build" then
		return { target = builder }
	end
	if edited(request.previous_turn_tool_results) or edited(request.tool_results) then
		return { target = builder, state = { content_json = BUILD } }
	end
	if phase == "plan" then
		return { target = planner }
	end
	return { target = planner, state = { content_json = PLAN } }
end

return {
	prompt_sections = {},
	state_version = "plan-build.v1",
	virtual_models = {
		{
			id = "plan-build",
			name = "Plan, then build",
			route = route,
		},
	},
}
