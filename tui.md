Implement a scrollback-native terminal renderer for `tea` by introducing a new zero-dependency workspace crate, **`crates/tea-tui`**, and migrating the existing `tea-agent` presentation onto it.

This is a renderer/terminal-architecture migration, **not a UI redesign**.

The overriding acceptance criterion is:

> **Tea must look and interact the same as it does now.**

Use the existing real-binary `ptytest` suite as the visual oracle. Preserve its row geometry, text, cursor placement, colors, emphasis, menu placement, modal surfaces, multiline composer behavior, streaming behavior, and terminal restoration unless a test assertion specifically encodes the old alternate-screen/transcript-scrolling architecture.

The architectural change underneath that presentation is:

> **Normal conversation history is written once into the terminal's main-screen scrollback; tea retains and redraws only the small mutable suffix of the UI.**

Do not redesign tea while doing this.

One detail I feel particularly strongly about after inspecting the code: don't port grid.rs into tea-tui and call that the migration. The current Grid -> diff -> absolute cells architecture is exactly the machinery this design lets you delete. render.rs already contains the richer presentation semantics and an internal line abstraction; that is the seam to preserve while replacing the substrate.

---

# 1. Start from the existing behavioral contract

Read only the repository areas relevant to this migration:

* `AGENTS.md`
* `docs/tui.md`
* `docs/ptytest.md`
* root `tui.md`
* `crates/tea-agent/Cargo.toml`
* `crates/tea-agent/src/grid.rs`
* `crates/tea-agent/src/render.rs`
* `crates/tea-agent/src/terminal.rs`
* `crates/tea-agent/src/composer.rs`
* `crates/tea-agent/src/editor.rs`
* `crates/tea-agent/src/ui/frame_layout.rs`
* `crates/tea-agent/src/ui/visual_layout.rs`
* `crates/tea-agent/src/ui/theme.rs`
* `crates/tea-agent/src/app/runtime.rs`
* `crates/tea-agent/src/app/state.rs`
* `crates/tea-agent/src/app/input.rs`
* `crates/tea-agent/src/app/picker.rs`
* `crates/tea-agent/tests/pty_streaming.rs`

Do not map unrelated core/harness/session implementation.

Before changing behavior, run the current terminal tests on the pinned toolchain:

```sh
rustup run nightly-2026-07-24 cargo test \
    -p tea-agent \
    --features pty-harness \
    --test pty_streaming \
    --locked
```

Treat this as the pre-migration visual baseline.

Do **not** casually update PTY expectations to make the migration pass.

When an existing visual assertion fails, assume the implementation is wrong unless the assertion specifically depends on one of these intended semantic changes:

1. normal conversation mode is no longer in alternate screen;
2. tea no longer implements transcript PageUp/PageDown scrolling;
3. `/new` cannot erase already-written terminal scrollback;
4. already-committed historical rows are not re-rendered after terminal resize.

Everything else should remain visually stable.

---

# 2. Preserve the existing ownership boundaries

Do not move all terminal handling into `tea-tui`.

Today `tea-agent/src/terminal.rs` correctly owns OS-facing terminal concerns through `rustix`:

* raw mode
* termios restoration
* terminal size discovery
* resize polling
* stdin polling
* key decoding
* bracketed paste decoding
* suspend/resume around `$EDITOR`

Keep those responsibilities in `tea-agent`.

`tea-tui` must instead own only portable presentation mechanics:

* geometry value types
* tiny style/text primitives
* ANSI/VT rendering of the mutable inline region
* writing permanent transcript rows
* ephemeral alternate-screen rendering for modal surfaces
* renderer bookkeeping necessary to find/redraw the live region

The dependency direction is:

```text
tea-agent
    ├── rustix          # OS terminal/input ownership
    ├── laputa-hi-lite  # existing presentation/highlighting
    └── tea-tui         # zero-dependency rendering primitive

tea-tui
    └── std
```

`tea-tui` must have:

* zero normal dependencies
* zero dev-dependencies
* zero build-dependencies
* no unsafe
* no FFI
* no libc
* no async runtime
* no terminal input parser
* no terminal-size discovery
* no signal handling
* no dependency on any tea crate

Add it as a normal workspace member and a path dependency of `tea-agent`.

Follow the workspace's edition/lint/toolchain conventions.

---

# 3. The new terminal model

Tea has two presentation domains.

## A. Main-screen conversation mode

