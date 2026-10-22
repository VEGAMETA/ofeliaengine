# Luau's highlighting in the code editor

Checked: 2026-10-22. Input for ADR 0107 E8-1c.

No code was carried over: our scanner (`engine_editor/src/highlight.rs`) is written from
the rules below.

Sources:

- **Luau's own lexer**, `Ast/src/Lexer.cpp` in luau-lang/luau (MIT). Read at master
  (2026-10-20), release 0.743; the engine builds 0.736 through `luau0-src` 0.21.0. It has
  the language's lexical rules.
- **Luau.tmLanguage** by JohnnyMorganz (MIT, pushed 2026-09-23). This is the grammar
  luau-lsp's VS Code extension colors Luau with, so it says what is colored as what.
- **VS Code's tokenizer** and **Godot's script editor** (`SyntaxHighlighter`, MIT), read
  for how they stay incremental.

## How each does it

| Source | How | What we take | What we do not |
|---|---|---|---|
| Luau's lexer | One pass over the bytes. Long brackets go by their level of `=`; `continue`, `type`, `export` are not reserved but read by the parser in context; broken tokens are reported, not thrown | Its rules: the 21 reserved words; long brackets; `--[=x` is a line comment; an unescaped line end ends a quoted string; `\` or `\z` before it goes on; numbers are digits, `.`, `_`, an exponent, then letters and digits (`0x1F`, `0b101`); names are ASCII; backtick strings hold code in braces, nested | Its C++ through FFI: the scanner is about 300 lines of Rust |
| Luau.tmLanguage | Regular expressions per scope, a rule stack carried across lines. Contextual words by lookahead: `type` before a name (not `function`), `export` only before a declaration | What it colors: keywords, `self` and the standard library (after no `.` or `:`), function names at declarations and calls (a name before `(`, `{`, a quote, `[[`), types, attributes (`@native`), interpolated strings with their expressions as code | Regular expressions (no regex engine in the editor's hot path); Roblox's globals (`game`, `workspace`, `Enum`): ours will come from the engine's `.d.luau` (E7) |
| VS Code, Godot | A state at each line's start (inside a long comment or string, a region). Lines are tokenized lazily up to the view, and again from an edited line | The same: a start state per line, dropped from the first line changed, made again up to the lines shown | Re-tokenizing until states converge across shifted lines: not needed at our sizes (measured below) |

## What we take

1. **Runs per line:**
   - **keyword:** reserved words, and contextual ones where they are keywords;
   - **string:** quoted, long and interpolated strings, an interpolated one's text;
   - **comment** and **number**;
   - **function:** a name where it is declared or called;
   - **builtin:** `self` and the standard library's names;
   - **type:** a name after `:`, `::`, `->` or `type`.

   A field or a method (`a.end`, `obj:type`) is a name, whatever its word.
2. **A line's start state:**
   - code;
   - inside a long comment, with its level;
   - inside a long string, with its level;
   - in a quoted string going on past its line's end.

   It is kept per line, dropped from the first line an edit touched, and made again up to
   the lines shown. The lines in view are cut into runs every frame.
3. **Colors** after VS Code's Dark+ theme, which most hands and eyes know.

## Measured (release, ref-1)

| What | Time |
|---|---|
| The states of 100,000 lines, from nothing | 9.6 ms |
| A frame's 60 lines cut into runs | 5.6 µs |

The 9.6 ms is spent once, for example by jumping to the end of a long file after editing
its start. An edit next to the view redoes only the lines between them.

## Left for later

- **Semantic colors** from Luau's analysis (E8-3): locals and globals, unknown names.
- **Interpolated expressions across lines:** they reset at the line's end, and Luau
  allows a line break only in their code.
