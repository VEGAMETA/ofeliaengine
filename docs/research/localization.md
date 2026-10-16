# Localization and internationalization

Checked 2026-10-16. Sources at the end.

- **Known from their documentation:** Godot 4's internationalization (`tr`, `tr_n`,
  `TranslationServer`, pseudolocalization, right-to-left layout), Unreal Engine 5's
  localization pipeline (localization targets, gathering, PO export and import, `.locres`,
  cultures), Unity's Localization package 1.5 (string and asset tables, Smart Strings,
  pseudo-localization, XLIFF / CSV / Google Sheets), Project Fluent's syntax guide, GNU
  gettext's PO format.
- **Code read:** fluent-rs (`fluent-bundle` 0.16.0, Apache-2.0 OR MIT): its number type and
  its dependencies.
- **Built and run:** ICU4X 2.3 (Unicode-3.0) plural rules, decimal and date formatting in a
  probe program on ref-1.
- **No code was copied.**

## Why

Queue item 8d (the owner, 2026-10-14: "simple to use"). The engine has no localization:
a game's text is whatever its code writes. What a game needs:

- **Text by key, per language,** in source files that merge well (stable keys, one file per
  language and module, translator comments).
- **Arguments, plurals and genders:** "{n} items" is "1 item", "2 items" in English, and
  "1 предмет", "2 предмета", "5 предметов", "21 предмет" in Russian (CLDR's `one`, `few`,
  `many` categories); some languages have six forms, some none.
- **Numbers and dates per locale:** "1,234,567.89", "1 234 567,89", "1.234.567,89",
  "12,34,567.89" (Hindi groups by two), Arabic digits in Egypt.
- **The language switched at run time,** the tables hot-reloaded while developing.
- **Fonts per language** (CJK, Arabic, Devanagari) with fallbacks, **right-to-left and
  complex scripts** through `engine_text`'s shaping and a bidirectional algorithm.
- **A check for missing and unused keys** ("one button": the CLI, later the editor).
- **The engine's rules:** no heap allocation per frame (a HUD counter is formatted every
  frame), nothing on the render thread that blocks, a feature costs nothing where unused.

## How other engines do it

### Godot 4 (documentation)

- **Lookup:** `tr(key)`, with an optional context (`tr("Close", "Distance")`), and
  `tr_n(singular, plural, n)` for plurals; placeholders through format strings
  (`{character}`), so translators can reorder them.
- **Locales:** language, optional script, region and variant (`en_GB`); the OS language
  first, then the project's fallback, then `en`. `TranslationServer.set_locale()` switches at
  run time; translations can be added and removed while running.
- **UI:** controls whose text matches a key translate themselves (auto-translate can be
  turned off per node). Right-to-left languages mirror anchors, margins, alignment and
  container order (`layout_direction`, `text_direction`, `structured_text_bidi_override`).
- **Pseudolocalization** (project settings): accents and lengthens strings, keeps
  placeholders; finds strings left out, layouts too tight, missing glyphs; not a test of CJK
  or right-to-left text.
- **Fonts:** fallback fonts, not per-language remaps. Languages without spaces need the text
  server's data (about 4 MB) for line breaking.
- **Assets:** remaps per locale (images, voice lines). Files: spreadsheets (CSV) and PO.

### Unreal Engine 5 (documentation)

- **`FText`** authored at the source (`LOCTEXT`, `NSLOCTEXT`, text properties) or once in
  **string tables** and referenced.
- **Gathering** collects text from code and assets into a manifest; per-culture archives
  hold translations; **PO files** go to translators and come back; everything is compiled into
  `.locres`, the only files loaded at run time. Text whose source changed is "stale".
- **Cultures** are IETF tags with fallback from most to least specific (`zh-Hans-CN` →
  `zh-CN` → `zh-Hans` → `zh`); `es-419` for Latin American Spanish with culture mappings.
- **ICU** formats numbers and selects plurals; `FText::Format`'s argument modifiers select
  plural, gender and ordinal forms and Korean post-positions (`|plural(one=…,other=…)`,
  `|gender(…)`, `|ordinal(…)`, `|hpp(…)`, as documented on its text formatting page, whose
  text did not load for this note).
