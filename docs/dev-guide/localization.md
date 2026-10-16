# Localization

Your game's text lives in **Fluent** files (`.ftl`), one folder per language. The engine
compiles them, picks the player's language, formats text with plurals and numbers the way
each language writes them, and falls back to your source language for anything not
translated yet (ADR 0102).

## The files

```
loc/
  en/            the source language: every message is here
    menu.ftl
    hud.ftl
  ru/
    menu.ftl
    hud.ftl
  de/
    menu.ftl     a language may leave messages out: they show in English
```

- **One folder per language**, named by its tag: `ru`, `de`, `pt-BR`, `zh-Hans`.
- **Files are yours to split**: by screen, by system. A message's identifier is unique
  across a language's files.
- **The source language** (`en` unless you say otherwise) has every message. Every other
  language may lack some; they show in the source language until translated.

## Writing messages

```ftl
# Plain text.
play = Play

# A variable: given by the game.
ammo = Ammo: { $count }

# Plurals: the variants each language needs. English has one and other.
items = { $count ->
    [one] { $count } item
   *[other] { $count } items
}

# An attribute: more text on the same message.
quit = Quit
    .tooltip = Leave the game

# A term: a word you reuse. Not shown on its own.
-brand = Ofelia
about = About { -brand }
```

The same file in Russian:

```ftl
play = Играть
ammo = Патроны: { $count }
# Russian has one (1, 21, 101), few (2-4, 22-24) and many (5-20, 25-30).
items = { $count ->
    [one] { $count } предмет
    [few] { $count } предмета
   *[many] { $count } предметов
}
quit = Выйти
    .tooltip = Покинуть игру
# A term with grammatical cases: a message asks for the one it needs.
-brand = { $case ->
   *[nominative] Офелия
    [genitive] Офелии
}
about = О программе { -brand(case: "genitive") }
```

- **Plural categories** come from CLDR, per language: English `one`, `other`; Russian and
  Polish `one`, `few`, `many`, `other`; Arabic `zero`, `one`, `two`, `few`, `many`,
  `other`; Japanese and Chinese only `other`. The default variant (`*`) is taken when none
  matches. An exact number works too: `[0] No items`.
- **Numbers** are written as the language writes them: `1,234,567` in English,
  `1 234 567` in Russian, Arabic digits in Egypt. To choose the digits:
  `{ NUMBER($seconds, minimumFractionDigits: 1, maximumFractionDigits: 1) }` (`12.3`),
  `useGrouping: "false"` (`1234`), and `type: "ordinal"` to select by `1st`, `2nd`, `3rd`.
- **Genders** through a term's attribute:
  ```ftl
  -sword = меч
      .gender = masculine
  found = { -sword.gender ->
      [masculine] Найден { -sword }
     *[feminine] Найдена { -sword }
  }
  ```
- **Comments** (`#`) are for translators: say where the text shows, what the variables
  are.
- **The subset of Fluent** the engine compiles: messages, attributes, terms with literal
  arguments, variables, references to messages and terms, string and number literals,
  selects on variables, `NUMBER(...)`, term attributes and literals. Not yet: `DATETIME`
  (the value is written as it is, with a warning) and functions of your own.

## In the game

The text is the world's `Localizer` resource: every hook that gets the world reaches it.

```rust
use engine_app::{Args, Localizer, loc};

fn present(&mut self, world: &mut World, _alpha: f32) {
    let Some(text) = world.resource::<Localizer>() else { return };
    self.hud.clear();
    text.format(loc!("ammo"), &Args::new().with("count", self.ammo), &mut self.hud);
}
```

- **`loc!("ammo")`**, or `loc!("quit", "tooltip")` for an attribute: the identifier is
  hashed when your code compiles.
- **`format` writes into your `String`**, appending. Once the string has grown it allocates
  nothing: format your HUD every frame. `text(...)` returns a new `String`, for things you
  format once.
- **Arguments:** up to eight; integers, decimals and text (`.with("name", "Ada")`).
- **When the text changes** (the language switched, a file reloaded) `generation()` moves
  on: format what you keep again.
- **Nothing fails:** a message missing in the player's language falls back (`es-MX` →
  `es-419` → `es` → the source language); missing everywhere, its identifier is written; a
  variable not given shows as `{$name}`.

## The player

```rust
use engine_app::{PlayerSettings, TextSource};

let settings = PlayerSettings {
    text: Some(if cfg!(debug_assertions) {
        TextSource::Folder { dir: "loc".into(), source: "en".into() }
    } else {
        TextSource::Pack("strings.pack".into())
    }),
    // The language the player chose in your settings; `None`: Windows' display languages.
    languages: saved.language.clone().map(|l| vec![l]),
    ..PlayerSettings::default()
};
```

- **In development, point it at the folder.** It is compiled when the game starts, and
  again whenever you save a file: the running game shows the change a few frames later. A
  file with errors keeps the previous text of its language; the errors go to the log and
  to `player.text_diagnostics()`, with file, line and column.
- **A shipped game reads the pack** (`ofeliaengine loc build`).
- **The language** is the first of the player's languages your text has (or one of its
  parents: `pt-BR` → `pt`); Windows' display languages if you give none (Settings → Time
  and language → Language and region). Switch at run time:
  `player.localizer_mut().unwrap().set_locale(&["de"])`, or through the world's resource.

## Building and checking

```
ofeliaengine loc build loc -o strings.pack [--source en] [--pseudo]
ofeliaengine loc check loc --code src --code scripts
```

- **`loc build`** compiles every language into a pack. An error stops it (exit code 1),
  with file, line and column.
- **`loc check`** says, for each language, what it lacks against the source; which
  messages use a variable the source's message does not pass (a typo: `$cnt` for
  `$count`; that message is left out, so the source's shows instead of a broken
  `{$cnt}`); with `--code`, the identifiers your code uses (`loc!("…")` in Rust,
  `loc("…")` in Luau) that are not defined, and the messages nothing uses. Errors give exit
  code 1: run it in your build.

## Testing your layouts

Two pseudo-languages are made from your source language:

- **`en-XA`**: `Play` becomes `[Plááýý]`. Accented, a third longer, in brackets. Text that is not
  localized shows plain; a layout too tight for German or Russian overflows; a message
  cut off loses its bracket.
- **`ar-XB`**: every message right to left.

A folder always has them; `loc build --pseudo` puts them into a pack. Switch to one with
`set_locale(&["en-XA"])`.

## What it costs

Measured on ref-1 (Ryzen 7 7700), release, into a string that has grown:

| Message (Russian) | Per format |
|---|---:|
| plain text | 14 ns |
| a plural with a grouped number (`1 234 567 предметов`) | 146 ns |
| a decimal (`12,3 с`) | 63 ns |
| a term in the genitive | 71 ns |
| a fallback to English | 55 ns |

None allocates. 10,000 messages compile in 8.3 ms, cook to 1.1 MB and load in 0.43 ms.
The plural rules and number formats of every CLDR language add about 0.2 MB to your game,
the Fluent parser 120 KB.

## Not yet

- Right-to-left layout of mixed text and fonts per language (next, in `engine_text`).
- Dates (`DATETIME`).
- Luau's `loc(...)`, with the scripting API (item 9).
- The editor's tools for text.
