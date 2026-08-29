return [[
Workspace commands

Use `bash` for bounded foreground commands, text search with `rg` or `grep`,
Git, builds, and ordinary directory inspection. Omitted timeouts receive a
finite host default. Do not use ordinary `bash` to create a durable background
service: it cleans up its owned foreground process scope before it settles.
There are no separate `write`, `grep`, or `ls` tools.
]]
