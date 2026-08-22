# `fx` UI compatibility oracle

This is a visual and interaction reference for tea's terminal projection. Tea
keeps its own event stream, provider/model registry, session format, accounting,
compaction policy, and reduced command set. `fx` is evidence, not code to port.

## Reference and evidence boundary

The pinned reference checkout is `/Users/josh/d/fx` at commit
`83a059c643cfe911db470a7c6c1dbc8fdb61de8a`. The reference surface is fx's
default dark **minimal** presentation mode. The checked-in evidence has two
sources: an existing read-only-tools Render Lab replay in `legacy` mode at
167×46, and direct PTY captures in `minimal` mode at 80×24 and 120×40.

The source test is `fx/tests/e2e/tui-render-replay.test.ts`,
“replays read-only tools capture without layout validation failures”. Its
archive is `/Users/josh/d/fx/tests/e2e/fixtures/fx-render-bug-20260510-075848.tar.gz`.
The archive SHA-256 is recorded in
[`crates/tea-agent/fixtures/fx-ui/manifest.json`](../crates/tea-agent/fixtures/fx-ui/manifest.json).

Render Lab's own README defines its default oracle as byte replay plus
terminal-owned text/grid state. It does not prove pixel-perfect colors, font
shaping, ligatures, or cursor paint. The tea fixtures therefore record text/grid
geometry and cursor coordinates where the replay frame JSON provides them, while
foreground, background, and attributes remain explicitly unavailable. Tea does
not claim parity for style fields without source evidence.

## Captured cases

These are the committed fx cases today. The legacy replay rows are provenance
backed seeds; the minimal rows are actual captures from
[`capture-minimal.sh`](../crates/tea-agent/fixtures/fx-ui/capture-minimal.sh).

| Presentation/state | Source and input | Captured evidence | Fixture |
| --- | --- | --- | --- |
| legacy / empty composer | replay frame 8 | full visible text grid; cursor `(row=10, column=3, visible)` | `fx/empty-composer-167x46.cells.json` |
| legacy / streaming thinking | replay frame 97 | full visible text grid; cursor `(row=14, column=3, hidden)` | `fx/streaming-thinking-167x46.cells.json` |
| minimal / startup — 80×24 | capture script; no input | full visible text grid; cursor `(row=2, column=2, visible)` | `fx/minimal-startup-80x24.cells.json` |
| minimal / startup — 120×40 | capture script; no input | full visible text grid; cursor `(row=2, column=2, visible)` | `fx/minimal-startup-120x40.cells.json` |
| minimal / help — 80×24 | capture script; `/help`, Enter | full visible text grid; cursor `(row=0, column=2, visible)` | `fx/minimal-help-80x24.cells.json` |
| minimal / help — 120×40 | capture script; `/help`, Enter | full visible text grid; cursor `(row=0, column=2, visible)` | `fx/minimal-help-120x40.cells.json` |
| minimal / leading-slash menu — 80×24 | capture script; `/` | full visible text grid; cursor `(row=2, column=3, visible)` | `fx/minimal-slash-menu-80x24.cells.json` |
| minimal / leading-slash menu — 120×40 | capture script; `/` | full visible text grid; cursor `(row=2, column=3, visible)` | `fx/minimal-slash-menu-120x40.cells.json` |

The normalized format and the exact evidence status live beside the cases in
[`crates/tea-agent/fixtures/fx-ui/README.md`](../crates/tea-agent/fixtures/fx-ui/README.md).
The first reviewed tea projection, `tea/minimal-startup-80x24.cells.json`, is
loaded by the Rust renderer test and compares every `Grid` cell, its style, and
the native cursor target. It substitutes only tea identity and tea status
content under the manifest's stated allowances.
Run its deterministic checker with:

```bash
python3 crates/tea-agent/fixtures/fx-ui/check.py
```

The checker expands compact cell runs, treats unlisted cells as blanks, and
rejects overlap/out-of-bounds coordinates. `fx` cases may not silently claim
style capture; tea-owned cases can record the `Grid<Cell>` styles that the
renderer deterministically assigns.

## Verification rule

Normalized grid checks should assert visible text, row/column placement, cursor
coordinates, and style classes only when the source evidence supports them. PTY
checks complement settled-grid fixtures for first-token streaming, slash-menu
navigation, cancellation, resize, and terminal restoration. Neither path should
assert escape-sequence ordering or bytes as a proxy for the cell contract.