- **Packaging** picks the cultures to ship and the ICU data sets.

### Unity Localization 1.5 (documentation)

- **String tables and asset tables** (textures, models, audio per locale) in collections
  sharing keys.
- **Smart Strings** (a modified SmartFormat): placeholders with selectors and formatters,
  plurals (`"I have {0:plural:1 Apple| Apples}"`), choices, nested scopes; arguments through
  `LocalizedString.Arguments`.
- **Pseudo-localization** before translations exist; import and export as XLIFF, CSV and
  Google Sheets.

## Formats

### Fluent (FTL)

Mozilla's localization system (Firefox, Thunderbird). A message is an identifier and a
pattern; patterns hold placeables (variables `{ $count }`, other messages, terms
`{ -brand-name }`, literals, functions) and select expressions:

```ftl
# Shown in the inventory's title.
items = { $count ->
    [one] { $count } item
   *[other] { $count } items
}
```

- **Selectors** match string keys directly and numbers by exact value (`[0]`) or by CLDR
  plural category (`zero`, `one`, `two`, `few`, `many`, `other`); `*` marks the required
  default variant. `NUMBER($score, minimumFractionDigits: 1)` formats before the category is
  chosen; `type: "ordinal"` selects ordinal categories ("1st", "2nd").
- **Terms** (`-brand-name`) with attributes carry grammatical information a translator uses in
  one language without the source knowing (asymmetric localization: Russian adds cases where
  English has none, without changing the code).
- **Attributes** (`.tooltip`, `.label`) give a message several texts.
- **Comments** for translators; one file per module and locale; plain text that merges.
- **Tools:** Pontoon is Mozilla's own tool for it. Weblate reads FTL since 4.8 (marked as
  still in development) and Crowdin since 2021, but neither edits plural variants as such:
  a translator edits the select expression as text.

### gettext PO

`msgid` (the source text, or a key) and `msgstr`; `msgctxt` for contexts; plurals by
`msgid_plural` and `msgstr[n]`, chosen by a C expression in the file's `Plural-Forms` header
(English: `nplurals=2; plural=(n != 1);`). The most widely supported format in translation
tools (Poedit, Weblate, Crowdin, Transifex); no genders or selectors beyond the count. The
gettext runtime is LGPL; the format is not a problem, the library would be.

### Spreadsheets (CSV)

What most small teams use (Godot's documentation says so): a key column and a column per
language. No plural or gender logic beyond separate keys; merges badly.

## Libraries

### fluent-rs (Apache-2.0 OR MIT)

- `fluent-syntax` 0.12.0: the parser, its AST, a serializer.
- `fluent-bundle` 0.16.0: messages of one locale, formatted with `FluentArgs`; plural
  operands from `intl_pluralrules` 7.0.2 (its own CLDR data), locales from `unic-langid`,
  formatters memoized by `intl-memoizer`.
- **Numbers are not formatted per locale:** `FluentNumber::as_string` is Rust's `to_string`
  padded to the minimum fraction digits, always with `.` and never with grouping (read in its
  source; `useGrouping` is parsed and ignored).
- **Each format allocates:** the arguments are a vector of `(Cow<str>, FluentValue)`, the
  result a `Cow<str>` (owned as soon as anything is substituted), errors a vector.
- Also: `fluent-fallback` and `fluent-resmgr` (resource loading and fallback chains),
  `fluent-pseudo` (pseudolocalization).

### ICU4X 2.3 (Unicode-3.0)

The Unicode Consortium's Rust implementation of ICU, with CLDR 48 data compiled into the
binary by default (`compiled_data`), or generated for chosen locales by `icu4x-datagen`.
Measured on ref-1 (release, fat LTO, stripped probe program):

| | binary | per call |
|---|---:|---:|
| plural rules + decimal formatting + locales, all locales | 193-216 KB | plural category 15.8 ns; a number into a reused `String` 35.3 ns |
| + date formatting (year, month, day, long), all locales | 1,025 KB | |

- Plurals right where it matters: 21 is `one` in Russian, `many` in Polish and Arabic.
- Grouping and digits per locale: en `1,234,567.89`, ru `1 234 567,89`, de `1.234.567,89`,
  hi `12,34,567.89`, ar-EG `١٬٢٣٤٬٥٦٧٫٨٩`.
- Dates per locale: en "October 16, 2026", ru "16 октября 2026 г.", ja "2026年10月16日".

### unicode-bidi 0.3.18 (MIT OR Apache-2.0)

Servo's implementation of the Unicode bidirectional algorithm (UAX #9): embedding levels per
paragraph, runs split by level, lines reordered visually. `engine_text` today shapes each
right-to-left script's run right to left, but resolves no levels: numbers inside Arabic
text, neutral punctuation and mixed paragraphs come out in the wrong order.

## What we take

- **Fluent's syntax as the source format:** expressive enough for plurals, genders and
  cases (terms), simple for plain text (`play = Играть`), one file per locale and module,
  translators' tools read it. Parsed at cook time by `fluent-syntax`.
- **Our own runtime over a cooked form:** messages compiled at cook time into compact
  programs in the pack, formatted into the caller's buffer without allocating (a HUD
  formats every frame), with locale-aware numbers. `fluent-bundle` is not used at run time:
  it allocates per call and formats numbers without the locale.
