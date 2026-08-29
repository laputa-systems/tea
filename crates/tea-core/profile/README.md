# Historical Pi profile evidence

`default-profile.json` is immutable captured Pi profile data retained for
historical review. It is not Tea's production coding configuration and does
not register any model-facing tool.

Tea's production coding surface is the checked-in Luau builtins in
`crates/tea-luau/builtins/{read,bash,edit,find}/`; its host-side authority tests are in
`crates/tea-core/tests/coding_capabilities.rs`. No historical capture check may
consult a repository cwd, credentials, sessions, a live provider, or a
host-installed Pi executable.
