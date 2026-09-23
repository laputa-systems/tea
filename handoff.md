Restored: the four tracked artifact directories are back, and the diff is down to the 29 files I intended to change. (They look like PTY failure logs committed by accident, but whether to remove them is your call.)

## Summary

HEAD didn't compile. It now builds cleanly in every feature configuration, and every automated check in the plan passes except the live-inference rows, which stay BLOCKED. Nothing is committed.

**Fixed to compile:** a missing test module, a missing import, a move out of an `Arc`, and a `for` loop over a type that isn't iterable. I also cleared the warnings that only showed up with certain features enabled.

**Bugs found and fixed** (each has a regression test that fails without the fix, except the PTY test fixes):
1. **TUI never showed your submitted message** once the runtime owned the input queue. User rows now appear when the input is actually sent to the model, tagged with its session entry ID so scrollback can't duplicate them.
2. **Ctrl-C right after a turn was taken as "cancel"** because a brief background check held a task. That check now finishes on the spot when nothing is queued.
3. **Typing during an active turn was rejected** with "recovery requires /continue", because the running operation looked like an interrupted one.
4. **The todo list could disappear from the TUI.** Its progress preview was dropped when the tool finished; the latest todo text is now kept.
5. **Session reopen was broken** in several places: the TUI checked for header kind `"session"` instead of `"tea-session"`. There is now one shared `SESSION_HEADER_KIND` constant.
6. **Compaction failed** whenever a failed tool call was in the part of history being kept, and also on a usage-field mismatch. The encoder now matches what a reopen reconstructs.
7. **Extension state changed on reopen:** Luau produces `Signed(1)` where decoding gives `Unsigned(1)`. The committed value is now normalized to what a reopen produces.
8. **Flaky child cleanup (~40% failure):** after cancellation, two `git` processes raced on the same child worktree's index lock. Git work is now serialized per worktree; the flaky test passed 12/12.
9. **Harness authoring could never succeed:** the allowed-capabilities set was empty while the built-in coding plugins require four capabilities. The live evolution prompt also told the model to edit `todo`, which is locked. The scripted test caught both; the scenario now adds a new session plugin that needs no capabilities.
10. **Two live scenarios reopened while still holding the session writer,** so the reopen was correctly refused as a second writer. They now release it first.

**Judgment calls to review:**
- I changed a compaction test to expect an error instead of a failed-but-returned result. Every effect-gate rejection fails closed, and no code path returns the shape the test wanted.
- A test provider ignored cancellation, which hung a test forever. I made the fixture honor cancellation rather than change the core.
- After a user cancels, the footer now reads "turn cancelled; input kept in the session" instead of "core run error: …".

**Still blocked:** the six live cases need an `OPENCODE_API_KEY` and your go-ahead to use `--live`. I ran the report without `--live`: all six offline counterparts passed, no credential was read, and nothing was sent.