This is the default.

It runs in the terminal's **normal/main screen**, not alternate screen.

The display consists conceptually of:

```text
permanently committed transcript
permanently committed transcript
permanently committed transcript

──────── commit frontier ────────

mutable/unsettled transcript suffix
activity
composer
slash menu if open
footer/status
```

Everything above the commit frontier belongs to the terminal emulator.

Everything at or below the frontier is tea's small live region.

Central invariant:

> **A committed transcript row is never subsequently repainted by tea.**

Do not maintain a framebuffer for committed rows.

Do not implement an application scrollback buffer.

Do not implement transcript virtualization.

Do not implement historical reflow.

## B. Temporary full-screen surfaces

The existing:

* Help
* Model picker
* Custom-model picker
* Session picker
* Tool detail

are fundamentally different. They temporarily own an entire terminal viewport and disappear on Escape.

Preserve this behavior using the **alternate screen only while such a surface is open**.

Transitions should therefore be:

```text
normal conversation
    main screen
    alternate_screen = false

open /help, /model, /session, Ctrl+O, etc.
    enter alternate screen
    render existing full-screen surface

close surface
    leave alternate screen
    terminal restores untouched main-screen conversation
    redraw the current live tail
```

This is an intentional exception to the "main screen only" rule.

Never put the conversation transcript itself in alternate screen.

The distinction should be explicit in code rather than an accidental side effect of `TerminalGuard`.

---

# 4. `tea-tui` should be much smaller than the previous plan

Do not start by implementing a generalized Flexbox-like layout crate.

Tea already has the layout behavior we need.

The migration should first preserve `ui/frame_layout.rs::plan_flow` and `ui/visual_layout.rs` semantics exactly.

A tiny `tea-tui` geometry layer is enough:

```rust
pub struct Size {
    pub width: u16,
    pub height: u16,
}

pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}
```

Maybe add very small helpers if existing code benefits.

Do not introduce `Constraint`, Flexbox, CSS, Cassowary, percentages, a widget tree, or a general layout language unless migration of an existing tea layout proves one is actually needed.

The current domain-specific `plan_flow` is already small, deterministic, saturating, and tested. Preserve it rather than replacing a known visual contract with a new abstraction.

A generic splitter can be reconsidered after this migration if it proves useful.

---

# 5. Eliminate the full-screen cell grid

The architecture to remove is:

```text
AppState
  ↓
render() -> Grid(width × height)
  ↓
Grid::diff(previous_grid)
  ↓
FrameDiff<CellChange>
  ↓
absolute CUP + per-cell repaint
```

Today this lives across:

* `grid.rs`
* `render.rs`
* `terminal.rs`
* `App::previous_grid`
* `App::redraw`

Replace it with:

```text
AppState
  ↓
presentation projection
  ├── newly stable transcript lines ──→ commit once
  └── live rows ──────────────────────→ repaint small tail
```

The existing `RenderLine` abstraction in `render.rs` is already close to the right intermediate representation.

Promote/refine that line-oriented representation rather than inventing another cell engine.

A line should support the existing styling fidelity:

* default style
* foreground/background ANSI colors currently used by tea
* bold
* per-span/per-run styling sufficient to preserve syntax highlighting and current PTY color assertions

Prefer styled runs/spans over a `Vec<Style>` indexed by every character if that simplifies the code without changing rendered output.

Do not add a retained full-screen cell matrix to `tea-tui`.

When migration is complete, the old:

* `Grid`
* `Cell`
* `FrameDiff`
* `CellChange`
* `App::previous_grid`

should be gone unless a genuinely separate use remains.

`Rect`, `Color`, and `Style` may move into `tea-tui`.

---

# 6. Preserve current rendering semantics

This migration must not rewrite the visual language.

Keep the existing semantics for:

* welcome line
* user `┃ ` rail
* assistant Markdown presentation
* code blocks
* syntax highlighting
* diff styling
* tables
* tool lifecycle lines
* blank lines between transcript entries
* thinking/activity row
* queued-next-message row
* composer `┃ ` rail
* `┃↑` hidden-above composer indicator
* footer/model/context lines
* slash menu
* all temporary surfaces

The current renderer is allowed to be refactored from "paint into Grid" into "produce styled rows."

Do not simplify visible Markdown/highlighting as part of this project.

`laputa-hi-lite` remains a `tea-agent` presentation dependency; it does not belong in `tea-tui`.

