# Slang in the code editor

Checked: 2026-10-22. Input for ADR 0107's E10-2 (the materials UX note's step 2,
`docs/research/ux/materials.md`).

The question: how the editor's code tabs get Slang as they have Luau (E8). That means
highlighting, errors as you type, completion, hover, a definition and a call's signature,
and what a save does.

No code was carried over: the scanner (`engine_editor/src/slang_highlight.rs`) and the
checker (`slang_check.rs`) are written from the rules below.

## Sources

- **Slang's language server, `slangd`**:
  - shader-slang/slang, Apache-2.0 WITH LLVM-exception;
  - shipped in Slang's releases (the pinned 2026.18.3) and in the Vulkan SDK's `Bin`
    (1.4.350, Slang 2026.8);
  - `slangd.exe` is 255 KB and calls `Slang::runLanguageServer`, exported by
    `slang-compiler.dll`, the library the engine already loads;
  - `source/slang/slang-language-server.cpp` read at master for its timing and settings.
- **VS Code's Slang extension** (shader-slang/slang-vscode-extension): a client of `slangd`.
  It colors lexically with a TextMate grammar and puts the server's semantic tokens on top.
- **Godot's shader editor** (MIT, read as code): its own shader language. Its parser
  (`ShaderLanguage`) runs in the editor's process for completion and errors, and the
  highlighter colors keywords from the language's lists.
- **Unreal and Unity** (as products): HLSL is written in an external IDE. Unreal's
  material Custom node is a plain text field, its errors shown after Apply.
- **Our Luau tabs** (E8-3): Luau's Analysis is vendored and called in process, on a
  checker thread, 150 ms after the text rests.
- **Slang's command line, `slangc`** (both versions), for the diagnostics' formats.

## slangd, probed

Its `initialize` answer (server "SlangLanguageServer" 1.8):
- positions in UTF-16;
- incremental text sync;
- hover, definition, completion (triggered by `.` `>` `:` `[` space `(` `"` `/`, with
  resolve), signature help (`(` `<` `,`);
- semantic tokens (full; type, enumMember, variable, parameter, function, property,
  namespace, keyword, macro, string);
- document symbols, inlay hints, and formatting through clang-format.

The settings it asks for (`workspace/configuration`, 15 of them, answered in order):
- `slang.predefinedMacros`, `slang.additionalSearchPaths`,
  `slang.searchInAllWorkspaceDirectories`;
- `slang.predefinedLanguageVersion`, `slang.workspaceFlavor`;
- `slang.inlayHints.*`, `slang.format.*`.

A probe of a 13-line module with a typo (`p.roughnes`) and an unfinished `p.`:

| step | time |
|---|---:|
| `initialize` | 93–116 ms |
| diagnostics after `didOpen` | 1.03–1.04 s |
| diagnostics after `didChange` | 1.06 s |
| completion after `p.` (`tint`, `roughness`) | 10–61 ms |
| hover on a function (`func shade(Params p, float2 uv) -> float4`, its file and line) | 10–21 ms |

- **Diagnostics wait a second by design.** `publishDiagnostics` returns early within
  1000 ms of the last change, and the server's loop wakes every 1000 ms.
  - `-periodic-diagnostic-update off` (meant for its tests) published none at all here.
  - Completion and hover answer in milliseconds, so the check itself is fast.

## Material types are not modules on their own

A material type (ADR 0092, `tests/golden/materials/stripes.slang`) is included by the
engine's `forward.slang` as text: `-D MATERIAL=<type> -D MATERIAL_FILE="<module>"`, then
`#include MATERIAL_FILE`. `IMaterial`, `MaterialContext`, `Surface`, `MODEL_LIT` and
`material_params` are `forward.slang`'s.
- **Checked alone,** a correct material type is full of errors (undefined `IMaterial` and
  the rest).
- **`slangd` reads it alone too.** No setting gives it a prelude, so its completion and
  hover know nothing of the engine's types inside a material type.
- **Checked as the engine compiles it,** inside `forward.slang` with its two defines, it is
  exact: ADR 0092 measured that front end at 0.12 s.

## Diagnostics as data

`-enable-machine-readable-diagnostics` (Slang 2026.8 and 2026.18.3) writes one line per
diagnostic, tab-separated:
- code, `error` / `warning`, file, start line and column, end line and column, title;
- a `span` line after it with the detail at its place (`'roughnes' is not a member of
  'Params'.`).

Columns count characters, a tab as one. That was checked with a tab and a two-byte `é`
before the error. `slangc`'s default text is "rich" (Rust-like `error[E30027]: … --> file:4:17`)
and was not parsed.

## Options

- **a) The front end in process for errors, a lexer of our own for colors.**
  - `engine_shader`'s compiler checks the tab's text on the checker's thread, no code
    generated: material types inside `forward.slang`, as they compile, and other modules
    alone.
  - Errors come 150 ms after the text rests, as Luau's do, and exactly as a save's compile
    would find them.
  - Slang's public API (`slang.h`) has no completion, hover or definitions.
- **b) `slangd` as a child process over LSP, for everything.**
  - It has all of VS Code's features with the same answers. JSON-RPC over pipes is cheap
    (serde_json is the editor's already), and a crash takes only the server down.
  - Its errors wait a second, and in a material type they are wrong, as is its
    completion (above).
- **c) `Slang::runLanguageServer` in process.** It reads stdin and writes stdout, the
  editor's own, and it is a C++ symbol, not an API.

## What we take

- **E10-2a: a.** It gives errors as you type, exact in material types, at Luau's
  rhythm, and Slang's lexical colors:
  - keywords;
  - types: the scalar, vector and matrix names, a declared type's name, PascalCase
    names by Slang's convention;
  - a call's or a declaration's name as a function;
  - attributes, `SV_` semantics, numbers with suffixes, strings, comments, block comments
    across lines, preprocessor lines.
- **E10-2b: b for completion, hover, definitions and signatures,** once material types can
  be read alone. A module the engine provides (`IMaterial`, `MaterialContext`, `Surface`,
  the helpers), imported by `forward.slang` and by material types (`import material;`),
  would let `slangd` and the front end read a type alone. Whether it moves any pass's
  registers is measured first (ADR 0092; CLAUDE.md §5).
- Semantic tokens could later color names that the lexical rules cannot tell apart.

## Measured (dev build of the editor's tests, Slang 2026.18.3, ref-1)

The golden `stripes.slang`, opened in a tab of a test project (`engine_editor`'s
`a_slang_material_type_is_highlighted_and_checked_inside_forward_slang`):
- **the first check came 264–268 ms after opening.** That includes the checker's thread
  starting and its Slang session.
- **clean inside `forward.slang`:** no finding of the module.
  - `forward.slang`'s own warnings (an implicit conversion, entry points' capabilities)
    are its own and are not shown.
- **a misspelt field showed at its place 281–282 ms after typing:** 150 ms of rest and
  about 130 ms of check.
- A 3-line module checked alone took 1.0 ms.
