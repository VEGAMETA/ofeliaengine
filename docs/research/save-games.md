# Save games: what is saved, the format, quicksave and autosave

Research for queue item 4 (core audit 3 §4: "save games (versioned, stable format, §13):
missing; ECS snapshots exist for rollback only"). Checked 2026-10-16.

## What a game needs

1. **Quicksave, quickload and autosave in one line each** (the owner, 2026-10-16): a key
   saves and loads; autosave on an interval and at checkpoints, into slots it rotates.
2. **Everything saved without listing it** (the owner: "gravity and the rest: far too many
   variables"): the game's entities and their data, its resources, the engine's state
   (physics, the simulation clock, random generators, animation). The developer marks only
   what is not saved: caches, effects, derived data.
3. **The developer can step in** (the owner: "if needed, let the developer fix the states
   in an autosave themselves"):
   - a hook before writing, to correct or leave out state;
   - a hook after loading, to fix state and to migrate old saves;
   - the save readable, to look into and fix by hand while developing.
4. **No hitch:** a save is taken inside a frame; encoding, compression and writing must not
   cost the frame. Loading belongs to a loading screen, but should not take seconds.
5. **Old saves load after the game updates:** added fields take their defaults, removed ones
   are skipped; renames and changed meanings go through a migration.
6. **A save never breaks:** power lost while writing leaves the previous save; corruption is
   detected, not loaded as garbage.
7. **A save is untrusted input:** players share saves and edit them. Loading must not run
   code or trust lengths (unlike object deserialization), and its parser is fuzzed.
8. **Slots with metadata** for the load menu: name, date, play time, level, a thumbnail.

## Sources

| Engine or library | Version (checked 2026-10-16) | License | How it saves |
|---|---|---|---|
| Unreal Engine 5 | 5.x (docs and guides) | proprietary (source not read) | A `USaveGame` subclass whose `UPROPERTY(SaveGame)` fields are written by tagged property serialization (`SaveGameToSlot`, async variant); new fields load as defaults, renames need code; `ULocalPlayerSaveGame` adds a data version and `HandlePostLoad` for migrations |
| Godot 4 | 4.x (docs and GDQuest guides) | MIT | No save system: the game collects a dictionary per node of the "Persist" group into JSON, or uses `store_var` (binary `Variant`s; objects off by default, since decoding objects can run code) |
| Unity | 6 | proprietary | No save system: `PlayerPrefs` (the registry on Windows), `JsonUtility`; Microsoft deprecated .NET's `BinaryFormatter` as unsafe for untrusted data |
| [moonshine-save](https://docs.rs/moonshine-save) (Bevy) | 0.8.0 (2026-10-11) | MIT | Saves only entities marked `Save`, their reflected components; separates the saved "model" from the "view" rebuilt on load |
| [bevy_save](https://crates.io/crates/bevy_save) | 2.0.1 (2025-08-16) | MIT/Apache-2.0 | Saves the whole world through reflection, with rollback snapshots |
| [postcard](https://docs.rs/postcard) | 1.1.3 | MIT/Apache-2.0 | A serde binary format with a documented stable wire format since 1.0; not self-describing (field order is the schema) |
| bincode | 3.0.0 is an empty tombstone | MIT | Unmaintained: [RUSTSEC-2025-0141](https://rustsec.org/advisories/RUSTSEC-2025-0141) (2025-12-16) |
| [rkyv](https://docs.rs/rkyv) | 0.8.18 | MIT | Zero-copy archives; changing a type changes the format (no field evolution) |

What they agree on:
- **The developer marks what is saved** in Unreal and moonshine-save; Godot and Unity leave
  it all to the game. Only bevy_save saves everything, and the owner wants the same
  convenience, minus what is marked transient.
- **Field names, not positions, carry evolution** (Unreal's tagged properties, JSON). A
  format whose field order is the schema (postcard, bincode, rkyv) breaks old saves on the
  first changed struct, unless the game versions every type by hand.
- **Saving is asynchronous** in Unreal's recommended path.

## What the engine has

- **`World::save` / `restore`** (`engine_ecs::Snapshot`, ADR 0017): copies every row of
  every archetype, plain-data components only, for rollback; 10,000 entities in 0.023 ms.
  Bound to the build (raw layouts), so not a format on disk, but the right way to take a
  save inside a frame.
- **Reflection** (`engine_reflect`, ADR 0022): names, offsets, attributes (`rename`,
  `skip`), C-like enums, arrays. No strings, lists, optional values or maps yet, and a game's
  state needs them (an inventory, a quest's name).
- **The scene text format** (`engine_scene`, ADR 0029): stable 128-bit `SceneId`s, entity
  references as `@id`, one field per line, missing fields as defaults, unknown fields as
  warnings, a `version` with migrations. Saves can reuse its identity, its references and
  its text form.
- **Replication's schema** (`NetSchema::replicate::<T>()`, ADR 0074) marks components for
  the network: the opposite default from what saves need.
- **Physics:** bodies' poses, velocities, sleeping and gravity factors are readable through
  `PhysicsBackend`; Jolt's own state recorder is bound to its version, like the snapshot.

## Measurements

`cargo run -p engine_scene --release --example save_compare`. ref-1 (Ryzen 7 7700), one
thread, median of 20.
- **The world:** 10,000 entities in 100 hierarchies of 100. Each has a `Transform`; 9,900
  also have a gameplay component of seven fields: health, ammo, a cooldown, a team, a
  state, a velocity.
- **The binary prototype:** each field written as a 4-byte hash of its name and its bytes;
  unknown fields are skipped, missing ones keep their defaults.
- **Both decode to the same world:** the binary's world saved as text equals the text.

| Format | Save | Load | Raw | zstd 1 | zstd 3 | zstd 19 |
|---|---|---|---|---|---|---|
| Text (ADR 0029) | 11.99 ms | 18.76 ms | 3,253 KiB | 341 KiB (2.5 ms) | 370 KiB (3.4 ms) | 271 KiB (0.92 s) |
| Tagged binary | 1.34 ms | 2.88 ms | 2,061 KiB | 277 KiB (1.5 ms) | 287 KiB (1.9 ms) | 242 KiB (0.45 s) |
| Snapshot (rows copied) | 0.023 ms | — | 1,285 KiB | — | — | — |

- **Encoding:** the binary is 9× faster to write and 6.5× faster to read. Decompression is
  at most 1.3 ms for either.
- **On disk:** the formats are within 25 % once compressed. The random ids (160 KB that
  do not compress) and the floats dominate.
- **Taken in a frame, written in the background:** capturing costs the frame 0.02 ms per
  10,000 entities, whatever the format.
- **Where the format shows:** loading. At a million entities that is about 1.9 s for the
  text, 0.3 s for the binary.

## Options

**What is saved.**
1. **Everything but what is marked** (`#[reflect(no_save)]` on a field, a transient marker
   on a component or entity): the owner's request; bevy_save's default.
2. **Only what is marked** (Unreal, moonshine-save): smaller saves, but every new variable
   is a chance to forget one, which is what the owner wants gone.
3. **The raw snapshot on disk:** fastest, but any code change breaks every save.

**The format inside the file.**
- **A) Text** (the scene format, compressed). Readable and editable, its parser exists;
  9× slower to write and 6.5× slower to read.
- **B) Tagged binary.** The fastest of the evolvable ones; read and edited only through the
  engine's tool.
- **C) Both:**
  - games write the binary;
  - `ofeliaengine save dump` / `save pack` turn it into the text form and back;
  - development builds and tests may read the text directly.

  More code: two encodings of one model, both kept.

**Protection against players' edits.**
- A) Integrity only: a checksum, an atomic replace, the previous save kept.
- B) Signed or encrypted by default: the key ships in the executable, so it stops only
  casual edits, and it makes saves attached to bug reports harder to read.
- C) A by default, B as a game's setting.

## What we take (pending the owner's choices of format and protection)

- **Everything but what is marked**, through reflection, which gains strings, lists,
  optional values and maps on the way.
- **Taken in the frame, written behind it:**
  - plain data copied like the snapshot, the rest cloned;
  - the engine's subsystems add their own state: physics bodies and gravity, the clock,
    random generators, animation players;
  - the encoding, zstd and the write run on a background thread.
- **Identity:** the level's own objects by their stable path in the pack, so a save stores
  only what changed relative to the level (moved, changed, destroyed). Spawned objects get a
  `SceneId`, and references between them are remapped through those ids, as in scenes.
- **The file:**
  - a header (magic, format version, encoding, engine and game versions, slot metadata,
    thumbnail), then sections, compressed;
  - a checksum;
  - written to a temporary file and swapped in with `ReplaceFileW`, which keeps the
    previous save as the backup.
- **Where:** the `Saved Games` known folder (`FOLDERID_SavedGames`,
  `%USERPROFILE%\Saved Games\<studio>\<game>`). Steam Auto-Cloud has a root for it
  (`WinSavedGames` in the Construct guide;
  [Valve's page](https://partner.steamgames.com/doc/features/cloud) to check when integrating).
- **Hooks:** before saving (correct or leave out) and after loading (fix, migrate), with
  the save's version.
- **The API:**
  - `quicksave`, `quickload`, `save(slot)` and `load(slot)`;
  - autosave on an interval and at checkpoints into rotating slots, off by default;
  - the slots listed with their metadata for the menu.

No GPL code was read. Unreal's and Unity's behaviour comes from their documentation and
third-party guides, not their source.