Similarly, preserve the existing composer width logic and Unicode behavior in `visual_layout.rs`. Do not turn this migration into a Unicode rewrite.

---

# 7. Commit frontier: exploit tea's typed transcript

`AppState` already has exactly the information needed to decide whether a transcript entry may become permanent.

Introduce an explicit **committed transcript prefix** owned by the terminal projection layer.

Do not copy the transcript itself. Retain only something like:

```rust
committed_entries: usize
```

plus an explicit reset/generation marker if needed.

An entry is stable when:

* `Welcome` → stable
* `User` → stable
* `Notice` → stable
* `Error` → stable
* `Assistant { streaming: false }` → stable
* `Assistant { streaming: true }` → mutable
* `Tool { Completed | Failed }` → stable
* `Tool { Started | Progress }` → mutable

Only advance a **contiguous prefix**.

For example:

```text
User                    stable
Assistant streaming     mutable  ← frontier stops
Tool completed           stable but cannot commit yet
```

The later tool row remains in the live suffix until the assistant before it becomes stable. Transcript order must never change.

When the assistant settles:

```text
User                    already committed
Assistant final          commit
Tool completed           commit
                         ↑ frontier advances across both
```

This mechanism is preferable to trying to infer stability from rendered strings.

---

# 8. Streaming behavior

The existing OpenRouter PTY test proves that the first model delta becomes visible before the fixture releases the final delta.

Preserve that exactly.

A streaming assistant entry belongs to the mutable live transcript suffix.

As deltas arrive:

* recompute only the required live presentation
* display the newest content immediately
* do not commit that assistant entry yet
* do not append duplicate rows to terminal scrollback on each delta

When `MessageEnd` changes the entry to `streaming: false`:

1. clear/redraw the mutable region as necessary;
2. render the assistant's **final form** at the current width;
3. permanently commit it;
4. advance the stable-prefix frontier;
5. redraw activity/composer/footer.

The semantic assistant text already exists in `AppState`; no second model-stream buffer is required.

The same principle applies to mutable tool projections:

* Started/Progress stays live.
* Completed/Failed may cross the commit frontier.

If an unfinished response becomes many terminal rows, do not create an unbounded rendered framebuffer. `AppState` already owns the semantic text. Project only the currently relevant live rows necessary for the terminal viewport.

---

# 9. Preserve visual wrapping at commit time

This is a deliberate correction to the earlier generic design.

Do **not** switch tea's transcript to raw terminal soft-wrapping if doing so changes its appearance.

Tea currently performs width-aware presentation:

* every wrapped user line receives its `┃ ` rail;
* Markdown is rendered structurally;
* code blocks and tables are width-aware;
* styling is assigned before terminal output.

Those are visible behaviors.

Therefore:

> **Render a stable transcript entry into final tea-styled rows at the terminal width that exists when the entry is committed, and write those rows permanently.**

Use semantic CR/LF row boundaries for these already-rendered rows.

Once committed, those rows are immutable.

If the terminal is later resized, **do not go back and re-render them**.

This means old history may retain wrapping appropriate to its original width. That is the intended price of eliminating application-owned scrollback/reflow.

Only the live tail is re-laid-out on resize.

Document this explicitly.

Do not build a transcript-reflow engine to chase perfect historical resizing.

---

# 10. The live-tail renderer

Implement a tiny line-oriented renderer in `tea-tui`, roughly around:

```rust
pub struct InlineTerminal<W: Write> {
    // writer
    // current live-region row count
    // cursor bookkeeping relative to live origin
    // whether a modal alternate screen is active
}
```

The exact API should emerge from tea-agent's real call sites.

Useful operations will probably resemble:

```rust
draw_live(...)
commit(...)
enter_surface(...)
draw_surface(...)
leave_surface(...)
finish(...)
```

Do not freeze this exact API before integration proves it.

## Main-screen live redraw

Only repaint the current mutable rows.

Use ordinary ANSI/VT primitives:

* cursor movement
* carriage return
* erase line / erase below
* SGR
* cursor visibility
* synchronized output if useful

Do not repaint already-committed rows.

Do not clear the entire main screen.

In particular, main conversation mode must never use the old:

```text
ESC [ 2 J
ESC [ H
```

full-frame-reset strategy.

## Synchronized output

`ptytest`'s existing `xterm-minimal-v1` profile already permits DEC private mode `2026`.

