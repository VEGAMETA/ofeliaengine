# 0121. The mixer file

- Status: **Accepted (team decision, 2026-10-28):** option a of the sound UX note's first
  question (`docs/research/ux/sound.md`), with its other three answered the same day by
  their recommendations. The file's lines below follow from that answer and the editor's
  grammar (ADR 0108).
- Date: 2026-10-28
- Amends: [ADR 0105](0105-sound-in-the-player.md) (the buses from
  `PlayerSettings::audio.buses`). Starts [ADR 0107](0107-editor.md)'s E11-1.

## Context

The player's buses came from code: `AudioSettings::buses`, a list of names, linear
volumes, parents and the reverb flag, music, effects, voice and ui by default. The
editor's mixer panel (E11-1) must edit them and keep the edits, as every other tool keeps
its mix: Godot in a `.tres` resource a project setting names, Unity in mixer assets, FMOD
and Wwise in their projects. The UX note asked where they live:

- **a.** a `.mixer` file, in the editor's grammar, named by `AudioSettings::layout`;
  `buses` goes; without a file, the built-in four play;
- **b.** a section of the project settings file;
- **c.** the scene (each scene its own mix).

The team chose **a** (2026-10-28): the mix is the game's, not a scene's (c); a file of
its own is what the panel writes, undoes and reloads as it does scenes and materials, and
what a merge sees (b would mix the mix with unrelated settings).

## Decision

### The file: `.mixer`, version 1

```text
[mixer version=1 id=6b1f0c2e9d8a47b3a5c4e3f2d1c0b9a8]

master.gain = 0
master.balance = 0
buses["effects"].parent = ""
buses["effects"].gain = -3
buses["effects"].balance = 0
buses["effects"].voices = 32
buses["effects"].reverb = true
buses["music"].parent = ""
buses["music"].gain = -8
buses["music"].balance = 0
buses["music"].voices = 0
buses["music"].reverb = false
```

