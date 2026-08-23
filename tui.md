Implement a new workspace subcrate named **`tea-tui`** (`tea_tui` in Rust code).

The goal is an extremely small terminal UI core for a coding agent. It must use the terminal emulator's **native main-screen scrollback as the transcript** and own only a small mutable live region at the end of normal terminal output.

Do not build a conventional full-screen TUI.

## Core model

The UI has exactly two classes of output:

1. **Committed transcript**

   * Ordinary terminal output.
   * Written permanently into the main screen buffer.
   * Naturally enters the terminal emulator's scrollback.
   * Never retained as rendered cells by `tea-tui`.
   * Never repainted by `tea-tui`.
   * Never application-reflowed after commitment.

2. **Live tail**

   * A small mutable set of terminal rows immediately following committed output.
   * Used for things such as:

     * unfinished streaming output
     * current tool status
     * approval prompt
     * composer/input
     * status/footer
   * May be cleared and redrawn freely.
   * Retaining enough state to redraw this small region is fine.

The central invariant is:

> **Committed output is never repainted.**

The terminal emulator owns transcript scrolling, selection, copy/paste, and reflow.

## Hard constraints

`tea-tui` must have:

* zero normal dependencies
* zero dev-dependencies
* zero build-dependencies
* no unsafe code
* no FFI
* no libc
* no async runtime
* no alternate-screen usage
* no application-owned scrollback buffer
* no retained full-screen framebuffer
* no general constraint solver
* no terminal input/event parser
* no terminal-size discovery
* no signal handling

Use only `std` and ANSI/VT output sequences.

The caller is responsible for:

* terminal raw mode, if desired
* keyboard/input handling
* SIGWINCH or equivalent resize detection
* supplying the current terminal size
* semantic conversation state
* model/tool execution

The crate should work anywhere ordinary ANSI/VT terminal control works. Do not add platform-specific implementations in v0.

## First inspect the repository

Before changing anything:

* inspect the workspace structure and conventions
* inspect existing terminal/UI code if any
* determine the workspace Rust edition and MSRV policy
* follow existing naming, linting, and formatting conventions
* add `tea-tui` as a normal workspace member

Do not perform unrelated refactors.

Do not stop for clarification unless the repository contains a genuine contradiction that makes implementation impossible. Make conservative decisions and continue.

---

# 1. Geometry and layout

Keep layout independent from terminal rendering.

Start with tiny value types approximately like:

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

pub enum Axis {
    Horizontal,
    Vertical,
}
```

Implement a small deterministic splitter.

A suitable sizing vocabulary is:

```rust
pub enum Constraint {
    Fixed(u16),

    Content {
        desired: u16,
        min: u16,
        max: u16,
    },