Use synchronized output around a repaint if it makes the renderer cleaner and reduces flicker.

No capability query is necessary.

## Do not toggle DECAWM

Do **not** implement the earlier proposal of disabling terminal autowrap with `DECSET/DECRST ?7`.

The existing audited PTY protocol profile does not authorize it, and it is unnecessary.

Instead:

* tea already measures/clips each live row to the terminal width;
* emit explicit cursor positioning between physical rows;
* never rely on an overflowing printable line to move to the next row;
* ensure a full-width row does not create an accidental extra wrapped row before subsequent cursor control.

Add byte-level tests for the last-column case.

Do not modify `xterm-minimal-v1` merely to accommodate renderer convenience.

---

# 11. Preserve the startup layout exactly

The current PTY baseline at 80×16 includes:

```text
row 0: tea v...
row 2: ┃ <composer>
row 4: local/Laguna-XS-2.1-5bit · effort off
```

with the cursor at:

```text
row = 2
column = 2
visible = true
```

and the composer rail bold.

The new inline architecture should produce exactly that same visible startup layout inside a fresh PTY.

This is an important design test because it proves that moving from a full-frame renderer to main-screen flow does not force a visual redesign.

Preserve the existing breathing rows and footer spacing encoded by `plan_flow`.

---

# 12. Slash completion remains an inline live surface

The slash menu is not a full-screen modal.

Keep it in the main-screen mutable tail.

Its current measured geometry is part of the visual contract.

At the existing 80×16 PTY fixture, preserve assertions such as:

* divider at row 3
* `/help` beginning on row 4
* navigation hint on row 11
* current selected-command accent styling

The fact that it now lives in a bounded mutable main-screen tail instead of a full `Grid` must not be visible.

---

# 13. Full-screen surfaces use ephemeral alternate screen

Refactor terminal mode ownership so `TerminalGuard::enter()` no longer enters alternate screen globally.

Normal activation should own roughly:

* raw mode
* bracketed paste
* cursor lifecycle

Conversation mode remains in the main screen.

Add an explicit modal-surface transition for:

```text
UiSurface::Help
UiSurface::ModelPicker
UiSurface::CustomModel
UiSurface::SessionPicker
UiSurface::ToolDetail
```

On transition:

```text
UiSurface::None -> non-None
```

enter alternate screen and render the existing full-frame surface.

On:

```text
non-None -> UiSurface::None
```

leave alternate screen and redraw the main-screen live tail.

Track the state explicitly so cleanup remains correct on:

* normal exit
* errors
* panic/unwind where applicable
* `$EDITOR` suspension
* picker transitions

Do not repeatedly enter/leave alternate screen on every frame.

The existing full-screen visual shape should remain:

* `┃ ` first row
* divider
* content
* lower divider
* navigation hint

No visual redesign.

---

# 14. Adapt `TerminalGuard`, don't replace it

Refactor `tea-agent/src/terminal.rs` around the existing host responsibilities.

Keep:

* `rustix`
* `tcgetattr` / `tcsetattr`
* raw mode
* `poll`
* winsize
* `InputDecoder`
* bracketed paste
* `TerminalEvent`
* terminal restoration

Remove from it:

* `FrameDiff`
* per-cell painting
* full-screen clear/redraw logic
* unconditional alternate-screen entry

It should expose its stdout writer/render boundary cleanly enough for `tea-tui` to emit terminal output without duplicating terminal ownership.

Prefer a small composition API over making `tea-tui` know anything about `rustix`.

---

# 15. `$EDITOR` must remain correct

The external-editor path currently suspends the terminal, launches `$EDITOR`, and resumes.

Preserve this.

Before suspension:

* clear/finish the tea live tail cleanly;
* restore cursor/style state;
* leave an active modal alternate screen if necessary;
* disable bracketed paste;
* restore termios.

After the editor exits:

* restore raw mode/bracketed paste;
* re-establish the current presentation mode;
* redraw the live tail or active surface.

Do not rebuild committed transcript history.

Remove the old `previous_grid = None` invalidation once the Grid no longer exists.

---

# 16. Remove application-owned transcript scrolling

Native terminal scrollback replaces tea's normal transcript viewport.

The following current concepts should disappear from normal conversation mode when no longer needed:

* `viewport_offset`
* `follow_output`
* `visible_transcript_lines`
* `transcript_rows`
* `set_viewport_metrics`
* transcript `page_up`
* transcript `page_down`
* `follow_end`
* `render::transcript_metrics`

