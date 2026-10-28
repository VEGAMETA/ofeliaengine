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