- **The editor's grammar** (ADR 0108, `engine_scene::write_value`): the header with the
  file's id, then a line a value. A bus is keyed by its name, so adding one adds lines
  and moves none (a list's `buses[3]` would renumber every bus after it: merges conflict).
- **Per bus:** `parent` (the bus it feeds by name; empty for the master), `gain` in dB
  (0 passes the sound as it comes; the panel's faders work in dB, as every mixer's do),
  `balance` (−1 to 1), `voices` (the most mixed at once, the least important made
  virtual beyond them; 0 for no limit), `reverb` (its 3D sounds reverberate with the room
  around the listener, ADR 0105's acoustics). The master has its gain and balance.
- **A file names its buses, and no others.** It is read into an empty layout: the
  built-in four (`MixerFile::builtin`) are what plays without a file, not defaults a file
  adds to, so a bus deleted from the file is gone.
- **Order:** the tree depth first from the master, siblings by name: the order buses are
  made in (each after its parent) and the panel's strips stand in. A parent the file
  lacks, a name the player keeps for a bus of its own (`master`, `reverb`, `editor
  preview`, names ending `.world`), or parents that loop (broken at the loop's first bus
  by name) put the bus under the master; `MixerFile::problems` says each, and the player
  logs them.
- **Where:** `AudioSettings::layout`, `main.mixer` under the project
  (`PlayerSettings::project`). A file that cannot be read is logged, and the built-in
  four play.
- **Effects and sends** (E11-2) are lines of the same buses, added then; a file without
  them reads as version 1's.

### The player follows it live

- **`Player::set_mixer_layout`** lays the mixer out again (the panel's unsaved edits), and
  **`Player::asset_changed`** reads the file again when it is written. Buses the layout no
  longer has go, their children and voices on to their parent and the master; new ones
  are made; the others are moved (`set_bus_parent`), faded, balanced and limited where
  they stand, over 20 ms. What plays goes on.
- **The reverb's sends follow:** a bus made reverberant gets its `.world` child and a send
  into the room's reverb; one made dry loses both.

### The game's volume sits on top of the fader

`Mixer::set_volume` (the world's resource, the game's options menu) now multiplies the
layout's fader instead of replacing the bus's gain (the UX note's §7, FMOD's VCAs): an
options menu at 0.8 keeps the mix's −6 dB under it. The game's volumes start at 1 and
stay across a new layout for the buses that stay.

## Options not taken

- **Linear gains in the file** (`volume = 0.5`): what `BusSettings` had. Mixers show and
  type dB; a file a person reads should say what the panel shows.
- **The buses as a list** with an explicit order: merge conflicts on every insertion; the
  order a person sets (strip order) can come later as a field (`order`) that defaults.

## Consequences

- `AudioSettings::buses` and `BusSettings` are gone; a game that set buses in code writes
  a `main.mixer` (or keeps the built-in four).
- `Mixer::volume` now reads the game's own volume (1 to start with), not the bus's gain.
- `.mixer` files are assets (`AssetKind::Mixer`): the editor lists them and hands their
  changes to the player.
- **Not done here:** the mixer panel (E11-1b), effects and sends (E11-2), packing the
  file for a shipped game with no project folder (it is read from the game's folder, as
  scenes are).

## Effects and sends: version 2 (E11-2a, 2026-10-28)

The decision above left effects and sends "lines of the same buses, added then". They
come in version 2; a version 1 file reads as one without them, and an engine of version
1 refuses a version 2 file instead of dropping its effects.

```text
[mixer version=2 id=6b1f0c2e9d8a47b3a5c4e3f2d1c0b9a8]

master.effects.0.kind = "limiter"
master.effects.0.params["ceiling_db"] = -1
buses["effects"].sends["canyon"].gain = -12
buses["effects"].sends["canyon"].pre_fader = false
buses["music"].effects.0.kind = "compressor"
buses["music"].effects.0.bypass = false
buses["music"].effects.0.sidechain = "voice"
buses["music"].effects.0.params["threshold_db"] = -24
buses["music"].effects.1.kind = "low_pass"
buses["music"].effects.1.params["frequency_hz"] = 2000
```

- **Effects are a list** (`effects.0`, `effects.1`, in the grammar's list lines): a chain
  has an order, and an effect put in front renumbers those behind it, as a chain must
  (Godot's `bus/1/effect/0`). The master has effects too (a limiter last, most often); it
  has no sends.
- **A kind by its name** (`engine_audio::dsp::EFFECTS`): `compressor`, `limiter`,
  `delay`, `reverb`, and a filter's shape as its own kind (`low_pass`, `high_pass`,
  `band_pass`, `notch`, `peak`, `low_shelf`, `high_shelf`, `all_pass`), as Godot's filters
  are. Options: **a.** one `filter` kind with a shape parameter, written as a number (0 to
  7: a person reading the file cannot tell a low-pass from a notch) or as a string field
  only filters have; **b.** a kind per shape. Taken: **b**, so every parameter is a
  number and the inspector shows the shape as a choice among the filter kinds.
- **Parameters by name, their units in the names** (`attack_ms`, `gain_db`,
  `frequency_hz`; Godot does the same with `release_ms`): the file says what a number
  means, and a time is in milliseconds where people think of one so (a delay of 250 ms,
  not 0.25 s). `ParamInfo::scale` takes a value in its unit to the effect's own. A
  parameter the file does not name plays at its default; the editor writes them all, so a
  default changed in a later engine does not change a mix. A value beyond its range plays
  at the range's end, and the file's problems say so.
- **`bypass`** leaves the effect out of the mix: it costs nothing, and coming back its
  tails and envelopes start from silence (an echo of what it heard before the bypass
  would be a surprise). **`sidechain`** names the bus whose sound keys it.
- **Sends are keyed by the bus they feed** (`sends["canyon"]`), as buses are by name:
  adding one moves no line. Each has a level in dB and its tap (`pre_fader`).
- **Loops are left out in one order:** the sends of each bus in the buses' order, then
  the sidechains (the master's first), each kept if it does not loop the sound back
  through what is kept so far (`MixerFile::routes`). The engine refuses loops too; the
  file decides which one goes, so the player and the problems the editor shows agree.
- **Problems** add: kinds the engine does not have (the effect holds its place and plays
  nothing), parameters a kind does not have, values beyond their range, sends and
  sidechains naming buses the layout lacks or the player keeps.

### The player follows them live

- **Effects carry on** where the new layout keeps them, their state with them (a
  reverb's tail rings on through a move): each effect of the new chain takes the one of
  the old that the layout gives the same, else the one of its kind at its place, else the
  first of its kind left. Those left over go, new ones are made at their defaults and
  given the file's values, each is moved to its place (`Audio::move_effect`), and only the
  parameters and bypass that changed are sent.
- **Routes the layout no longer has go first**, before any bus moves, so no move loops
  through them. A move that would loop through a route the layout keeps (possible only in
  the middle of a change of many) takes every send and sidechain away, moves, and lays
  them again.
- **A new send fades in** over 20 ms, as faders do; a send whose tap changes is made
  again.

### In `engine_audio` (amends ADR 0024)

- `Audio::move_effect` (its place in its bus's chain), `Audio::set_effect_bypass` (left
  out of the schedule; its sidechain's edge stays, so putting it back never loops; reset
  when it comes back).
- `Effect::reduction_db`, read after each sub-block, published per effect slot as the
  peaks are (atomics, the most since the last read): `Audio::take_reduction` for the
  compressor's and the limiter's meters.
- `dsp::EFFECTS`, `dsp::effect_info`: each built-in kind's name, label, parameters
  (name, label, index, range, default, unit, scale), whether a sidechain keys it and
  whether it reduces gain, and how to make one at its defaults.

### Not done here

- The editor's rows, menus and inspector for them (E11-2b).
- Latency compensation across buses (the limiter's 1.5 ms is not made up elsewhere).
- A game's own effects in the file (a registry of kinds), VST3 plugins (ADR 0107), and
  effects on the room's reverb.

## The room's fader: version 3 (E11-2c, 2026-10-28)

The sound UX note's §9 (accepted with E11's recommendations, team decision 2026-10-28):
the room's reverb is shown for what it is, a strip marked automatic whose parameters
follow the room and whose fader is a trim kept in the file.

- **`room.gain`** (`RoomLayout`), in dB, 0 by default: the room's reverb bus's fader, a
  trim on what the reverb returns. Its parameters stay the room's (the walls, a reverb
  zone): the file holds nothing else of it. A version 1 or 2 file reads with it at 0.
- **The player:** the trim is the reverb bus's gain when the bus is made (the first room
  heard) and with every new layout (over 50 ms). `take_peak("reverb")` meters the bus;
  `Player::room_heard` says what the room is now and what it follows (`RoomFrom`: a zone
  with its weight and whether the walls are heard under it, the walls, or nothing).
- **Options not taken:** the room as an ordinary bus in the file, its reverb's parameters
  editable (the room's would fight them; the UX note's §9: "showing them raw confuses");
  no trim at all (a mix could not bring the room's reverb down without changing every
  zone).