    Fill {
        weight: u16,
        min: u16,
        max: u16,
    },
}
```

Exact naming may change if a simpler API emerges.

Important: **`Content` does not measure text.** The caller supplies its desired extent.

The splitter should accept:

* parent `Rect`
* axis
* constraints
* optional fixed gap
* caller-provided output slice if practical

and produce child rectangles.

### Allocation rules

Keep the algorithm simple and explicit:

1. subtract gaps using saturating arithmetic
2. allocate fixed tracks
3. clamp content tracks to `[min, max]`
4. allocate their desired sizes where space permits
5. distribute remaining space among `Fill` tracks by integer weight
6. respect min/max bounds
7. distribute integer remainders deterministically from first to last
8. if space is insufficient, shrink without underflow or panic
9. rectangles must never escape the parent rectangle

No floating point.

No percentage sizing unless an actual repository use-case proves it necessary.

No CSS compatibility.

No Cassowary/Kasuari-style solver.

Useful trivial `Rect` helpers such as `inset`, `contains`, and `intersection` are acceptable if they remain tiny.

### Layout tests

Test aggressively with ordinary unit tests:

* empty areas
* zero constraints
* zero-sized tracks
* gaps larger than available space
* fixed tracks exceeding available space
* weighted fills
* min/max clamping
* odd integer remainders
* terminal dimensions of 0 and 1
* horizontal and vertical symmetry
* overflow-resistant arithmetic near `u16::MAX`

No property-testing dependency. Small exhaustive loops are preferable.

---

# 2. Terminal architecture

Create a small type along the lines of:

```rust
pub struct InlineTerminal<W: Write> {
    // writer
    // live-region bookkeeping only
}
```

It must operate exclusively in the **normal/main terminal screen**.

Never emit alternate-screen enter/leave sequences.

Never use a partial DECSTBM scrolling region in v0.

The implementation should use only relative cursor movement, carriage return, erase-line/display operations where necessary, ANSI styling, and ordinary terminal output.

## Live-tail bookkeeping

Track only what is necessary to locate and repaint the mutable tail, for example:

* number of currently drawn live rows
* number of physical live rows already reserved
* terminal cursor's row relative to the live-tail origin
* last known terminal size, if useful
* optionally the serialized contents of the tiny live tail if that materially simplifies committing

Do **not** retain previous committed transcript output.

It is acceptable for the live tail to retain a handful of strings/rows. That is not the scrollback buffer we are avoiding.

---

# 3. Line-oriented renderer, not a cell engine

Do not build Ratatui again.

The live renderer should be **line-oriented**, not a full cell framebuffer.

The live tail is expected to be on the order of 1–15 rows. Repainting those rows completely is cheap.

Therefore:

* no per-cell diff algorithm
* no retained screen matrix
* no damage tracking
* no widget tree

On each live repaint:

1. enter a synchronized-output update if supported by ordinary VT escape sequences
2. temporarily hide the cursor
3. move to the known live-tail origin
4. clear the rows previously owned by the tail
5. ensure enough rows exist if the new tail grew
6. redraw complete live rows
7. restore/reset SGR state
8. restore autowrap
9. position or hide the cursor as requested
10. end synchronized output

Unknown synchronized-output escape sequences may simply be ignored by terminals that do not support them. Do not introduce capability detection.

## Important live-row rule

Render live rows with **terminal autowrap disabled**.

Every logical live row must correspond to exactly one physical terminal row.

This substantially simplifies cursor accounting and prevents ordinary width changes from turning one live row into several soft-wrapped rows.

Long live lines may be clipped at the terminal edge.

That is acceptable in v0.

Committed transcript output, in contrast, must have normal terminal autowrap **enabled**.

---

# 4. Native-scrollback commitment

Provide a clean operation for permanently committing transcript output.

Conceptually:

```rust
ui.commit_text("...");
```

When committing:

1. locate the beginning of the current live tail
2. erase the live tail
3. enable normal terminal autowrap
4. write the committed text as ordinary terminal output
5. finish on a clean line boundary
6. establish the new live-tail origin
7. redraw the live tail if the API retains it, or make the next explicit live redraw trivial

The terminal must naturally scroll when committed output reaches the bottom of the screen.

That scrolling is exactly what we want.

## Do not application-wrap committed text

This is critical.

Never insert newlines merely because text reached the current terminal width.

Only semantic line breaks should become line breaks.

Allow the terminal emulator to wrap long lines. This allows the terminal itself to handle scrollback reflow after resizing.

In particular, avoid creating a transcript-reflow subsystem.

Normalize semantic line endings appropriately for terminal output so a newline reliably starts at column zero.

---

# 5. Streaming-agent usage

Design the API around this coding-agent pattern:

```text
committed transcript:
    user messages
    completed assistant paragraphs/lines
    completed tool calls
    completed tool results

live tail:
    unfinished streaming suffix
    running tool/status
    approval interaction
    composer
    status line
