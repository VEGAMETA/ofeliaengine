# 0036. Default fonts and icons; OFL-1.1 and CC BY 4.0 for font and icon assets

- Status: Accepted (license policy: team decision, 2026-09-26)
- Date: 2026-09-26
- Related: [ADR 0025](0025-text.md) (text layout and fallback stacks)

## Context

The editor shell needs a UI font that covers the languages our users write in (at least
Latin, Cyrillic, Greek and Vietnamese), a monospaced font for code and the console, and an
icon set for play / pause / stop, folders, node and file types. Until now the only bundled
fonts were the Go fonts (BSD-3-Clause, ADR 0025): Latin, Greek and Cyrillic only, no icons.

Almost every good open font is under the SIL Open Font License 1.1, and the large icon sets
are split between MIT, Apache-2.0 and CC BY 4.0. Neither OFL-1.1 nor CC BY 4.0 is on the
allowed list of CLAUDE.md §15, so this is a licensing question for the team (§3).

## Options

1. **Stay on the Go fonts and draw icons as shapes.** No license change. No Vietnamese or
   extended Latin, every icon is custom code, and code text uses a font that was not made
   for the UI.
2. **A patched "Nerd Font" (a text font with icons merged in).** One file, but it is a
   modified font: OFL forbids the reserved font name on modified versions, patched builds
   lag behind upstream, and we cannot swap the text font without re-patching.
3. **An unmodified text font + an unmodified "Symbols Nerd Font" as a fallback.**
   `engine_text` already shapes with fallback stacks (ADR 0025), so the result on screen is
   the same as option 2, with every file unmodified and replaceable on its own.

## Decision

Option 3, with this stack for the editor UI:

| Font | Version | License | Role |
|---|---|---|---|
| Noto Sans Regular | 2.015 | OFL-1.1 | UI text: Latin (incl. Vietnamese), Greek, Cyrillic |
| JetBrains Mono Regular | 2.305 | OFL-1.1 | code and console; in the UI stack, arrows, math and key symbols (→ ≤ ✓ ⌘ ⇧ ⏎) that Noto Sans lacks |
| Symbols Nerd Font | Nerd Fonts 3.5.1 | MIT (font), icon sets MIT / Apache-2.0 / CC BY 4.0 / OFL-1.1 / Unlicense | icons (Material Design, Codicons, Font Awesome, …) |

UI stack: `[NOTO_SANS, JETBRAINS_MONO, NERD_SYMBOLS]`; code stack: `[JETBRAINS_MONO, NOTO_SANS, NERD_SYMBOLS]`.
The Go fonts stay as the test font of `engine_text`.

**License policy (team decision):** OFL-1.1 and CC BY 4.0 are allowed **for font and icon
assets only**, never for code, on the condition that `THIRD_PARTY_NOTICES.md` has a
"Fonts and icons" section that credits every font and icon set we ship, with author,
upstream, version and license. Fonts are shipped unmodified; a modified OFL font must drop
its reserved font name. CLAUDE.md §15 is amended accordingly.

Fallback fix that came with it: `TextLayout` itemization kept Common-script characters
(arrows, math, punctuation) in the current run's font even when that font lacked them, so
they drew as `.notdef`. Now a neutral character stays in the current font only if the font
has it, otherwise it takes the first font in the stack that does; combining marks always
stay with their base.

## Numbers

- Size in the binary (`include_bytes!`): Noto Sans 607 KB, JetBrains Mono 264 KB, Symbols
  Nerd Font 2,479 KB; 3.3 MB in total. The editor pays it; a game embeds only the fonts it
  uses (unreferenced `engine_text::fonts` statics are normally dropped by the linker's
  `/OPT:REF`; to be verified when the game build exists).
- Coverage (cmap): Noto Sans 2,965 code points, JetBrains Mono 1,372, Symbols Nerd Font 10,637.
- Shaping cost: a fallback lookup is one charmap query per font per character, only when
  the current font misses; ASCII and Cyrillic UI text never leaves Noto Sans.

## Consequences

- Other scripts (CJK, Arabic, Hebrew, Devanagari, …) come from further Noto families added
  to the stack when needed, under the same policy; CJK alone is 10+ MB, so it will be an
  optional download rather than embedded.
- Every new font or icon set means a row in the "Fonts and icons" section of
  `THIRD_PARTY_NOTICES.md` in the same commit.
- Games that ship these fonts must carry the same notices; `docs/dev-guide` will say so
  when the game build produces its own notices file.
