# 0122. Mixer snapshots: layers with weights

- Status: **Accepted (team decision, 2026-10-28):** the sound UX note's second question
  (`docs/research/ux/sound.md`), answered b as layers with weights: each snapshot sets only
  what it names, several at once, the later in the file's list winning where they meet;
  scripts blend one in and out (`audio.blend("underwater", 1.0, 0.5)`), one snapshot alone
  being a blend to 1. The file's lines and the paths below follow from that answer and the
  mixer file's grammar (ADR 0121).
- Date: 2026-10-28
- Amends: [ADR 0121](0121-mixer-file.md) (the mixer file, version 4);
  [ADR 0114](0114-luau-api.md) (`audio.blend`). Starts [ADR 0107](0107-editor.md)'s E11-5.

## Context

A pause menu that muffles the game, a dive that dulls everything, combat that lowers the
ambience: sets of mixer values a game moves between. The mixer file (ADR 0121) holds one
mix; a game reaches it through `Mixer` volumes and nothing else.

How the tools do it:

- **Unity's Audio Mixer:** snapshots, one active at a time, with transitions; a value set
  from a script stops following them.
- **FMOD Studio:** snapshots that override or blend, each with a scope (it changes only the
  properties in it), a priority and an intensity, several active at once.
- **Wwise:** states and RTPCs layered on the mix, several at once.
- **Godot:** none; a game swaps bus layouts or sets values from code.

The team chose layers with weights (2026-10-28): several moods meet (paused while under
water), and a weight is what a script fades.

## Decision

### The file: snapshots in `.mixer`, version 4

```text
[mixer version=4 id=6b1f0c2e9d8a47b3a5c4e3f2d1c0b9a8]

master.gain = 0
buses["music"].gain = -8
buses["music"].effects.0.kind = "low_pass"
buses["music"].effects.0.params["frequency_hz"] = 20000
snapshots.0.name = "underwater"
snapshots.0.fade = 0.5
snapshots.0.values["music/gain"] = -14
snapshots.0.values["music/effects/0/frequency_hz"] = 600
snapshots.0.values["effects/sends/canyon"] = -6
snapshots.1.name = "paused"
snapshots.1.fade = 0.2
snapshots.1.values["effects/gain"] = -40
snapshots.1.values["room/gain"] = -40
```

- **A snapshot** has a name (unique in the file), a fade in seconds (0.5 by default: what a
  blend takes when it names none), and the values it sets, by path. A path it does not name
  keeps the mix's value: a snapshot is only what it changes (FMOD's scope).
- **Paths** name what a snapshot can set:

  | Path | Value |
  |---|---|
  | `master/gain`, `music/gain` | a fader, in dB |
  | `master/balance`, `music/balance` | −1 (left) to 1 (right) |
  | `room/gain` | the room's reverb's trim (version 3), in dB |
  | `music/sends/canyon` | a send's level, in dB |
  | `music/effects/0/frequency_hz` | an effect's parameter, in its unit (`dsp::EFFECTS`) |
  | `music/effects/0/bypass` | 1 bypassed, 0 not |

  An effect is named by its place in its bus's chain. Bus names cannot hold `/`.
- **The order** of the list is the order the layers stack in: where two snapshots set the
  same value, the later one wins (at full weight).
- **`MixerFile::problems`** names a path that names nothing (a bus, send, effect or
  parameter the layout lacks), a value out of range, and two snapshots with one name.
  The player leaves out what it cannot reach and plays the rest.
- **Older files** (versions 1 to 3) read with no snapshots.

### Blending

Each snapshot has a weight, 0 (not heard) to 1 (all of it). Every value a snapshot sets is
made from the mix's value up the list:

```text
v = the mix's value
for each snapshot, in the file's order, that sets the value:
    v = v + (its value − v) × its weight
```

- A snapshot alone at 1 gives its values; at 0.5, halfway; two at 1 give the later one's.
- dB values blend in dB (a fader's travel); parameters in their units; a bypass is on when
  its blend reaches 0.5.
- The game's volumes (`Mixer::set_volume`) still multiply the blended faders, and the
  editor's listening aids still act on top.

### The game's side

- **`Mixer::blend(name, weight, seconds)`** moves snapshot `name`'s weight to `weight`
  (clamped to 0..1) over `seconds` (`None`: the snapshot's fade), from where it is now;
  `false` for a name the layout has no snapshot for. **`Mixer::weight(name)`** is its weight
  as heard now.
- **Luau** (amends ADR 0114 §4): **`audio.blend(name, weight, fade?)`**, the same, returning
  whether the snapshot exists. Typed in the scripts' declarations:
  `blend: (name: string, weight: number, fade: number?) -> boolean`.
- **Fades** run on the game's thread, a weight moving linearly towards its target each
  frame; the values it changes go to the mixer only when they changed, a fader's gain
  ramping there as every gain does (20 ms), parameters stepped each frame.
- **A new layout** (the editor's edits, a file written) keeps each snapshot's weight by its
  name; a snapshot that goes takes its weight with it.
- **Not kept:** weights are not in saves (as the game's volumes are not), and not
  replicated: they are what a side hears. A game sets them again from its state
  (`on_load`).

### The editor (E11-5b)

- **The header's list:** "Mix" (the file's own values) and each snapshot, in their order;
  + Snapshot, rename (F2), delete, and the order by dragging.
- **A snapshot chosen** takes the panel's edits (Unity): a fader dragged, a parameter
  typed or a send's level set while it is chosen becomes one of its values, one undo step
  as any edit. What it sets is marked as a prefab's overrides are (bold, a bar, ↺ takes the
  value out of it).
- **Heard while chosen:** the player plays it at weight 1 over the mix, in the editor and
  in Play, so what is edited is what is heard; "Mix" hears the game's own weights again.

### Cost

- **No weight moving:** one check a frame; nothing is sent.
- **A weight moving:** for every path the snapshots set, a multiply-add per snapshot that
  sets it, and the changed values sent; the paths are resolved when the layout is laid out,
  so a frame allocates nothing.

## Options not taken

- **None (a):** moods in code (`Mixer::set_volume`, effect parameters set one by one),
  heard only in Play and lost to the panel.
- **One snapshot at a time (the note's b as written):** simpler to explain, but a pause
  over a dive needs a third snapshot made by hand for every pair.
- **FMOD's priority and intensity:** the list's order is the priority, and the weight is
  the intensity; a separate number for each adds rules to learn and no case the order does
  not cover.
- **A snapshot as a partial layout** (the file's own lines under `snapshots.0`): the
  grammar cannot say "not set" for a value with a default, so every snapshot would set
  everything.

## Consequences

- The mixer file gains a section the panel edits and a merge sees; a renamed bus, a moved
  or removed effect and a removed send rename or drop the paths that name them (the
  editor's mixer functions do it in the same step).
- Scripts gain one function in the public API (the team's answer).
- **Later:** blending curves other than linear, snapshots triggered by zones (a zone that
  blends one in where the listener is), and parameters named by an effect's label instead
  of its place.
