return [[
Todo extension

Use `todo` for substantial multi-step work or an explicit user checklist.

Call `todo` with no arguments to read the complete durable plan.

Use `updates` to report status changes. The extension handles hierarchy,
recursive transitions, and automatic next-task selection.

Use `markdown` only to create or restructure the plan. Preserve existing
#IDs when editing returned Markdown; omit an ID for a new task.

Keep tasks concise and outcome-oriented. Mark work done immediately after
it is actually completed and verified. Block only on a real external
dependency; drop work only when intentionally removing it from scope.

Todo state is durable and may outlive context you can still see. After
compaction or resume, or whenever you are unsure of the current plan, read
the durable list instead of reconstructing it from memory.
]]
