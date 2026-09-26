# Text: shaping, layout, rasterization

Research for `engine_text` (CLAUDE.md §13 "Text", §12 editor UI). Text is needed by the
editor, the in-game UI, debug overlays (latency, profiler) and world-space labels.
Decision: [ADR 0025](../adr/0025-text.md).

Checked on 2026-09-26 (crates.io versions and dependency trees).

## Candidates

| Stack | Version | License | What it is |
|---|---|---|---|
| Parley (+ fontique, HarfRust, skrifa, ICU4X) | 0.11.1 | Apache-2.0 / MIT | Linebender's rich text layout. Fontique handles font enumeration and fallback (DirectWrite on Windows); ICU4X handles segmentation and properties. |
| cosmic-text (+ fontdb, HarfRust 0.5, skrifa 0.40, swash) | 0.19.0 | MIT / Apache-2.0 | System76's all-in-one text: shaping, layout, editing, swash rasterization. Used by Bevy UI and iced. |
| HarfRust | 0.13.3 | MIT | The HarfBuzz project's Rust port of HarfBuzz shaping (successor of rustybuzz). |
| skrifa / read-fonts | 0.46–0.47 | MIT / Apache-2.0 | Google's font reading, scaling and hinting (fontations, used in Chrome). |
| swash | 0.2.10 | Apache-2.0 / MIT | Scaler and rasterizer (zeno) on skrifa ≤ 0.44. |
| unicode-linebreak | 0.1.5 | MIT / Apache-2.0 | UAX #14 break opportunities, table-driven, no dependencies. |
| unicode-bidi | 0.3.18 | MIT / Apache-2.0 | UAX #9 bidirectional algorithm. |

Test font: Go Regular (Bigelow & Holmes for the Go project, BSD-3-Clause), vendored in
`third_party/fonts/go`. It covers Latin, Cyrillic and Greek.

## Measurements

`bench/text-compare`, release build. All stacks get the same font from bytes, with no
system fonts, at 14 px. Numbers are µs, medians; the dev container (Xeon @ 2.8 GHz,
shared) is not reference hardware, and ref-1 numbers come from the Bench workflow.

| Scenario | Parley | cosmic-text | HarfRust shaping only | Fast-path sketch |
|---|---:|---:|---:|---:|
| 1,000 editor labels (1–4 words, English + Russian), shape + lay out | 5,891 | 5,344 | 1,204 | **997** |
| Per label | 5.89 | 5.34 | 1.20 | **1.00** |
| 10 KB paragraph, shape + wrap at 600 px | 1,300 | 2,616 | 133 (no wrap) | **193** |
| Re-wrap the paragraph at 3 widths | 139 | 438 | — | — |

- **Fast-path sketch:**
  - script runs (Latin / Cyrillic, with shared punctuation joining the current run);
  - HarfRust with shape plans cached per script;
  - UAX #14 opportunities from `unicode-linebreak`;
  - greedy wrapping.
- **Glyph counts are identical** in all four stacks (22,570 for the labels).
- **Line counts differ:** Parley 84, the sketch 88. Parley lets trailing spaces hang past
  the edge; the sketch counts them. A real implementation hangs them too.

The implementation (`engine_text`, measured in the same harness): 1.40 µs per label and
265 µs for the paragraph. Its glyphs and lines (84) match Parley's.

Dependency trees (normal dependencies, Windows target):
- Parley: 80 crates (66 without system fonts), including ~15 ICU4X crates with data;
- cosmic-text: 44;
- HarfRust + skrifa + swash: 22.

Clean release builds of Parley and the small stack both took about 25 s in the container.

## How each works, and what it costs

- **Parley:**
  - builds a full rich-text model per layout: style runs, ICU segmentation (words,
    graphemes, line breaks), bidi, font selection with fallback, shaping per run, then
    line breaking and alignment;
  - re-wrapping reuses the shaping (139 µs for three widths).
  - Everything is general, and every label pays for it: 5.9 µs where shaping is 1.2 µs.
- **cosmic-text:**
  - similar pipeline, organized around an editable `Buffer` of lines;
  - its own caches; slower on paragraphs here (2.6 ms);
  - pins older HarfRust and skrifa, so using it next to a direct HarfRust would compile
    two copies.
- **HarfRust directly:**
  - shaping is only 20 % of Parley's time for labels, and 10 % for paragraphs;
  - most of the rest is analysis and layout machinery, which UI text rarely needs:
    single style, single direction, one or two fonts.

## Rendering glyphs

- **Bitmap atlas at the displayed size** (what Chrome, VS Code and Dear ImGui do):
  - coverage rendered per (font, glyph, size, subpixel x offset), packed into an atlas and
    drawn as quads;
  - the crispest at small UI sizes (12–16 px);
  - costs atlas space per size.
- **SDF / MSDF** (Green, "Improved Alpha-Tested Magnification", SIGGRAPH 2007; Chlumský,
  "Shape Decomposition for Multi-channel Distance Fields", 2015; msdfgen, MIT):
  - one atlas entry per glyph at any scale, sharp corners with MSDF;
  - right for world-space and scaled text;
  - softer than hinted bitmaps at small UI sizes.
- **GPU vector rendering:**
  - Slug (Lengyel) is patented and commercial, so it is not an option;
  - Vello (Linebender, compute-based) renders whole scenes; heavy for glyphs alone.
- **Rasterizer:** swash's `zeno` requires skrifa ≤ 0.44, while HarfRust 0.13 needs
  read-fonts 0.43 (skrifa 0.46). A coverage rasterizer over skrifa's outline pen (signed
  area accumulation, as in font-rs and ab_glyph) is about 200 lines. It keeps one
  read-fonts copy and lets us control subpixel positioning.

## What we take

- **HarfRust + skrifa** as the base (one read-fonts version):
  - shaping plans cached per (font, script, direction);
  - `unicode-linebreak` for UAX #14 and `unicode-bidi` only when a paragraph has RTL
    characters.
- **Our own layout for the common case:**
  - one style, runs by script and by font (fallback chain), greedy wrapping with hanging
    whitespace, alignment;
  - 5–7× faster than Parley on the measured cases, with the same glyphs.
- **Bitmap atlas first** (UI and the editor), rendered into the UI's own target
  (premultiplied alpha, [frame generation](frame-generation.md)).
- **MSDF next** for world-space text.
- **Parley stays the reference** in `bench/text-compare`: glyph and line counts are
  checked against it.

## What we do not take, and why

- **Parley as the engine's layout:**
  - 5–7× slower where the editor spends its text time;
  - 80 crates on Windows.
  - Revisit if rich text editing (mixed styles and scripts in one paragraph) becomes a
    requirement that our layout cannot meet cheaply.
- **cosmic-text:** slower than Parley on paragraphs, and it pins old HarfRust and skrifa.
- **swash:** its skrifa pin conflicts with current HarfRust.

## Open

- **System font fallback on Windows** (DirectWrite `IDWriteFontFallback`) for CJK and
  emoji in the editor: next step after the core.
- **Complex-script line breaking** (Thai, Lao, Khmer need dictionaries; ICU4X segmenter)
  when a game ships those languages.
- **Hinting:** skrifa's autohinter vs unhinted with subpixel positioning at UI sizes;
  judge visually in the editor.
