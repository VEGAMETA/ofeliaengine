# Editor UI framework

Checked: 2026-09-26. Sources were read for design ideas only; no code was carried over.

CLAUDE.md §12 asks for:
- our own UI system on top of `engine_render` (the same viewports);
- Taffy to be evaluated for layout; Parley/swash/rustybuzz for text (we have
  `engine_text`, ADR 0025);
- an ADR.

The editor must:
- answer input within a frame;
- stay responsive in heavy scenes;
- render panels lazily;
- have everything undoable, keyboard-first, searchable and dockable.

## Sources

| Project | Version | License | Model |
|---|---|---|---|
| Godot `Control` / `Container` | 4.x | MIT | retained node tree, signals, containers lay out children |
| Dear ImGui | 1.9x | MIT | immediate mode, state keyed by ID hashes, docking branch |
| egui | 0.3x | MIT/Apache-2.0 | immediate mode, single-pass layout, persistent memory keyed by `Id` |
| Xilem / Masonry (Linebender) | 2025 | Apache-2.0 | declarative view tree diffed into a retained widget tree |
| RAD Debugger UI (Ryan Fleury's "UI" article series) | 2024–2025 | MIT | immediate-mode builder API, per-frame tree, retained per-key state, multi-pass autolayout |
| Clay (Nic Barker) | 0.1x | zlib | C layout library in the Fleury style: build a tree every frame, lay it out in microseconds, emit render commands |
| Taffy | 0.9 | MIT | flexbox, grid and block layout over a retained node tree with caching (used by Bevy UI, Dioxus, Zed's GPUI) |
| RmlUi | 6.x | MIT | HTML/CSS-like documents, retained DOM |

## How they work, and what matters for us

- **Retained widget trees** (Godot, Masonry, RmlUi):
  - The UI is a long-lived object graph, updated through callbacks or diffing.
  - Pro: no per-frame rebuild; natural for documents.
  - Con: two copies of the truth (model and widgets) that must be kept in sync. In
    editors this is the classic source of stale-inspector bugs and undo glitches.
  - Masonry adds a declarative diff layer to reduce that cost, at the price of
    complexity.
- **Immediate mode with retained state** (Dear ImGui, egui, RAD Debugger):
  - UI code runs every frame and reads the model directly, so there is no sync layer.
  - Per-widget state (hover animation, scroll, text cursor) persists in a map keyed by
    a hashed id.
  - Dear ImGui and egui lay out in one pass as widgets are emitted. So a widget cannot
    depend on the size of a later sibling: centering, fill-remaining and right-aligned
    groups need workarounds.
  - RAD Debugger and Clay build the whole tree first (an arena reset each frame), then
    lay it out in several passes: fixed sizes; sizes from children; sizes from
    parents; resolving overflow. Then they draw. Input uses the previous frame's
    rectangles, one frame of latency for hit testing only.
- **Layout engines:**
  - Taffy implements CSS flexbox and grid, with caching over a retained tree. It is
    powerful but general: CSS semantics (min-content, percentages against indefinite
    sizes) and its own node storage.
  - Clay's model is small: per axis, a size of `fixed`, `fit` (children), `grow`
    (share of the remainder) or `percent`; a direction; padding; gap; alignment;
    floating elements for popups and tooltips; scroll containers. It is enough for
    editor panels, docking, lists, inspectors and toolbars.

## Conclusions

1. **Immediate-mode building with retained state, full-tree layout** (the RAD Debugger
   and Clay model):
   - The inspector reads the ECS and reflection directly every frame, so there is no
     stale UI.
   - Undo and redo, script changes and multi-editing show up without invalidation code.
   - Layout is correct in both directions, unlike egui and Dear ImGui.
   - Editor frames are drawn only when something changed (input, model change,
     animation). An idle editor costs nothing.
2. **Our own Clay-like layout, not Taffy.**
   - The editor needs rows, columns, grow/fit/fixed, scroll and floating layers. CSS
     semantics add cost and surprises.
   - A layout pass over an arena-built tree is allocation-free per frame.
   - Taffy stays an option for in-game UI documents (§13), where designers may expect
     flexbox and grid.
3. **Rendering** goes through `engine_render2d`'s `Canvas`:
   - rounded rectangles, outlines and clip rectangles already exist;
   - text uses `engine_text` and the glyph atlas (PR text-draw);
   - the editor is a viewport like any other (§6).
4. **Docking** sits on top of the same layout: a split tree of panels, each panel a
   subtree. It comes after the core.

## Not taken

- Retained widget objects with signals (Godot): sync bugs and boilerplate.
- CSS-like styling documents (RmlUi) for the editor: the editor's style comes from theme
  tokens in code (§12 accessibility), not stylesheets.
- egui as the editor UI: CLAUDE.md allows it for debug overlays. Its single-pass layout
  and styling limits are the reason it is not the editor framework.
