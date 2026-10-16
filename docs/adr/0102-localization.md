# 0102. Localization: Fluent's syntax, cooked, formatted without allocating

- Status: Accepted (2026-10-16; the owner kept Fluent `.ftl` as the source format)
- Date: 2026-10-16
- Related:
  - [ADR 0025](0025-text.md): `engine_text` (shaping, line breaking, fallback fonts);
  - [ADR 0056](0056-cooked-packs-and-streaming-reads.md): packs;
  - [ADR 0099](0099-player-crate.md): the player;
  - research note `docs/research/localization.md`;
  - queue item 8d.

## Context

- **Nothing localizes.** A game's text is whatever its code writes. The owner (2026-10-14):
  localization and internationalization for the game developer, simple to use.
- **What a game needs** (the research note):
  - text by key per language, in files that merge (stable keys, translators' comments);
  - arguments, plurals and genders: Russian "1 предмет", "2 предмета", "5 предметов",
    "21 предмет" (CLDR's `one`, `few`, `many`), six forms in Arabic, none in Japanese;
  - numbers and dates per locale (`1,234,567.89`, `1 234 567,89`, `12,34,567.89`, Arabic
    digits);
  - the language switched at run time, tables hot-reloaded while developing;
  - fonts per language, right-to-left and complex scripts;
  - a check for missing and unused keys.
- **The engine's rules:** no heap allocation per frame (a HUD counter is formatted every
  frame), a feature costs nothing where unused.
- **Measured** (ref-1, release, the research note):
  - ICU4X 2.3's plural rules, decimal formatting and locales for all of CLDR 48's locales:
    193-216 KB of binary; a plural category 15.8 ns, a number into a reused `String` 35.3 ns;
    long dates for all locales add 0.8 MB;
  - `fluent-bundle` 0.16.0 allocates per format (its arguments are a vector, its result a
    `Cow` owned once anything is substituted) and formats numbers without the locale
    (Rust's `to_string`, `.` always, no grouping).
- **`engine_text`** shapes right-to-left scripts' runs right to left, but resolves no
  bidirectional levels: numbers inside Arabic text and mixed paragraphs come out wrong.

## Options

**The source format.**

1. **Fluent (`.ftl`)** (proposed; the owner's question): plurals, genders and grammatical
   cases (terms) in the format; plain messages stay plain (`play = Играть`); one text file
   per locale and module. Pontoon is built for it; Weblate and Crowdin read it but edit plural
   variants as text.
2. **gettext PO:** every translation tool reads it; plurals by a count only (a C expression
   per language in the header), no genders or cases; keyed by the source text by default.
3. **Spreadsheets (CSV):** what small teams use; no plural logic, merges badly.
4. **Our own RON tables:** like the engine's other sources; no translators' tools.

**The runtime.**

1. **`fluent-bundle`:** works today; allocates per call, numbers without the locale.
2. **Our own resolver over a cooked form** (chosen): messages compiled at cook time,
   formatted into the caller's buffer without allocating.

**Plural rules and numbers.**

1. **`intl_pluralrules`** (Fluent's, its own copy of CLDR) and numbers of our own.
2. **ICU4X** (chosen): the Unicode Consortium's, current CLDR, small (measured above).
3. **Rules generated from CLDR ourselves:** ICU4X's work again.

**Bidirectional text.**

1. **`unicode-bidi`** (chosen): Servo's UAX #9, MIT OR Apache-2.0.
2. **Our own UAX #9:** weeks of work for the same result.

## Decision

### Data

- **Sources:** `loc/<locale>/<module>.ftl` (`loc/en/menu.ftl`, `loc/ru/menu.ftl`), message
  identifiers unique across a locale's files. The source locale (`en` unless the project says
  otherwise) is complete; another locale may leave messages out, and those fall back.
- **Cooked** (`ofeliaengine loc build`, later part of the game's build) into a strings pack:
  an entry per locale (kind `Strings`, pack version 7; packs without them keep their version):
  message identifiers as 64-bit hashes with their names, patterns compiled into a flat list of
  operations (text, variable, reference, select, number), attributes, terms.
- **Pseudo-locales made by the cook:** `en-XA` (accented, a third longer, bracketed: text left
  out of localization and layouts too tight show at once) and `ar-XB` (right to left).

### The runtime: `engine_loc`

- **`Localizer`:** the tables, the locale chain (the requested locale, its parents, the source
  locale: `zh-Hans-CN` → `zh-Hans` → `zh` → `en`), ICU4X's plural rules and decimal
  formatter for each locale in it.
- **`localizer.format(loc!("items"), &args, &mut text)`** writes into the caller's `String`
  without allocating once it has grown; `loc!` hashes the identifier at compile time.
  Arguments (`Args`, up to eight, on the stack): integers and decimals with Fluent's `NUMBER`
  options (fraction digits, grouping, ordinal selection), strings, dates (feature `dates`).
- **Errors never stop the game:** a missing message writes its identifier, a missing variable
  `{$name}`, reported once in development builds.
- **`set_locale`** switches the chain; `generation()` tells text to resolve again. Windows'
  preferred UI languages (`GetUserPreferredUILanguages`) negotiate the first locale.
- **Hot reload** in development builds: `loc/` watched (`engine_platform::fs_watch`), a
  changed file compiled on a worker, the table swapped between frames; a syntax error keeps
  the previous table and reports file, line and column.

### Text and fonts

- **`engine_text`'s layout runs UAX #9** (`unicode-bidi`): embedding levels per paragraph,
  runs split at level, script and font changes, shaped in their direction, reordered per line.
- **Fonts per language:** the game gives a font stack per language (CJK, Arabic, Devanagari
  fonts are large and its choice; the engine ships none); the UI takes the current locale's.
  Mirroring a UI for right-to-left languages belongs to the UI (item 9).

### The check

`ofeliaengine loc check <loc> [--code <dirs>]`: syntax errors with positions; messages missing
or extra per locale against the source; variables a translation uses that the source does not
pass; unknown references; identifiers used in Rust (`loc!`) or Luau (`loc(...)`) but not
defined; defined but unused (a warning). A non-zero exit code on errors, for builds.

## Steps

1. **8d-1:** the research note and this ADR.
2. **8d-2:** `engine_loc`: the compiler (FTL to operations, behind a `compile` feature so a
   shipped game does not carry the parser), the runtime (format, plurals, numbers), the
   `Strings` entry; tests (plurals in English, Russian, Polish, Arabic; fallback; missing
   messages and variables; no allocation per format) and a benchmark.
3. **8d-3:** the player (`PlayerSettings::strings`, the locale from Windows, switching, hot
   reload); `ofeliaengine loc build`, `loc check`, the pseudo-locales.
4. **8d-4:** bidirectional layout and font stacks per language in `engine_text`, with tests of
   mixed Arabic, Hebrew and Latin text and a golden image.
5. **8d-5:** the dev guide (`docs/dev-guide/localization.md`), measurements, the gate.

## As built

### 8d-2 (2026-10-16, `engine_loc`)

- **The compiler** (`compile`, `compile_pseudo`): a locale's files into a table of
  operations; duplicates and cycles are errors (the entries left out), references to
  nothing warnings, with file, line and column.
- **The runtime** (`Localizer`): the chain from ICU4X's `LocaleFallbacker` (`es-MX` →
  `es-419` → `es`; `zh-TW` → `zh-Hant`, never `zh`); a table's numbers and plural categories
  by the locale asked for when it is more specific than the table's (Egypt's Arabic digits
  from an `ar` table); `Args` on the stack (eight at most).
- **Measured** (ref-1, release, `benches/format.rs`, into a string that has grown): plain
  14.4 ns, a plural with a grouped number 142.2 ns, a fraction 62.1 ns, a term in the
  genitive 70.5 ns, an English fallback 55.0 ns; 10,000 messages compiled in 8.1 ms, 1115 KB
  cooked, read in 0.42 ms. No allocation over 10,000 HUD frames (`tests/zero_alloc.rs`).

### 8d-3 (2026-10-16)

- **No `compile` feature** (the decision said one). Only the CLI would enable it, so
  `engine_loc`'s features would differ between the workspace's build and a package's own:
  every crate above it rebuilt in turn (ADR 0077), and the workspace hack cannot unify a
  crate that depends on it. Measured instead: a release binary with fat LTO, stripped, is
  224 KB with the runtime only and 346.5 KB with the compiler called, so a shipped game
  carries about 120 KB of a parser it does not run. The player uses it for `loc/` folders.
- **Translations compile against the source** (`compile_translation`):
  - references the source resolves are not reported;
  - messages the source lacks are warnings;
  - a message using variables its source message does not pass is an error, and left out:
    the source's shows, not `{$cnt}`. The variables are followed through references as the
    runtime resolves them (the translation's, else the source's). Every message wrong
    through any reference is left out first; then those right once the others are left out
    come back. Bringing one back never makes another wrong: its variables are a subset of
    its source message's.
- **A table keeps names for the check:** an attribute's entry its name (`menu.tooltip`), a
  select its variable's; 10,000 messages cook to 1147 KB (1115 before).
- **The pack:** entry kind `Strings` (code 11), pack version 7. `meta[0]` is 1 for the
  source locale, `meta[1]` its message count; the source locale's entry comes first.
  `engine_assets::strings::{write_strings_pack, load_strings, load_localizer}`; a scene's
  loading skips such entries.
- **The CLI:** `ofeliaengine loc build <loc> -o <pack> [--source] [--pseudo] [--level]`,
  where an error stops the build (exit code 1). `ofeliaengine loc check <loc> [--source]
  [--code <dir>]...` reports:
  - each locale's missing messages and attributes;
  - the translations' variables;
  - identifiers used in `loc!("…")` (Rust) or `loc("…")` (Luau) but not defined (errors);
  - messages neither the code nor another message uses.
  `engine_loc::{read_sources, compile_sources, find_uses, check}` hold it, for the editor
  later.
- **The player:**
  - `PlayerSettings::text`: `TextSource::Pack`, or `TextSource::Folder`, kept live:
    - a recursive directory watcher;
    - the folder compiled again on a thread of its own once its files have been quiet for
      30 ms;
    - the tables swapped between frames;
    - a locale with errors keeps its previous table (`Player::text_diagnostics`).
  - `PlayerSettings::languages`: `None` takes Windows' display languages
    (`engine_platform::locale::preferred_languages`, `GetUserPreferredUILanguages`).
  - The localizer is the world's resource, reached from every hook. `Localizer`
    remembers the languages asked for and negotiates them again when its tables change.
  - In `tests/text.rs`, an edit showed in the running player 3 frames later.
- **ICU4X's `sync`** (icu_provider's feature: its data behind `Arc`, not `Rc`) makes the
  localizer `Send + Sync`, which a world's resource must be. It costs about 3 ns on a plural
  with a grouped number: 145.7-148.5 ns against 142.2-143.2, binaries of both commits run in
  turn; 143.8-145.1 ns without `sync`. Nothing measurable elsewhere.
- **A string selector matches variants named like categories** (`"one"` selects `[one]`).
- **Not done:** a pack's hot reload (folders only), dates, bidirectional layout (8d-4).

### 8d-4 (2026-10-16, `engine_text`)

- **UAX #9 adapted, not used as a crate** (the decision said `unicode-bidi`). Measured
  (ref-1, release), unicode-bidi 0.3.18 allocates on every call: 7 times and 450 ns for a
  line of Latin text, 14 to 23 times and 0.9 to 2.2 µs for Arabic or Hebrew lines. A HUD
  laid out every frame would break the rule of no allocation per frame. Its algorithm
  (MIT OR Apache-2.0) is adapted in `engine_text/src/bidi.rs`:
  - levels per character (unicode-bidi's are per byte);
  - an isolating run sequence's runs linked through two arrays;
  - every buffer kept between calls, the bracket pairs sorted without allocating;
  - nothing done for text with nothing right to left, which below U+0590 no character is;
  - character classes looked up a page of 256 at a time, the page filled on first use
    (5 µs; 64 KB at most). That halved the cost: 1,184 → 659 ns for a 48-character Arabic
    line, 7.9 ns a byte.

  The crate stays for its character data (Unicode 16). All 91,707 cases of Unicode 16's
  `BidiCharacterTest.txt` pass (`ENGINE_BIDI_TESTS` names its folder); a sample of 1,846 is
  checked in and run by every test.
- **The layout:**
  - runs split by level as well as font and script, shaped in their level's direction;
  - each line's levels reset at its end (L1), its glyphs reordered by level (L2);
  - HarfRust mirrors brackets in right-to-left runs (L4).
  - `TextStyle::direction`: `Auto` (the first strong character), `LeftToRight`,
    `RightToLeft`.
  - `Line::rtl`. `Align::Start` and `End` are the paragraph's: right and left for
    right-to-left text. A right-to-left line's trailing whitespace hangs past its left.
  - `glyphs()` is in visual order per line.
- **Costs** (ref-1, `benches/text.rs`):
  - left-to-right text a little faster than before: labels 0.83-0.84 µs against
    0.86-0.87, a paragraph 16.4 ns a byte against 16.9-17.1 (binaries of both commits run
    in turn);
  - Arabic and Hebrew labels with numbers 3.41 µs;
  - an Arabic paragraph 117 ns a byte, most of it shaping.

  No allocation over 540 frames of Arabic and Hebrew text among the others
  (`tests/zero_alloc.rs`).
- **Fonts per language:** `FontStacks`, a stack per language tag, a locale taking its
  longest tag set (`zh-Hant-TW` → `zh-Hant` → `zh`). Noto Sans Arabic 2.013 and Noto Sans
  Hebrew 3.001 (OFL-1.1) are bundled for tests (`engine_text::fonts`); a binary that does not
  name them does not link them.
- **The golden image** (`engine_text/tests/data/bidi.png`) is rendered on the CPU: an Arabic
  paragraph with numbers and Latin, Hebrew in Latin, Hebrew with Latin, Latin forced right
  to left. Layout and rasterization are deterministic, and the 2D renderer's own test checks
  that the GPU draws the atlas's coverage, so no GPU shot per vendor is needed.
- **Not done:** a UI mirrored for right-to-left languages (item 9, the UI's), vertical text.

## Consequences

- **Translators work in FTL** (Pontoon, Weblate, Crowdin); PO and XLIFF export can come with
  the tools for teams that want them.
- **New dependencies,** all on the allowed list: `fluent-syntax` (in every build, about
  120 KB; 8d-3 as built),
  `icu_plurals`, `icu_decimal`, `icu_locale`, `icu_datetime` behind `dates` (Unicode-3.0),
  `unicode-bidi` (MIT OR Apache-2.0). About 0.2 MB of binary, 1 MB with dates.
- **We implement a subset of Fluent** (messages, terms, attributes, variables, selects, `NUMBER`,
  `DATETIME`; no custom functions at first); the dev guide says which.
- **Luau** (item 9) calls the same lookup.