Do not confuse these with:

* semantic `AppState::transcript`
* prompt history
* `surface_offset`

Those remain valid.

In particular:

> `AppState::transcript` is semantic agent/session projection state, **not a terminal scrollback buffer**.

It remains necessary for rendering live entries, rebuilding a resumed session, tool detail, durable projection, etc.

The thing being removed is the **rendered transcript viewport**, not the semantic transcript.

## Input behavior

While a temporary full-screen surface is active, retain its current application-owned Up/Down/PageUp/PageDown behavior through `surface_offset`.

In normal conversation mode, do not intercept PageUp/PageDown to implement transcript scrolling.

Let the user's terminal emulator own its normal scrollback interaction.

Do not invent key-sequence forwarding or fake scrollback controls.

---

# 17. Session boundaries need a projection generation

Native terminal scrollback cannot be erased semantically.

That affects `/new` and `/session`.

## `/new`

`/new` should still clear the current semantic Tea session exactly as it does now:

* durable harness/session state
* semantic transcript
* prompt history
* queued input
* composer
* context state

But previously printed terminal history remains above in the user's native scrollback.

That is expected and desirable.

The visible new-session UI should remain coherent and the existing PTY interaction should still find the "new session" state.

Do not attempt to erase prior main-screen scrollback.

## Resume

When a saved session is resumed, `restore_messages()` rebuilds its semantic transcript.

Those restored messages now need to be **printed as a new projection into the main-screen terminal history**.

Reset the commit frontier and commit the restored stable transcript from the beginning.

The existing PTY test that resumes the session and expects the earlier assistant response to become visible must continue to pass.

## Explicit reset mechanism

Introduce the smallest explicit mechanism that prevents the renderer from confusing one semantic transcript with another.

For example:

```rust
projection_generation: u64
```

or an App-owned reset flag/frontier generation.

Increment/reset it on semantic transcript replacement such as:

* `/new`
* restored durable session

Avoid content comparison or heuristics.

---

# 18. PTYTEST IS THE VISUAL ORACLE

The real-binary PTY suite is the primary acceptance harness for this migration.

Do not replace it with synthetic renderer-only tests.

Keep its fixture synchronization model:

* semantic screen predicates
* explicit OpenRouter fixture barriers
* no arbitrary settle sleeps
* existing `xterm-minimal-v1` protocol profile

## Existing behaviors that must still pass visually

Preserve:

### Startup

* welcome text
* composer row
* footer rows
* cursor row/column
* cursor visibility
* composer emphasis

### Composer

* normal editing
* multiline bracketed paste
* history
* cursor movement
* current wrapping behavior

### Slash completion

* exact divider/menu/footer placement
* selection styling
* Escape behavior
* Tab completion

### Model/session surfaces

* visible contents
* navigation
* selection
* Escape restoration

### Help

* existing full-screen geometry
* exact lower divider/hint placement
* Escape restoration

### Streaming

The first released OpenRouter token must appear **before** the fixture releases the completion records.

The unreleased second token must remain absent.

After release, both must appear.

### Resize

The application must remain coherent after the existing 40×10 kernel PTY resize.

Only live content needs to be relaid out.

Do not add an assertion claiming old committed transcript rows were historically reflowed.

### Exit

Terminal state must still restore exactly.

---

# 19. Intended PTY assertion changes

Make very few test changes.

## Normal mode

The current test says:

```rust
assert!(active.modes.alternate_screen);
```

Change this contract to:

```rust
assert!(!active.modes.alternate_screen);
```

Normal conversation mode is now main-screen native-scrollback mode.

Continue asserting:

```text
bracketed_paste = true
cursor_visible = true
```

## Modal surfaces

Add mode assertions around at least one representative existing full-screen surface:

```text
normal:
    alternate_screen = false

open /help or /model:
    alternate_screen = true

Esc:
    alternate_screen = false
```

Then assert that the restored main-screen composer/status presentation is visually unchanged.

This directly verifies the new architectural boundary.

## Everything else

Do not rewrite existing row/cell/color assertions merely because the implementation changed.

They are the oracle.

---

# 20. Add one overflowing-transcript PTY regression

The current suite is strong but should gain one targeted scenario proving the new main-screen behavior under enough output to exceed terminal height.

Use the existing deterministic/mock or local fixture infrastructure; do not involve a real provider.

