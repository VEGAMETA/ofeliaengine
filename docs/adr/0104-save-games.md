# 0104. Save games: everything but what is marked, taken in a frame, two encodings

- Status: Accepted (the owner, 2026-10-16: both encodings, text written by development
  builds and binary by release ones, switchable; integrity by default, signing or
  encryption as a game's setting). Steps 1 to 4 built, but for sound, thumbnails, GPU
  buffers and signing (see Steps).
- Date: 2026-10-16
- Related:
  - [ADR 0017](0017-ecs.md): the ECS and its snapshots;
  - [ADR 0022](0022-reflection.md): reflection;
  - [ADR 0029](0029-scene-format.md): the scene text format and `SceneId`;
  - [ADR 0099](0099-player-crate.md): the player and the `Game` hooks;
  - [ADR 0101](0101-mesh-colliders.md): the pack's bodies;
  - research note `docs/research/save-games.md`;
  - core audit 3 §4, queue item 4.

## Context

- **Nothing saves a game.** CLAUDE.md §13 asks for "game saves in a separate, stable
  format"; core audit 3 §4 lists them among the core's gaps.
- **The owner (2026-10-16):**
  - quicksave and autosave convenient for the developer;
  - every state saved, gravity and the rest ("far too many variables" to list);
  - the developer able to fix an autosave's states themselves;
  - both a text and a binary encoding, switchable, text by default.
- **The engine's rules:**
  - no hitch: nothing that scales with the save runs in the frame;
  - old saves load after the game updates;
  - a save is untrusted input.
- **Measured** (the research note; ref-1, one thread, 10,000 entities):

  | | Write | Read | Size after zstd |
  |---|---|---|---|
  | Text (ADR 0029) | 12.0 ms | 18.8 ms | 341 KiB |
  | Tagged binary | 1.3 ms | 2.9 ms | 277 KiB |
  | Snapshot of the rows (in a frame) | 0.023 ms | — | — |

## Options

**What is saved.**
1. **Everything but what is marked** (bevy_save's default; the owner's request).
2. **Only what is marked** (Unreal's `UPROPERTY(SaveGame)`, moonshine-save): every new
   variable is a chance to forget one.
3. **The raw snapshot on disk:** any code change breaks every save.

**When the work happens.**
1. **Encode in the frame:** 12 ms of text or 1.3 ms of binary per 10,000 entities, a hitch
   at every autosave.
2. **Copy in the frame, encode and write behind it** (chosen): the frame pays for a copy.

**The encoding:** text, binary, or both (the research note). The owner chose both. The
first suggestion was a flag with text as the default everywhere; the owner then agreed to
write by the build (text while developing, binary in release), since shipped games would
otherwise read a large world in 1.9 s instead of 0.3 s.

## Decision

### What is saved

- **Components:**
  - every component whose type is reflected (`reflect_component!`), unless the type is
    marked `#[reflect(skip)]`: caches, effects, data derived again on load
    (`GlobalTransform` is);
  - every field of it unless marked `#[reflect(skip)]` (reflection's "not saved or
    loaded", which scenes already honour).
- **Resources:** every reflected resource (`reflect_resource!`), with the same marks.
- **The engine's own state, through providers:**
  - physics: gravity, each body's pose, velocities, sleeping and gravity factor;
  - the simulation clock: tick, time, time scale;
  - random generators;
  - animation players.
- **No list to keep:** the reflection macros put each type into a list collected at link
  time (`linkme`'s distributed slices, MIT OR Apache-2.0), so loading finds a type by
  name even before the world has seen it. Checked in step 1 with rust-lld in the dev,
  release and shipping profiles (fat LTO, `panic = "abort"`): types from a crate the
  program names nowhere else are listed too.
- **Reflection grows what a game's state needs:** strings, lists, optional values and maps.
- **Identity:**
  - **the level's own objects** (the pack's bodies and the entities made for them) by a
    stable key from the cook: the hash of their glTF node path. A save stores only what
    changed relative to the level: moved, changed, destroyed. A level re-cooked after an
    edit still matches; an object removed from it drops its saved state with a warning;
  - **objects the game spawned** get a `SceneId` (ADR 0029) and are created again;
  - **references between entities** are written as ids and remapped, as scenes do.

### Taking a save

- **In the frame** (`Player::save` and the rest, at the end of a tick): plain-data
  components are copied by rows, as the snapshot does; the others are cloned; the providers
  copy their state. Then `Game::before_save` may correct or leave out what it likes, on
  the copy: the developer fixing an autosave's states.
- **Behind it**, on a background thread:
  - encoding, zstd and the checksum;
  - the write: a temporary file, flushed, then swapped in with `ReplaceFileW`, which keeps
    the previous save as `<slot>.save.bak`.
- **One save in flight at a time;** an autosave due while one is written is skipped.

### Loading

- **Loads at a tick boundary:**
  1. the level is loaded again;
  2. what changed is applied to it;
  3. spawned objects are created;
  4. the providers restore physics and the clock;
  5. then `Game::after_load` runs with a report (versions, what was dropped and why), to
     fix states and migrate.
- **Old saves:** added fields take their defaults, removed ones are skipped, renamed ones
  follow `#[reflect(rename = …)]`; a type's `#[reflect(version = N)]` is in the save for
  migrations in `after_load`.
- **Untrusted input:** lengths are checked against the file and nothing runs code. The
  decoders are fuzzed (`cargo-fuzz`, CLAUDE.md §15).

### The encodings (the owner's choice: both)

- **Text:** the scene format's line grammar (ADR 0029), grown for strings (`"…"`), lists
  (`[…]`) and maps (`{key: value}`): readable and editable.
- **Binary:** each type's fields named once per file (type name, version, field names and
  kinds), then rows packed in that order. Loading maps the file's fields to the current
  type's by name, once per type. The prototype measured above tagged every field; the
  table of names makes rows about the snapshot's size.
- **Which one is written:**
  - the build chooses: text in development builds (`debug_assertions`), binary in release
    and shipping;
  - `SaveSettings::encoding` overrides either way (text in a moddable game's release, for
    instance).
- **Reading takes both always:** the header says which. Switching never strands a save.
- **The CLI:**
  - `ofeliaengine save dump <file>` writes the text form;
  - `save info` prints the header.

  No `save pack`: every build reads both encodings, so an edited text save loads as it is
  (`engine_save::text_file` makes a file of an edited dump).

### The file

- **The header:**
  - magic and format version;
  - the encoding and flags;
  - the engine's and the game's versions;
  - the slot's name, date, play time and level;
  - a thumbnail for the load menu.
- **Then sections,** each a kind and a length: entities, resources, providers' states, the
  game's own. The body is zstd-compressed and covered by a checksum.
- **Where:** `%USERPROFILE%\Saved Games\<studio>\<game>\` (`FOLDERID_SavedGames`), one file
  per slot. Steam Auto-Cloud syncs that root.

### Time, the GPU's state and rendering caches (the owner, 2026-10-16)

- **Time-dependent shaders** resume at the same phase.
  - Their time comes from the simulation's clock, which is saved, never from the wall
    clock. Materials have no engine time input today: what a game animates in a material
    comes from its own state, which is saved.
  - A shared material time, when the engine adds one, is that clock.
- **State that compute shaders computed** (particles, fluids, deformation on the GPU) is
  not saved by default.
  - It is rebuilt after loading: particles spawn again, a fluid settles.
  - The saved clock and random generators' seeds make a deterministic effect run the same.
  - A game that needs the exact state (a simulation on the GPU that gameplay reads) marks
    its buffer or texture as saved, in one call. It is copied back from the GPU behind the
    frame, so the save completes one or two frames later and the frame does not wait. On
    load, the bytes are uploaded before the first frame.
- **Rendering caches** rebuild themselves: cached shadows, GI, TAA's history, exposure. A
  load is a camera cut to the renderer, so the temporal history starts over and nothing
  smears.

### Timers, tweens, sound and video (the owner, 2026-10-16)

- **Nothing that runs on time starts over after a load.**
  - The engine's clocks are saved: the simulation tick, its time, and the fixed step's
    remainder.
  - A game's timers kept as numbers in components or resources are saved like any value.
- **`Timer`** (engine_runtime) is a countdown on the game's clock: duration, remaining,
  repeating, paused. The player ticks it before `Game::tick`. A game uses it instead of
  `Instant`, which no save can hold.
- **`Tween`** (engine_runtime) moves a component's field on the same entity, named by its
  path (`Transform.translation`, `PointLight.color`):
  - from a value to another over a duration, with an easing;
  - once, looping, or back and forth.

  The player ticks it on the game's clock; it is saved as a component, so it resumes at
  the same point.
- **Sound and music resume from the same place:**
  - each source saves its position, volume, pause and looping;
  - a source marked to start over after a load starts from the beginning, the developer's
    choice;
  - a video saves its position the same way.

  Built with sound in the player (queue item 4), whose sources are saved from the start.

### Protection against players' edits (the owner's choice)

- **By default, integrity only:** the checksum, the atomic swap, the backup. A player who
  wants to edit a save can, with the engine's tool, as in most single-player games.
- **Signing or encryption as a game's setting** (`SaveSettings::protection`), for games
  where it matters. The key ships in the executable, so it stops casual edits only; the
  CLI's `save dump` takes the game's key to read such saves for bug reports.

### The API

```rust
player.quicksave()?;                 // the "quick" slot, written behind the frame
player.quickload()?;                 // at the next tick boundary
player.save("slot 1")?;  player.load("slot 1")?;
let slots = player.saves()?;         // name, date, play time, level, thumbnail
// PlayerSettings::saves: SaveSettings { autosave: Some(Autosave { every, slots: 3 }), .. }
player.autosave_now();               // a checkpoint
// Game::before_save(&mut SaveDraft), Game::after_load(&mut World, &LoadReport),
// Game::can_autosave() (not during a cutscene, say)
```

### Steps

1. **Reflection** (done, `feat/save-games`):
   - `TypeKind::String`, `List`, `Option`, `Map` with functions for their contents
     (`String`, `Vec<T>`, `Option<T>`, `BTreeMap`, `HashMap`);
   - `#[reflect(version = N)]` on a type; `skip` on a type;
   - `reflect_resource!`;
   - `engine_ecs::reflected_types()` and `find_reflected(name)`: every reflected component
     and resource of the program, with a writer of its `Default` value when it has one
     (found without a bound, by method resolution);
   - replication and scenes skip the containers for now (scenes write them with step 2).
2. **`engine_save`** (done, `feat/save-games`):
   - **`capture`** walks archetypes chunk by chunk:
     - plain-data columns are copied as they lie, the rest written at once;
     - the hierarchy comes from the `Parent` column (parent and previous sibling, so the
       children keep their order);
     - saved entities are numbered in a dense table by slot, not hashed;
     - `Transient` leaves an entity out, `#[reflect(skip)]` a type or field;
     - `SaveImage::set_state` and `exclude` are for the providers and `before_save`.
   - **The binary body:**
     - each type's shape once (field names, kinds, enum variants by name);
     - the entities (id or local, parent, previous sibling) with their values;
     - the resources, the states, the destroyed level ids.
   - **The text** is written from the shapes alone, so `dump` needs no game types: the
     same text for a text save and a binary one.
   - **Reading:**
     - fields by name, numbers converted (saturating), variants by name;
     - kinds that changed keep the default, with a note;
     - counts checked against the bytes left before anything is allocated.
   - **`apply`:**
     - matches the level's entities by id, creates the rest, removes the destroyed ones;
     - drops components the save says a level entity no longer had;
     - restores the hierarchy in order, resources from their default.
   - **The file:**
     - header, zstd level 1, checksum;
     - written to `.tmp`, flushed, the old file renamed to `.bak`, the new one renamed in;
     - `read_save` falls back to `.tmp`, then `.bak`;
     - `slot_path` (a hash added when a name had to change), `list_slots` (newest first).
   - **Tests:**
     - a played level comes back from both encodings;
     - the text is readable and the dumps of both encodings are equal;
     - a hand-edited text save loads, and a typo is an error with its line;
     - exclusion;
     - the swap and the fallback;
     - an old type read into a changed one;
     - 3,000 damaged bodies applied without a panic.
   - **Measured** (`cargo run -p engine_save --release --example save_bench`; ref-1, one
     thread):
     - the world: 10,000 entities with an id and a `Transform`, 9,900 with a plain-data
       unit, 100 with an inventory (a name, ten items, five flags);
     - the frame's capture: 0.51 ms (2.1 ms before its hashing was removed);

     | Encoding | Encode and compress | Write the file | Read | Apply | Body | File |
     |---|---|---|---|---|---|---|
     | Binary | 2.5 ms | 1.9 ms | 1.6 ms | 5.7 ms | 1,003 KiB | 159 KiB |
     | Text | 30.8 ms | 2.1 ms | 24.8 ms | 11.6 ms | 3,247 KiB | 177 KiB |

     Applying looks every field up by name; a plan compiled per type would cut that if a
     game's loads need it.
3. **The player** (done, `feat/saves-player`):
   - **`PlayerSettings::saves`:** the folder (`Saved Games\<studio>\<title>`, through
     `SHGetKnownFolderPath`), the game's version, the encoding, autosaves (on time played,
     in turn into `autosave 1..n`, held off by `Game::can_autosave`).
   - **The world's `Saves` resource** takes the game's requests from its hooks and
     systems: `quicksave`, `quickload`, `save(slot)`, `load(slot)`, `autosave_now`,
     `slots`, and the outcomes.
   - **The player** takes the frame's requests after its ticks. A save is captured, its
     clock state added, `Game::before_save` run, then written by a thread of its own
     (joined at drop, so a save asked for before quitting is kept).
   - **A load:**
     1. the level's instances destroyed in play are spawned again from the pack's
        records;
     2. the save's anonymous entities are removed;
     3. `Game::before_load` runs;
     4. the save is applied, matched entities read in place;
     5. the clock is restored;
     6. every camera cuts;
     7. `Game::after_load` runs.
   - **The cook keys every instance** by its glTF node's path and primitive (pack
     version 9): the same across cooks while the hierarchy holds; a renamed node changes
     its own keys only.
   - **Physics:**
     - a moving body's entity takes an id from its first instance;
     - `ScenePhysics::save` and `restore` (one line each in the hooks) keep gravity and
       the velocities, and put the bodies back where their entities were saved.
   - **Engine components saved:** the camera, lights, probes, the GI volume, renderers.
     A renderer is saved with its mesh and material ids, the same while a game adds them
     in the same order; a stale id draws nothing.
   - **`#[reflect(opaque)]`** marks a field whose type is a runtime handle: described by
     its layout, never saved.
   - **`SceneId::named`** names a game's long-lived entities. They are matched on load
     and keep what saves do not hold.
   - **`Timer` and `Tween`** (engine_runtime) are moved by the player before every tick.
   - **A game's own saves:**
     - `SaveImage::new()` for data alone;
     - `CaptureOptions::only` for some entities;
     - `ApplyOptions::add_only()` to add them to another world.
   - **Tested on the player** (offscreen, `tests/saves.rs`): a quicksave and a quickload
     bring back a named entity's state, its timer and tween where they were, a resource,
     the spawned objects; the clock goes on. The quicksave is a 835-byte file, encoded in
     0.11 ms and written in 2.5 ms behind the frame.
   - **Not yet:**
     - sound and video positions (with sound in the player);
     - thumbnails;
     - GPU buffers saved on request;
     - signing or encryption;
     - a destroyed prop's body made again (its entity comes back without a body).
4. **The CLI and the guide** (done): `ofeliaengine save dump` and `save info`;
   `docs/dev-guide/saves.md`.

## Consequences

- A game saves with one call and loses nothing it did not mark. A transient type left
  unmarked costs space, not correctness, and `save info` shows the largest sections.
- `Entity` is `Default` (`Entity::NULL`), so components that hold entities derive
  `Default` and load; `()` is an empty bundle (`World::spawn(())`).
- Text saves while developing can be read, diffed and fixed by hand, and kept in tests.
- Reflection's growth (strings, lists, maps) serves the inspector, scripting and scenes
  too.
- **Costs to watch:** cloning non-plain-data components in the frame (an inventory of
  thousands of strings), measured in step 2 with a budget.
- **Later:** saves across the network (a host's), cloud conflicts (Steam's), per-user
  profiles.