```

Application code should be able to gradually promote stable output from live state into committed terminal history.

The simplest acceptable policy is:

> Commit streaming assistant output on complete semantic newline boundaries.

Do not make `tea-tui` parse Markdown.

Do not make it understand model streams.

It merely provides the primitives necessary for the caller to implement this policy.

---

# 6. Safe text output

Do not pass arbitrary model/tool/repository strings directly through as trusted ANSI.

A coding agent routinely displays untrusted terminal-like data, and terminal escape injection is unacceptable.

Provide safe text-writing primitives that:

* permit ordinary printable UTF-8
* normalize intended newlines
* do not allow arbitrary ESC sequences
* neutralize destructive C0/C1 controls such as ESC, BEL, backspace, carriage-return rewriting, etc.
* handle tabs using one simple documented policy

ANSI generated internally by `tea-tui` for styling and cursor control remains trusted.

If styled text is useful, implement only a very small safe representation, for example:

```rust
pub struct Style {
    pub bold: bool,
    pub dim: bool,
    pub reverse: bool,
    pub fg: Option<Color>,
}
```

Keep the color model small.

Do not implement a rich text system.

---

# 7. Unicode scope

Do not import or recreate Unicode width/grapheme databases.

Committed UTF-8 may be passed safely to the terminal because the application does not calculate its wrapped layout.

For the live tail:

* rows are physically protected by disabled autowrap
* long content may be clipped
* the caller may supply cursor columns if it knows them
* exact emoji/CJK/grapheme measurement is explicitly outside v0

Do not let Unicode edge cases turn this crate into a text-layout engine.

Document this limitation clearly.

---

# 8. Cursor handling

Support a minimal optional physical cursor for input.

A reasonable API is something like:

```rust
pub struct Cursor {
    pub row: u16,
    pub column: u16,
    pub visible: bool,
}
```

Coordinates are relative to the live tail.

The caller is responsible for determining a correct visual column for complex Unicode input.

If no cursor is requested, hiding the physical terminal cursor is acceptable.

Maintain enough internal bookkeeping that the next redraw can always return from the current cursor position to the live-tail origin.

Ensure successful operations never accidentally leave:

* autowrap disabled
* an active SGR style
* synchronized-update mode open

Provide an explicit cleanup/finish operation and sensible best-effort `Drop` cleanup if useful.

---

# 9. Resize behavior

The caller supplies the new `Size`.

On resize:

* redraw the live tail using the new dimensions
* let the terminal emulator reflow committed history
* never reconstruct committed history
* never maintain a transcript solely to repair reflow

Be explicit about the unavoidable tradeoff:

A drastic terminal-height shrink can cause previously visible mutable rows to be moved into terminal scrollback before the application receives the resize event. Do not build a transcript reconstruction engine to repair this pathological case.

Best-effort redraw is the correct behavior for `tea-tui`.

The architecture is intentionally choosing simplicity over perfect full-screen-TUI resize semantics.

---

# 10. Suggested public shape

Keep the public API compact.

Something in this general vicinity is sufficient:

```rust
pub use geometry::{Axis, Constraint, Rect, Size};
pub use style::{Color, Style};

pub struct Line<'a> {
    // one physical live row made from safe styled spans
}

pub struct Cursor {
    // relative live-tail position
}

pub struct InlineTerminal<W: Write> {
    // ...
}

impl<W: Write> InlineTerminal<W> {
    pub fn new(writer: W) -> Self;

    pub fn commit_text(&mut self, text: &str) -> io::Result<()>;