Drive the real binary until committed transcript output exceeds the PTY height.

Prove at least:

* normal mode remains `alternate_screen = false`;
* final committed content is visible;
* composer/footer remain coherent after settlement;
* repeated live spinner/stream redraws do not visibly duplicate output;
* the process remains interactive;
* terminal restoration still succeeds.

Do **not** claim this PTY assertion proves terminal-emulator scrollback retention.

`ptytest`'s current semantic backend intentionally does not model that as a public contract.

The architecture itself plus byte-level renderer tests provide that proof.

---

# 21. Unit-test the scrollback protocol invariants inside `tea-tui`

Because `tea-tui` is generic over `std::io::Write`, test emitted bytes directly with `Vec<u8>`.

These tests should establish things the PTY visual backend cannot.

At minimum prove:

### Committed history

* committing rows emits normal terminal text and row boundaries;
* committed rows are written only once;
* later `draw_live` calls never repaint committed contents;
* commits never issue whole-main-screen clear;
* commits never enter alternate screen;
* committing enough rows naturally emits forward line movement rather than implementing an internal viewport.

### Live tail

* repeated identical/revised live draws do not append a new transcript copy;
* growing and shrinking the tail leaves no stale visible rows;
* drawing can return to the live origin deterministically;
* no arithmetic underflow on tiny terminals;
* zero width/height does not panic;
* full-width lines do not accidentally generate an extra row;
* cursor placement is deterministic.

### Modes

* synchronized-output begin/end are paired if used;
* SGR ends reset;
* cursor visibility is restored as intended;
* main mode does not contain `?1049h`;
* modal surface mode does use paired `?1049h` / `?1049l`;
* no DECSTBM partial scrolling region is emitted;
* no DECAWM `?7` toggles are emitted.

### Safety

Do not allow arbitrary model/tool/repository text to inject terminal control sequences.

If existing tea rendering currently assumes strings are safe, introduce the narrowest text-escaping boundary needed in `tea-tui` without changing visible normal text.

Internal tea-generated styling/control sequences remain trusted.

---

# 22. Keep the existing PTY protocol profile

Do not expand `xterm-minimal-v1` unless an unavoidable and reviewed terminal semantic requires it.

The planned renderer fits within its existing surface:

* CR/LF
* cursor movement
* erasure
* SGR
* cursor visibility
* alternate screen
* bracketed paste
* synchronized output

Specifically avoid adding:

* DECSTBM
* autowrap mode toggling
* OSC features
* terminal queries
* private protocols

A smaller output vocabulary is a design goal.

---

# 23. Rendering surfaces without a cell framebuffer

For ordinary inline mode, prefer a projection shaped approximately like:

```rust
struct Presentation {
    commit: Vec<StyledLine>,
    live: Vec<StyledLine>,
    cursor: Option<Cursor>,
}
```

This is conceptual only.

Do not allocate this exact structure if a streaming iterator/slice design is simpler.

For an active modal surface:

```text
render_surface()
    -> styled viewport rows
    -> tea-tui alternate-screen renderer
```

It is acceptable to regenerate every modal row on every modal redraw. These surfaces are small and ephemeral.

There is no reason to introduce per-cell diffing for them.

---

# 24. Preserve current layout code before generalizing it

The earlier `tea-tui` plan proposed a `Constraint`-based split engine.

Do not implement that merely because it was in the old plan.

First migrate these existing behaviors:

```text
frame_layout::plan_flow
visual_layout::VisualLayout
composer viewport
slash menu
footer placement
surface rows
```

Once all PTY visual tests pass, inspect what geometry functionality actually remains duplicated.

Only then, if there is an obvious tiny reusable primitive, move it into `tea-tui`.

The ideal outcome may simply be:

```text
tea-tui:
    Size
    Rect
    Style/Color
    StyledLine/Span
    InlineTerminal

tea-agent:
    tea-specific frame_layout
    composer measurement
    Markdown rendering
    menus
    transcript presentation
```

That is preferable to prematurely turning `tea-tui` into Ratatui-lite.

---

# 25. Migration sequence

Implement this incrementally and keep the PTY oracle green as early as possible.

## Phase A — lock behavior

1. Run the existing PTY suite unchanged.
2. Record any useful existing semantic snapshots before implementation if the harness already has an appropriate snapshot location.
3. Do not change presentation.

## Phase B — create `tea-tui`

Implement:

