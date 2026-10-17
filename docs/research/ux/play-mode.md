# Play mode

Checked: 2026-10-17. Products were studied as products (behaviour, documentation); Godot
(MIT) was read for ideas only. No code was carried over. The architecture behind it is in
`docs/research/editor-architecture.md`. Input for [ADR 0107](../../adr/0107-editor.md).

CLAUDE.md §12: minimal modes (Raskin): editing during Play, and changes made in Play can
be kept explicitly. CLAUDE.md §5: entering Play from the editor ≤ 1 s.

## How four editors do it

| Editor | What runs | Window | Changes made while playing | What works | What annoys |
|---|---|---|---|---|---|
| Unreal (Play In Editor) | a copy of the editor's world | the editor's viewport, a new window, or a separate process (Standalone) | lost, except K ("Keep Simulation Changes"), which copies a selected actor back | entering takes seconds; Simulate (no player); F8 ejects to fly around; several clients and a server in one process | K is per actor and hard to discover |
| Unity | the open scene itself, after scripts and the scene are reloaded | the Game view | lost; a tint is the only reminder | "Enter Play Mode Options" skips the reloads and enters almost at once | slow entry by default; lost work |
| Godot 4 | the project in a separate process (F5), or the current scene (F6) | a floating window by default since 4.4, or embedded | edits on nodes picked in the running game are temporary; edits in the editor's own tree are pushed into the game and kept | picking nodes in the running game; edits pushed live | two trees with different rules |
| Defold | the engine in a separate process | its own window | resources edited in the editor are reloaded into the game | hot reload of resources | the game's state is seen only remotely |

## Scenarios

| The developer wants to… | Ours | Keys |
|---|---|---|
| tune a jump while playing and keep the value | F5; select the player in the tree; change `jump_height`; it acts at once; "keep" on the field; F8 | 2 keys, 1 edit, 1 click |
| move props while the game runs and keep the new layout | move them in the editor's viewport (the running copy); "Keep changes" lists them; one click keeps all, as one undo step | 1 click |
| start from where the editor's camera is | "Play from here" (Shift+F5): the player starts at the camera | 1 |
| restart after a change | Ctrl+F5 restarts: stop and play in under a second | 1 |
| look at one tick at a time | F7 pauses; "Step one tick" advances by one | 1 per tick |
| test co-op | "Play with players: 2": a host and a guest in one process, each in a window (later, with ADR 0106) | 2 |

## What we take

1. **A copy of the world runs** (Unreal):
   - the scene's world is copied when Play starts (ADR 0104's copy: 10,000 entities in
     0.023 ms), and the game's hooks run on the copy;
   - the scene's world is untouched until changes are kept;
   - Stop drops the copy and the scene is back at once.
2. **The game's own window by default** (Godot's default; the team tests that way).
   Embedded in a viewport is an option. A separate process comes later, for crashes
   (ADR 0107).
3. **One tree, one inspector.** While playing they show the running copy, and edits apply
   at once. There is no second "remote" tree.
4. **Keeping changes:**
   - entities and fields changed since Play started are marked (a dot in the tree, a
     mark on the field);
   - "keep" per field, per entity (Unreal's K), or "Keep changes" for everything, with
     a list to untick;
   - kept changes enter the scene's world as one undo step;
   - entities the game spawned can be kept as new scene entities.
5. **Quiet controls** (`editor-layout.md`):
   - F5 plays the main scene, F6 the open one, F7 pauses, F8 stops (Godot's keys);
   - Shift+F5 plays from the editor's camera (Unreal's "Play from here");
   - Ctrl+F5 restarts;
   - a thin coloured edge and the status bar show the state.
6. **Pause and step:** pausing stops the ticks while rendering and the editor go on;
   "Step one tick" advances by one tick.
7. **Entering ≤ 1 s** (§5). Nothing is loaded from disk: the copy, the game's `start`
   hook, the second window. Measured when built (ADR 0107, phase E3).
8. **Co-op in one process** (Unreal's multiplayer PIE), later: a host and guests over
   loopback UDP, as ADR 0106's test already runs them.

## What we do not take

- Losing changes made while playing (Unity, Godot's picks in the running game).
- Two trees with different rules (Godot's local and remote trees).
- Reloading everything to enter Play (Unity's default).
