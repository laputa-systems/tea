# Terminal renderer migration record

The scrollback-native terminal renderer is implemented. The durable product
contract lives in [docs/tui.md](docs/tui.md); this file is intentionally a
short completion record rather than a second architectural specification.

`tea-tui` is a zero-dependency ANSI/VT primitive crate. Tea keeps semantic
conversation state in `AppState`, commits settled styled rows once to native
main-screen scrollback, and redraws only a bounded mutable tail. Temporary
full-screen surfaces borrow the alternate screen and return it on close.

Historical rows retain the width at which they were committed. Terminal
emulator scrollback, not tea, owns history navigation and historical reflow.