* `Size`
* `Rect`
* `Color`
* `Style`
* small styled-line/span representation
* generic `InlineTerminal<W: Write>`
* main-screen live-tail redraw
* permanent commit
* modal alternate-screen operations
* byte-level unit tests

Zero dependencies.

Do not integrate it with Tea yet beyond compiling it.

## Phase C — line-oriented Tea presentation

Refactor `render.rs` so existing presentation functions produce styled lines instead of painting cells.

Keep visual logic unchanged.

Preserve existing renderer unit tests and add direct line-projection assertions where useful.

## Phase D — main-screen conversation integration

Replace:

```text
Grid
previous_grid
FrameDiff
TerminalGuard::draw
```

with:

```text
commit frontier
tea-tui InlineTerminal
live presentation
```

At this point:

* startup must visually match;
* composer must match;
* footer must match;
* normal PTY mode must report alternate screen false.

Get those gates green before proceeding.

## Phase E — streaming/tools

Wire stable-prefix advancement to:

* assistant streaming/final state
* tool Started/Progress/Completed/Failed

Get the existing OpenRouter streaming PTY test green.

## Phase F — menus and modal surfaces

Keep slash completion inline.

Move full-screen surfaces to ephemeral alternate-screen transitions.

Get existing help/model/session behavior green and add mode-transition assertions.

## Phase G — sessions and native transcript semantics

Introduce/reset projection generation for:

* `/new`
* session resume

Remove app-owned transcript viewport scrolling.

Add overflowing-transcript regression.

## Phase H — delete obsolete architecture

Remove:

* full Grid framebuffer
* FrameDiff
* CellChange
* `previous_grid`
* transcript viewport offsets/metrics
* dead full-frame terminal drawing code
* obsolete tests that only prove removed implementation details

Do not leave compatibility layers for the old renderer.

This project is allowed to make a clean internal cut.

---

# 26. Failure handling

Terminal writes can fail after only part of a sequence has reached the PTY.

The old renderer responded by discarding `previous_grid` and forcing a full repaint.

That recovery strategy no longer applies because committed history must never be repainted.

For the new renderer:

* propagate the I/O error;
* make best-effort mode/style/cursor restoration;
* do not attempt to reconstruct potentially partially committed history;
* terminate cleanly if the renderer can no longer know its live-region position.

A fatal terminal I/O error is preferable to violating the committed-history invariant.

Keep state transitions explicit enough that `Drop`/`finish()` can safely leave:

* alternate screen
* synchronized output
* styling
* cursor visibility

in a sane state.

---

# 27. Security boundary

Terminal output may contain:

* model-generated text
* tool output
* filenames
* repository contents
* errors

Treat these as untrusted text.

`tea-tui` should ensure arbitrary ESC/C0/C1 terminal-control injection cannot escape through ordinary styled-text APIs.

Do not alter legitimate Unicode/plain-text appearance.

Keep trusted ANSI construction internal to `tea-tui`.

Do not create a generic terminal sanitizer dependency.

---

# 28. Documentation changes

Update `docs/tui.md` to describe the durable architecture accurately:

```text
DurableHarness/session
        ↓
semantic AppState transcript
        ↓
presentation projection
        ↓
┌──────────────────────────────────┐
│ stable prefix → native scrollback│
│ mutable suffix → live tail       │
└──────────────────────────────────┘
```

Explain clearly:

* semantic transcript remains in AppState;
* rendered transcript viewport no longer exists;
* terminal emulator owns history navigation;
* settled history is immutable after being written;
* historical wrapping is not recomputed after resize;
* full-screen surfaces temporarily use alternate screen;
* slash completion stays inline;
* `/new` starts a new semantic session but does not erase terminal scrollback.

Update `docs/ptytest.md` with the new mode contract and the explicit boundary that ptytest verifies visible terminal semantics but does not claim terminal-emulator scrollback/reflow behavior.

Remove or replace the old root `tui.md` plan once this migration is complete so it cannot become stale architecture documentation.

---

# 29. Hard non-goals

Do not add any of the following:

* Ratatui
* Taffy
* Cassowary/Kasuari
* generalized Flexbox
* widget framework
* component tree
* retained full-screen framebuffer
* per-cell diffing
* application-owned normal transcript scrollback
* transcript virtualization
* historical transcript reflow
* terminal emulator
* terminfo
* terminal capability detection
* DECSTBM scrolling regions
* mouse support
* async runtime in `tea-tui`
* new Unicode-width dependency
* new PTY dependency
* dependency from `tea-tui` to `tea-agent`
* second semantic transcript representation