    pub fn draw(
        &mut self,
        size: Size,
        lines: &[Line<'_>],
        cursor: Option<Cursor>,
    ) -> io::Result<()>;

    pub fn clear_live(&mut self) -> io::Result<()>;

    pub fn finish(&mut self) -> io::Result<()>;
}
```

This is illustrative, not mandatory.

Prefer fewer concepts if possible.

The crate should be understandable by reading its public API and then approximately one screenful per module.

---

# 11. Explicit non-goals

Do not implement any of these:

* alternate screen
* scrollback model
* transcript database
* transcript reflow
* virtualized list
* scrolling widgets
* generic widget trait
* component tree
* flexbox
* CSS
* general constraint solving
* retained-mode UI framework
* per-cell framebuffer
* screen diffing
* Markdown
* syntax highlighting
* input decoding
* keybindings
* mouse support
* clipboard
* focus system
* async
* terminal capability database
* terminfo
* PTY abstraction
* shell integration
* Unicode grapheme engine
* Unicode width tables
* Windows Console API
* POSIX termios
* logging framework

If implementation pressure points toward one of these, stop and find the smaller design.

---

# 12. Tests

The terminal writer must be generic over `std::io::Write` so tests can use an in-memory writer.

Test exact emitted byte sequences where valuable.

At minimum verify:

* no alternate-screen sequences are ever emitted
* committed text is emitted with autowrap enabled
* live output is emitted with autowrap disabled
* live redraw returns to the correct tail origin
* growing a tail reserves rows correctly
* shrinking a tail clears stale rows
* repeated same-size draws do not scroll the terminal
* commits happen above/replacing the live tail rather than after it
* multiple commit/draw cycles preserve cursor bookkeeping
* terminal controls embedded in untrusted text are neutralized
* style is reset
* cursor visibility is restored appropriately
* zero-width/zero-height terminal sizes do not panic
* resize bookkeeping remains internally valid
* arithmetic cannot underflow or overflow

Tests may contain a tiny purpose-built model of the few VT operations we emit if this substantially improves confidence, but do not build a general terminal emulator.

---

# 13. Example / proof

Add one tiny example or repository-local demonstration that produces an interaction resembling:

```text
user:
fix the failing parser test

assistant:
The failure comes from the EOF branch in parser.rs.

tool:
cargo test parser

running 4 tests...
test result: ok

Thinking…
> _
main · parser.rs
```

Completed portions must become ordinary terminal output while only the last few rows are repainted.

The example must make it visually obvious that the user can scroll upward using the terminal emulator's own scrollback.

Do not add dependencies solely for the example. If terminal sizing/input cannot be demonstrated cleanly without dependencies, accept dimensions as arguments or keep the demonstration focused on output behavior.

---

# 14. Size discipline

This crate exists specifically to avoid a heavyweight TUI stack.

Aim for roughly:

* **≤800 lines of non-test Rust**
* treat **1,000 lines of non-test implementation as a review threshold**

Do not game the line count, but if the implementation grows substantially beyond this, reconsider the abstractions before continuing.

Tests may be substantially larger than the implementation.

Prefer:

* direct code
* plain data types
* small functions
* explicit state transitions

over abstraction layers intended for hypothetical future features.

---

# 15. Verification

Before considering the work complete, run at least:

```sh
cargo fmt --check
cargo test -p tea-tui
cargo clippy -p tea-tui --all-targets -- -D warnings
cargo tree -p tea-tui -e all
```

Verify from the manifest and dependency tree that `tea-tui` truly has **zero dependencies of every kind**.

Also search the implementation to verify there are no alternate-screen or DECSTBM sequences.

If practical, manually exercise the example in a normal terminal and verify:

1. committed content enters native scrollback
2. scrolling back works normally
3. the live tail redraws without flooding scrollback
4. repeated spinner/status updates do not add lines
5. resizing redraws the live portion reasonably
6. long committed lines are terminal-wrapped rather than pre-wrapped
7. exiting restores normal cursor/wrap/style state

## Final deliverable

Finish with:

* the implemented `tea-tui` subcrate
* tests
* the minimal demonstration/example
* any required workspace wiring
* concise crate-level documentation explaining the transcript/live-tail model and its resize/Unicode tradeoffs

Keep the implementation aggressively small.

The architecture should remain recognizable in one sentence:

> **Write history normally; redraw only the tiny mutable tail.**
