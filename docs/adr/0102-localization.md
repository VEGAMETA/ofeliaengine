# 0102. Localization: Fluent's syntax, cooked, formatted without allocating

- Status: Proposed (2026-10-16; the source format is the owner's question)
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

## Consequences

- **Translators work in FTL** (Pontoon, Weblate, Crowdin); PO and XLIFF export can come with
  the tools for teams that want them.
- **New dependencies,** all on the allowed list: `fluent-syntax` (cook and development only),
  `icu_plurals`, `icu_decimal`, `icu_locale`, `icu_datetime` behind `dates` (Unicode-3.0),
  `unicode-bidi` (MIT OR Apache-2.0). About 0.2 MB of binary, 1 MB with dates.
- **We implement a subset of Fluent** (messages, terms, attributes, variables, selects, `NUMBER`,
  `DATETIME`; no custom functions at first); the dev guide says which.
- **Luau** (item 9) calls the same lookup.