Do not modify `laputa-systems/ptytest` merely to get this migration through.

If a genuine ptytest limitation is encountered, first prove whether byte-level `tea-tui` tests can establish that invariant instead.

---

# 30. Size discipline

`tea-tui` should remain aggressively small.

Target approximately:

```text
≤ 1,000 non-test LOC
```

Treat significant growth beyond that as evidence the boundary has expanded too far.

Tests may exceed implementation size.

Tea-specific Markdown, composer, model picker, session logic, input decoding, and terminal OS ownership do not belong in the crate.

Optimize for code that can be understood nearly in one sitting.

---

# 31. Acceptance gates

The migration is complete only when all of these are true.

## Architecture

* `crates/tea-tui` exists.
* It has zero dependencies of every kind.
* Normal tea conversation mode never enters alternate screen.
* Stable transcript history is written once.
* Only mutable tail rows are repainted.
* Full-screen modal surfaces use alternate screen only while active.
* There is no retained width×height screen grid.
* There is no `previous_grid`.
* There is no application-owned normal transcript viewport.
* `AppState` still retains the semantic typed transcript.
* Session restore starts a fresh terminal projection correctly.
* `/new` does not attempt to erase native scrollback.

## Visual parity

Except for terminal-mode/scrollback semantics, the existing PTY presentation remains unchanged:

* startup row positions
* composer rail
* cursor
* styles
* footer
* multiline input
* slash menu geometry
* slash-menu colors
* help geometry
* model/session picker presentation
* streaming
* queued-message behavior
* terminal resize of the live UI
* terminal restoration

Do not accept "roughly equivalent."

The existing semantic PTY assertions are intended to catch accidental movement by even one terminal row.

## Scrollback architecture

Byte-level tests prove:

* commits are ordinary main-screen line output;
* committed content is never subsequently redrawn;
* live refreshes do not add duplicate transcript lines;
* no full-screen clears occur in main conversation mode;
* no DECSTBM is used;
* no DECAWM toggle is used.

## Terminal lifecycle

PTY tests prove:

```text
normal conversation:
    alternate_screen == false
    bracketed_paste == true
    cursor_visible == true

temporary full-screen surface:
    alternate_screen == true

after closing surface:
    alternate_screen == false

after exit:
    original terminal state restored
```

---

# 32. Verification

Run the repository's pinned toolchain.

At minimum:

```sh
rustup run nightly-2026-07-24 cargo fmt --all --check

rustup run nightly-2026-07-24 cargo test \
    -p tea-tui \
    --locked

rustup run nightly-2026-07-24 cargo clippy \
    -p tea-tui \
    --all-targets \
    -- \
    -D warnings

rustup run nightly-2026-07-24 cargo test \
    -p tea-agent \
    --lib \
    --locked

rustup run nightly-2026-07-24 cargo test \
    -p tea-agent \
    --features pty-harness \
    --test pty_streaming \
    --locked

rustup run nightly-2026-07-24 cargo test \
    --workspace \
    --locked

cargo tree -p tea-tui -e all
```

Confirm from `cargo tree` and `Cargo.toml` that `tea-tui` has exactly zero dependencies.

Search the final code to confirm there are no main-screen:

* full clears
* DECSTBM sequences
* DECAWM toggles

and that alternate-screen entry is reachable only through explicit ephemeral-surface handling.

---

# 33. Final report

When done, report concisely:

1. final `tea-tui` public API;
2. `tea-tui` non-test LOC;
3. old rendering machinery deleted;
4. how stable-prefix commitment works;
5. how modal alternate-screen transitions work;
6. which AppState viewport fields were removed;
7. PTY visual tests run;
8. new scrollback-specific tests;
9. zero-dependency proof;
10. any intentionally unsupported resize/scrollback edge cases.

Do not report completion merely because unit tests pass. The real-binary PTY visual oracle must pass.

The final architecture should be explainable in two sentences:

> **Tea keeps its semantic conversation state, but not a rendered transcript viewport. Settled rows are written once into native terminal history; only unfinished conversation state and the composer/footer are redrawn.**

And:

> **Normal conversation lives on the main screen; truly modal full-screen surfaces borrow the alternate screen temporarily and give it back on close.**
