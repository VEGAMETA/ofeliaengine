# 0030. UI core: immediate-mode building, retained state, full-tree layout

- Status: Accepted
- Date: 2026-09-26
- Research: [docs/research/ui-framework.md](../research/ui-framework.md)
- Amends: CLAUDE.md §12 ("our own retained-mode system"): the system is our own and
  retains state, but frames are built in immediate mode. The reasons follow.

## Context

The editor needs a UI framework (CLAUDE.md §12, §17 step 4). The requirements:
- input answered within a frame;
- stays fast in heavy scenes;
- lazy panels;
- everything undoable, keyboard-first, searchable and dockable;
- the same viewport system as the game;
- evaluate Taffy for layout.

## Options

1. **Retained widget tree** (Godot, Masonry, RmlUi). Widgets are long-lived objects,
   updated by signals or diffing. The model and the widgets are two copies of the truth;
   every model change (undo, scripts, multi-editing) needs invalidation code, and a
   missed one shows stale values.
2. **Immediate mode with single-pass layout** (Dear ImGui, egui). No sync layer, but
   sizes only flow forward: centering, fill-remaining and right-aligned groups need
   workarounds or a frame of jitter.
3. **Immediate-mode building, retained per-id state, full-tree multi-pass layout**
   (RAD Debugger, Clay; chosen).

For layout:
- **Taffy:** flexbox and grid; general, CSS semantics, its own retained node store.
- **Own Clay-like sizing** (chosen): per axis `Fixed`, `Fit`, `Grow(weight)`,
  `Percent`; row or column; padding; gap; alignment; clip; scroll; floating layers.
  It covers editor panels, lists, inspectors and toolbars.

## Measurements

`cargo bench -p engine_ui --bench ui`, container (4 threads, not reference), bench
profile. Inspector-like frame: 2,000 rows of label, spacer, button and label in a
scroll list, plus a paragraph; 10,006 nodes.

| stage | µs | per node |
|---|---:|---:|
| whole frame (build, layout, hit test, draw list) | 962 | 96 ns |
| build only (ids, nodes, text) | 335 | 34 ns |

- Culling subtrees outside the clip rectangle cut the draw list from 8,004 commands to
  what is visible, and the frame from 1,023 to about 950 µs.
- The rest is layout over a 10k-node array (six passes).
- Editor lists will build only visible rows, so typical panels stay at a few thousand
  nodes, 0.1–0.3 ms.
- `tests/zero_alloc.rs`: 0 allocations over 200 steady-state frames with scrolling,
  clicks and a floating layer.

## Decision

- **`engine_ui`** (no GPU dependency):
  - `Ui::begin_frame(input, viewport)`, build, `Ui::end_frame(measure)` → `Frame` with
    `DrawCmd`s (rounded rectangles, text, clip changes) and the frame's text.
  - Nodes are stored in pre-order in a reused array: bottom-up passes run backwards,
    top-down passes forwards, with no recursion.
  - Ids hash a key under the parent id, like Dear ImGui's id stack. State that must
    persist (scroll offsets) lives in a map keyed by id and is dropped after 120 frames
    unused.
  - Hit testing takes this frame's mouse against the previous frame's layout. Hover and
    clicks follow the pointer without delay; only the rectangles are a frame old.
  - Floating layers draw last and win hit tests.
  - Keyboard is first-class: Tab and Shift+Tab move focus through focusable widgets,
    Enter and Space activate, Escape drops focus; a focus ring is drawn.
  - Text is measured through the `TextMeasure` trait. The editor implements it on
    `engine_text` (ADR 0025); tests use `MonospaceMeasure`.
  - Widgets so far: label, paragraph (wrapped), button, spacer, panel, scroll column.
    Theme tokens are in `Theme`.
- **Rendering** maps `DrawCmd`s to `engine_render2d` quads (rounded rectangles,
  outlines, clip rectangles) and glyphs (text drawing, ADR 0025 follow-up). That adapter
  comes with the text-draw PR.
- Frames are built only when something changed (input, model change, animation); an
  idle editor builds nothing.
- Taffy is not used for the editor. It remains an option for data-driven in-game UI
  documents.

## Consequences

- Editor panels read the ECS and reflection directly every frame: no stale inspector,
  no invalidation code.
- Layout has no overflow shrinking yet (content larger than its parent is clipped or
  scrolled). Clay-style shrink passes can be added if panels need them.
- The node array is fat (the full style per node). If profiles show layout
  bandwidth-bound, split it into hot and cold parts.
- Next:
  - the render adapter with text;
  - docking (a split tree of panels over this layout);
  - text input;
  - the command palette;
  - the undo/command system.