- **ICU4X** for plural rules, numbers and dates (dates behind a feature: 0.8 MB of data for
  all locales; nothing where a game does not use them).
- **Locale fallback** from most to least specific, then the source locale (Unreal's and
  Godot's order); the player's preferred languages from Windows.
- **`unicode-bidi`** in `engine_text`'s layout for right-to-left and mixed text; fonts chosen
  per language through fallback chains (Godot's fallback, not remaps).
- **Pseudolocalization** made by the cook (accents, length, brackets), Godot's and Unity's
  testing aid.
- **A check** (`ofeliaengine loc check`): messages missing or extra per locale against the
  source locale, variables a translation uses that the source does not pass, keys used in
  code and scripts but missing, keys defined but unused.
- **Later, with tools:** PO and XLIFF export and import for translators (Unreal's route),
  asset tables (localized textures and voice).

## What we don't take

- **`fluent-bundle` at run time:** allocations per format and numbers without locale.
- **The gettext runtime** (LGPL) and PO as the source: a plural count only, no genders.
- **Spreadsheets as the source:** no plural logic, poor merges (export to them later, for
  teams that want them).
- **ICU4C:** C++ and larger; ICU4X is the same data and rules in Rust, 50-90 % smaller by its
  authors' figures.
- **Text matched as keys** (Godot's auto-translate of UI text equal to a key): explicit keys
  only, so changing English text never silently breaks a translation.

## Sources

- Godot Engine documentation, "Internationalizing games" (4.x stable), docs.godotengine.org,
  checked 2026-10-16.
- Weblate documentation, "Fluent format" (docs.weblate.org); Crowdin store, "Mozilla Project
  Fluent FTL" (store.crowdin.com/ftl), checked 2026-10-16.
- Unreal Engine 5 documentation, "Localization overview" and "Text formatting",
  dev.epicgames.com, checked 2026-10-16.
- Unity Localization 1.5 manual (overview, Smart Strings), docs.unity3d.com, checked
  2026-10-16.
- Project Fluent, syntax guide (selectors), projectfluent.org; fluent-rs,
  github.com/projectfluent/fluent-rs, `fluent-bundle/src/types/number.rs` and
  `fluent-bundle/Cargo.toml` on `main`, checked 2026-10-16; crates.io: fluent-bundle 0.16.0,
  fluent-syntax 0.12.0, intl_pluralrules 7.0.2.
- GNU gettext manual, "Plural forms" (the `Plural-Forms` header).
- ICU4X, github.com/unicode-org/icu4x; Unicode blog: ICU4X 2.0 (2025-05-29), 2.1 (CLDR 48,
  2025-10-30), 2.2 (CLDR 48.2, 2026-04); crates.io: icu 2.3.1, icu_plurals 2.3.0,
  icu_decimal 2.3.0, icu_datetime 2.3.0, icu_locale 2.3.1, checked 2026-10-16.
- unicode-bidi 0.3.18, crates.io, checked 2026-10-16; Unicode Standard Annex #9.
