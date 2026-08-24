return [[
Goal extension

An active goal is a durable, multi-turn objective owned by the goal extension.
Use `get_goal` to inspect it. Use `create_goal` only when no unfinished goal
exists. When the objective is genuinely achieved, call `update_goal` with
`status = "complete"`. When the same concrete blocker has prevented progress
for three consecutive goal turns, call `update_goal` with `status = "blocked"`
and that blocker. Do not mark a goal complete merely because a turn ended.
]]
