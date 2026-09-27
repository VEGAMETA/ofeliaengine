# Command palette and shortcuts

Checked: 2026-09-26. Products were studied as products (behavior, documentation). The
open-source ones (VS Code, Godot) were read for ideas only; no code was carried over.

CLAUDE.md §12:
- every action is reachable by search (command palette);
- the editor is keyboard-first;
- hotkeys are rebindable, and conflicts are highlighted.

## How five editors do it

| Editor | Open | What it searches | Matching | What works | What annoys |
|---|---|---|---|---|---|
| VS Code | Ctrl+Shift+P (commands), Ctrl+P (files; `>` switches to commands) | commands with category prefix ("Git: Commit") | fuzzy subsequence with highlights; recently used first | one box, prefixes switch modes, shortcut shown next to each command | long category names push the verb to the right |
| Blender | F3 (menu search) | menu entries with their menu path | fuzzy, recent first | finds anything in menus; path teaches where it lives | only what is in menus; operators without menu entries are missing |
| Godot 4 | Ctrl+Shift+P | editor commands, with shortcut | fuzzy | shortcut shown and rebindable in Editor Settings | separate from scene/file search |
| JetBrains | Shift Shift (Search Everywhere) | actions, files, symbols, settings in tabs | camel-hump and word-start matching ("gtd" → "Go To Declaration") | everything in one place; word-start matching is fast to type | slow to open in large projects |
| Sublime Text | Ctrl+Shift+P | commands | fuzzy with strong bonuses for word starts | fast and predictable ranking | none of note |

## What we take

1. **One box for everything**, like VS Code and JetBrains. Commands by default; prefixes
   switch the source: `>` commands, `@` scene objects, `#` assets, `:` settings. Every
   source uses the same matcher.
2. **Ranking** (VS Code, Sublime, JetBrains):
   - the query must be a subsequence of the title (case-insensitive);
   - bonuses for matches at word starts and camel humps ("gtd" → "Go To Definition")
     and for consecutive characters;
   - a penalty for gaps and for a late first match;
   - recency: recently run commands rank higher among similar scores.
3. **Show the shortcut** next to each command, so the palette teaches shortcuts
   (Godot, VS Code).
4. **Category as a dim prefix** ("Scene: Duplicate"). Category words match, but a
   match in the title ranks higher (VS Code's long-category complaint).
5. **Shortcuts:**
   - one map from key chords to command ids, rebindable, saved as text;
   - conflicts, where one chord maps to two commands in the same context, are reported
     when bound and listed in settings (§12);
   - context scopes (global, viewport, text field) so the same chord can mean different
     things where that is intended.
6. **Speed:** matching 10,000 entries per keystroke must stay well under a frame (target
   < 1 ms), without allocating per keystroke.

## What we do not take

- Separate search UIs per source (Godot has both a command palette and a quick-open
  dialog).
- Menu-only search (Blender): commands that are not in menus must be findable too.
