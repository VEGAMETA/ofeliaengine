# Sound tools (E11)

Checked: 2026-10-28. Products were studied as products, from public documentation, articles
and videos; facts marked * were not found there again today (from use and older
documentation). Audiokinetic's pages refused automated reading today (HTTP 403), so Wwise's
row rests on the excerpts its search results show. No code was read or carried over. Input
for [ADR 0107](../../adr/0107-editor.md) §7, phase E11, after
[ADR 0024](../../adr/0024-audio-output-and-mixer.md) (the mixer),
[ADR 0050](../../adr/0050-spatial-audio.md) (sound in space) and
[ADR 0105](../../adr/0105-sound-in-the-player.md) (sound in the player).

CLAUDE.md §12: everything undoable, nothing blocks the UI, keyboard-first, immediate
feedback, errors next to their cause, the editor held to the game's speed. §11: buses, sends
and sidechains in one graph, built-in DSP, and nothing on the audio thread that allocates,
locks or waits. §14: the editor's profiler shows audio DSP per bus. ADR 0107's E11: "the
mixer panel (buses, sends, meters, effects), previews, sources' and zones' gizmos; VST3
plugins' windows later".

## Where the engine starts

- **The mixer** (`engine_audio`, ADR 0024):
  - up to 64 buses with the master (`AudioConfig::max_buses`). Each has a parent, a gain
    (linear; every change ramps), a balance, a mute and a voice limit. Buses are added,
    removed and moved under another parent at any time;
  - sends from any bus to any bus, with a gain, tapped before or after the fader
    (`SendTap`, fixed when the send is made);
  - effects appended to the end of a bus's chain, their parameters set by index. There is
    no reordering and no bypass yet;
  - a sidechain per effect, for ducking;
  - any graph without cycles. Each change compiles a schedule on the game thread. A change
    that would make a cycle is refused, and the graph stays as it was. Rerouting keeps
    reverb tails and envelopes;
  - built-in effects: a biquad filter (eight kinds), a compressor, a lookahead limiter
    (1.5 ms of latency), a delay, a reverb. The compressor knows its gain reduction
    (`reduction_db`), but only on the audio thread: nothing publishes it;
  - meters: `Audio::take_peak(bus)` gives each channel's peak after the fader since the
    last call. A read takes it, so there is one reader. Also the mixer clock, mixed and
    virtual voices in total, and stream underruns. There is no RMS, no level per voice and
    no "virtual" flag per voice;
  - the output reports its device, rate, period and latency, and the audio thread's render
    times (p50, p99) and late wake-ups (`Output::info`, `Output::stats`).
- **The player** (ADR 0105):
  - the components: `AudioSource` (plain), `AudioSource3D` (distances, `Inverse` or
    `Linear` falloff, Doppler, occlusion, reverb, priority), `AudioListener`, and
    `ReverbZone` (a box at its entity, `half_extents` in its units, `fade` in metres, a
    `Room`, a priority);
  - **the buses come from code:** `PlayerSettings::audio.buses` lists names, linear
    volumes, parents by name and `reverb` flags. The default is music, effects, voice and
    ui, with effects and voice reverberant. It holds no effects and no sends;
  - the player adds a child `<bus>.world` under each reverberant bus (its 3D sounds with
    `reverb` play there), and a `reverb` bus fed by sends from those children. The reverb
    follows the automatic room and the zones;
  - the world's `Mixer` resource sets volumes and mutes, for an options menu. A volume set
    there replaces the one code gave;
  - sources name their sound by its path under `sounds/`, and their bus by name. A bus name
    the layout lacks plays on the master, silently;
  - sounds of 10 s or more stream. A seek decodes from the file's start, about 1 ms per
    second of offset: a jump to 3:00 costs about 180 ms on the thread that asks, unless
    `PreparedStream` does it elsewhere.
- **The editor:**
  - the bottom bar has an Audio tab with nothing behind it yet. Bottom panels are 240 px
    tall, fixed;
  - the dock shows sounds with a note icon and no thumbnail. `#sound:` (or `#audio:`) finds
    them. Sounds have no import settings;
  - the overlay draws icons and reach lines for lights and cameras (E2-7), and side handles
    for brushes (`engine_editor::handles`). Radius handles are still to build (ADR 0110);
  - while editing, the world stands and nothing plays (E3-1). At Stop the scene comes back,
    and Ctrl+K keeps chosen entities' changes (E3-3);
  - the profiler panel shows the frame's "sound" stage (E6-5);
  - `AudioSource.sound` and `.bus` are plain strings in the inspector: no drop, no list, no
    preview. A renamed sound file does not follow, since E4-7's renames follow asset ids.

## How six tools do it

| Tool | What it has | What works | What annoys |
|---|---|---|---|
| Godot 4.7 (Audio panel, `AudioStreamPlayer3D`, `Area3D`) | the Audio bottom panel (Alt+A): a strip per bus, the master leftmost, each with a VU meter; effects "applied in order"; solo, mute and bypass per bus and an on/off per effect, kept with the volumes, effects and connections in an `AudioBusLayout`, saved by itself to `res://default_bus_layout.tres`, which a project setting names; a bus sends only to a bus further left; a bus silent for 2 s (below −60 dB) is switched off, its meter blue; players name buses as text; `AudioStreamPlayer3D`: an attenuation model, `unit_size`, `max_distance`, an emission cone, an area mask; an `Area3D` sends what plays inside it to a reverb bus (amount, uniformity) or to another bus; the 3D player's gizmo (4.0, PR #60517): one ring where the sound fades out, cold colours for a soft cap, warm for a hard one; a stream's waveform and play button in the inspector*; `playing` ticked in the inspector plays a 3D player in the editor, heard from the editor's camera* | the mixer one key away, under the scene; the layout is one text resource; a meter on every strip; areas change the reverb as the listener moves | routing follows the strips' order; renaming a bus sends its players to Master without a word; solo and mute are saved with the mix, so a bus left soloed stays so in the game; the ring is drawn only, with no handle; the FileSystem dock plays nothing by itself*, and a plugin (Quick Audio Preview) adds click-to-play |
| Unity 6.6 (Audio Mixer, Audio Source, Audio Reverb Zone, Preview) | mixer assets (several at once; a mixer can feed a group of another); the window's Mixers, Hierarchy, Snapshots and Views panels; a strip per group: a VU meter that "shows both RMS and peak hold values", the attenuation fader on the same dB scale (−80 to +20 dB), Solo, Mute and Bypass, effects with a circle each to bypass it, dragged to reorder or onto another strip; Duplicate Group (effects included); send, receive and duck-volume units; snapshots: one always active, the selected one takes the edits, a start snapshot, transitions set per target; exposed parameters for scripts; Edit in Play Mode; an Audio Source's distance curves (volume, spatial blend, spread, low-pass, reverb zone mix), with "the current distance to the Audio Listener … marked in the graph by the red vertical line"; reverb zones as spheres, the inner (full) and outer (fading) radii drawn as circles; a selected source's minimum and maximum spheres, with handles*; a clip's Preview window: Play, Auto Play ("as soon as you select any clip"), Loop | RMS and peak hold in one meter; choosing a snapshot to edit it; the listener's distance on the curve; Auto Play for going through many clips; mixing "while playing it in real time" | the mixer's values "aren't editable in Play mode" until a toggle is found; a parameter set from script "will not change as the game transitions Snapshots"; exposing a parameter is a right-click step; a new send starts at −80 dB with nowhere to go until a unit that takes it exists; "If you Solo a Receive unit, the sound stops playing. This is by design."; under Logarithmic rolloff, Max Distance is ignored; edits made in Play stay in the asset* |
| Unreal Engine 5.8 (submixes, sound classes, attenuation, Audio Insights, MetaSounds) | submixes: a graph (many inputs, one output), an effect chain of preset assets, output, wet and dry levels shown linear or in dB (−120 to 0); sounds' sends to submixes, before or after attenuation; Sound Classes (volume and pitch multipliers, inherited) and Sound Class Mixes (passive or pushed); attenuation as a sphere, capsule, box or cone, with an inner size and a falloff distance measured from its edge, curves, occlusion, a reverb send, priority and focus; the shape drawn around a selected sound*; an Ambient Sound's Details with "Play and Stop options … to preview"; Audio Insights: tick the submixes and buses to meter (only those send data), a meter per channel on a logarithmic dB scale, a live signal-flow graph whose wires follow the signal's strength, "Clear All Mutes/Solos"; MetaSounds: Play in the editor, "a real-time meter on the output", inputs changed while listening | the falloff starts at the inner edge, so the two never cross; only what is ticked is metered; signal strength on the wires; previews from the Details panel | levels live in three systems (submixes, sound classes, mixes); Audio Insights observes, and levels change in assets elsewhere; a Content Browser preview once played at the world's origin through the sound's attenuation, silent far from it (forum, 2018; gone by 4.20); attenuation is typed as numbers, not dragged in the viewport* |
| FMOD Studio 2.03 (Mixer window, snapshots, live update) | the Mixer window: a browser (Routing, VCAs, Snapshots), the mixing desk, the deck with input and output meters for each channel, the overview; strips coloured by kind (group buses white, return buses blue, VCAs red); return buses fed only by sends; VCAs, "not part of the routing chain", for settings-screen volumes; snapshots overriding or blending, each with a scope ("no effect on properties outside of its scope"), a priority and an intensity ("a wet/dry mix dial"); mixer views; solo and mute per strip, and on the master a Monitor button that switches every solo and mute off and back, for A/B; live update to a running game; an asset preview with the waveform, auditioned with and without compression; since 2.03, effects bypassed while the game runs, one at 0 % wet "automatically optimized away" | a snapshot holds only what it changes; the options volume stays apart from the mix; one button for A/B; a bypassed effect costs nothing; while connected, "meters throughout FMOD Studio reflect the audio playing through your game" | live changes "only become a permanent part of your game if you then save and build"; many kinds of strip and snapshot to learn |
| Wwise 2025.1 (Mixing Desk, States, Game Object 3D Viewer) | the Mixing Desk: a grid, a column per strip and a row per group of properties, for the objects put in a Mixing Session; a meter per bus (the Meter view shows the main bus by default); loudness and true-peak meters*; while connected, the strips show a voice playing, a bus being ducked, an effect bypassed; States: property changes with transition times*; the Attenuation Editor's curves (volume, low-pass and more over distance); the Game Object 3D Viewer: emitters and listeners in 3D, with their cones and radii; Soundcaster sessions to try events and game syncs without the game; live editing with a connected game, expanded in 2023.1 | levels, effects, states and positioning in one desk; activity on the strips; attenuation seen in 3D with the game's objects; the mix tried without the game | a Mixing Session must be made and filled before the Mixing Desk can be used; forum reports of the states panel not following state changes; the 3D viewer shows the game's objects in a view of their own, not in the level |
| REAPER 7 (meters) | right-click a meter: peak only, peak and RMS, an RMS bar, before or after the fader; the master shows Peak + RMS when wide enough; loudness as stereo RMS, combined RMS, LUFS-M or LUFS-S, with a readout of the maximum, LUFS-I, the current value or the loudness range; an RMS window (250–300 ms suggested); oversampled ("true") peaks; the number above a meter holds the highest peak until clicked, and Ctrl+click clears every one | peaks and loudness in one meter; the held number and the clip light keep what happened while you looked away | the held number is not the current level, and people look for a live one |

**Meters, the conventions:**
- A peak meter shows the highest sample: what clips. RMS and loudness (LUFS, ITU-R BS.1770)
  show what the ear hears.
- The scale is dBFS: 0 is the digital maximum. Godot's guide puts the working range at about
  −60 to 0 dB.
- A held number and a latched clip light say what happened while you looked away. REAPER
  clears them with a click.
- Games have loudness targets. Sony's ASWG-R001 asks console titles for about −24 LUFS
  integrated (−23 ±2 in its first version) and portable ones for −18, with peaks under
  −1 dBTP, measured over at least 30 minutes of play.

## What we take

1. **A strip per bus in the bottom bar's Audio tab** (Godot's place and key, Alt+A): the
   master leftmost, the rest in tree order. Mixing compares levels side by side, which
   strips show and a graph does not. It is the team's Godot layout.
2. **The layout is a project file, not code** (question 1). The panel writes it: buses,
   faders, balances, voice limits, outputs, sends, effects and their parameters. The player
   reads it at start and again when it changes. Every tool above keeps its mix as data, and
   §13 wants text that merges. Undo, autosave and hot reload then work as they do for
   scenes.
3. **Any routing the mixer allows.** A bus feeds any parent and sends to any bus. The output
   and send lists grey out the buses that would close a loop. ADR 0024 compiles any graph
   without cycles; Godot's rule of sending only to the left is what we avoid.
4. **Meters that keep what happened:** stereo peak bars on a dBFS scale (−60 to +6), a hold
   mark, a held number above each strip (a click clears it, Ctrl+click clears every one:
   REAPER), and a clip light latched above 0 dBFS, with a sign, not colour alone. RMS beside
   the peak (Unity's meter) and loudness on the master come in a later step. Peaks exist
   already. Colour alone fails colour-blind users (§12). Loudness is what targets are
   measured in.
5. **Metered only when asked.** The editor reads the peaks of the strips on screen. RMS and
   loudness run on the audio thread only while the panel asks for them (Unreal's ticked
   submixes), so a shipped game pays nothing (CLAUDE.md §5).
6. **Solo and mute are listening aids.** They live in the session, never in the file. A
   Monitor button turns them all off and back (FMOD). Soloing a bus keeps what it feeds and
   the room reverb it sends to. A bus muted while mixing must not ship muted (Godot saves
   both with the mix), and a soloed return must not go silent (Unity's "by design"). The
   game mutes through `Mixer`.
7. **The game's volume sits on top of the mix** (FMOD's VCAs): `Mixer::set_volume`
   multiplies the fader instead of replacing it. An options menu at 80 % must not undo a
   designer's −6 dB. No new object is needed: the game side multiplies before it sends the
   gain.
8. **Effects on the strip, their parameters in the inspector** (Godot, Unity). An effect is
   a row: a bypass dot, its name, and a gain-reduction number for dynamics. A click shows its
   parameters in the inspector: units, scrubbing by the owner's rules, typed expressions, ↺,
   undo. Rows drag to reorder or onto another strip (Unity). A bypassed effect leaves the
   schedule and costs nothing (FMOD 2.03). Each built-in effect describes its parameters
   (name, unit, range, default, linear or logarithmic). That is the shape VST3 parameters
   have, so plugins show the same way later.
9. **The room's buses shown for what they are.** A reverberant bus's `.world` child folds
   into its strip. The `reverb` bus is a strip marked automatic: its parameters read-only and
   named by what drives them ("Hall zone, 62 %"), its fader a trim kept in the file. Hiding
   them hides where the reverb's level comes from; showing them raw confuses.
10. **Every action a command and an undo step** in the one history (ADR 0107 §3). A fader
    drag is one step, Escape takes it back, and steps read "Mixer: music −6 dB". After a
    click in the panel its keys work (a key context of its own): ← and → between strips, M,
    S, B, F2, Delete, Ctrl+D (Unity's Duplicate Group, effects included).
11. **Edits while playing apply at once and stay** (question 3): Unity's mixing "while
    playing it in real time", without its toggle, and without FMOD's "save and build".
    Strips changed since Play are marked until Stop, and one command takes them all back.
    Mixing happens while the game plays.
12. **Bus names follow renames.** A rename rewrites the `bus` fields of the open scene in the
    same step and lists the other files that name the bus. The inspector's `bus` field
    becomes a list of the layout's buses, and a name the layout lacks is marked on its field.
    Godot's silent fallback to Master is the thing to beat (§12: errors next to their
    cause).
13. **Previews never go through the world or the game's buses.** A file from the dock plays
    dry, on an editor bus straight into the master, one at a time; Escape stops it. Unreal's
    old preview at the world's origin went silent. A solo or a muted music bus must not hide
    what a file sounds like.
14. **Previews where sounds are found.** In the dock: a waveform on each tile, ▶ on hover,
    Space on the picked file, the arrows to the next, Auto Play (Unity). In the inspector:
    the whole waveform and a click to seek (FMOD, Godot*). Forty footsteps should take forty
    keys, not a hundred clicks, and a waveform shows length, silence and clipping at a
    glance.
15. **Hear a source where it is.** P (or ▶ on its component) loops the selected sources as
    the game would play them: through their bus, from their place, heard from the editor's
    camera, inside the zones around it (Unreal's Play in Details; Godot's `playing` in the
    editor*). Without it, each distance tweak is a trip through Play.
16. **Gizmos to drag.** A selected `AudioSource3D` shows its two spheres, each with a handle.
    A `ReverbZone` shows its box with the brushes' side handles, and its fade with one
    handle. The ring under the pointer says what it means ("40 m · −16 dB, held"). Godot's
    ring and Unreal's shapes are drawn only*; dragging while the sound loops closes the loop.
17. **How loud it is from here.** The inspector's falloff curve marks the camera's distance
    (the listener's in Play) and the level there (Unity's red line). It turns a curve into a
    number.
18. **Coverage at a glance.** "View: Sound Ranges" draws every source's outer ring on the
    ground and every zone's box, dim, as Alt+C draws colliders. Level design asks where sound
    does not reach, which rings on the selection alone cannot show.

## What we do not take

- **A graph as the place to mix** (Unreal's submix graph, Audio Insights' signal flow):
  strips compare levels, and routing reads from each strip's output and sends. A routing
  view can come later.
- **Three systems for levels** (Unreal's submixes, sound classes and mixes): one tree of
  buses.
- **Several mixers at once** (Unity): one layout per game. A level that needs its own mood
  uses a snapshot (question 2).
- **VCAs as objects** (FMOD): the game's volume multiplies each fader instead.
- **Views** (Unity, FMOD) **and Mixing Sessions** (Wwise): there are at most 64 buses. A
  filter box and folded children are enough, and nothing must be set up before the mix
  shows.
- **An Edit in Play Mode toggle** (Unity), **or save and build to keep live edits** (FMOD).
- **"Expose to script"** (Unity): every parameter has a path
  (`music/Compressor/threshold`), as scene fields do.
- **Solo and mute in the file** (Godot).
- **A limiter on the master by default** (Godot's guide calls one good practice): its
  lookahead adds 1.5 ms to every sound. The master's clip light says when one is needed, and
  adding it takes two clicks.
- **Reverb zones as spheres** (Unity): ours are boxes that fit rooms, with a fade.
- **A field that does nothing in one mode** (Unity's Max Distance under Logarithmic): both
  our falloffs use both distances, and the ring's label says how.

## The proposal

### The mixer panel

```text
┌ Audio  [filter    ]  + Bus  Monitor  Base ▾       Speakers · 48 kHz · 10 ms · 0.7 % · 37/210 ┐
│ master        music         effects       voice         Room (auto)                        │
│ + effect      ● Low-pass    ● Comp −3.1   + effect      ● Reverb                           │
│               + effect      + effect                    Hall zone 62 %                     │
│                             → room (3D)   → room (3D)                                      │
│   −0.3          −8.2          +2.3 !        −12.5         −20.4       held peak, clip       │
│   ▌▌ ┃          ▌▌ ┃          ▌▌ ┃          ▌▌ ┃          ▌▌ ┃        meter L R, fader      │
│   0.0           −6.0          0.0           0.0           trim 0.0                         │
│   M S B ∞       M S B ∞       M S B 32      M S B ∞                                        │
│   → device      → master      → master      → master      → master                         │
└────────────────────────────────────────────────────────────────────────────────────────────┘
```

- **Where:** the bottom bar's Audio tab, Alt+A, "View: Audio Mixer". It opens taller than
  the other bottom panels (about 320 px) once a bottom panel's top edge can be dragged, and
  in a window of its own once docks can move (§12).
- **The header:** the filter (always shown), + Bus, Monitor, the snapshot list (question 2),
  and the output's state: device, rate, period, the audio thread's p99 render time as a
  share of the period, mixed and virtual voices, stream underruns. All of it exists
  (`Output::info`, `Output::stats`, `Audio::voice_counts`).
- **A strip, top to bottom:**
  - the name (F2 renames);
  - the effects, a row each (dot, name, gain reduction), then `+ effect`;
  - the sends, a row each (→ target, level, before or after the fader), then `+ send`;
  - the balance;
  - the fader beside the stereo meter, with ticks at 0, −6, −12, −24 and −48 dB and more
    travel near 0 dB, as in DAWs. The number under it is typed into after a click, as the
    inspector's numbers are;
  - the held peak above the meter, and the clip light;
  - M, S, B and the voice limit (∞ by default);
  - the output: → parent, a list.
- **Order:** the master, then the tree depth first. A parent can fold its children into its
  strip.
- **The room:** the automatic reverb strip at the right end.
- **Short panels:** effects and sends fold into counts ("3 effects"), and the fader keeps its
  room.
- **In Play:** the meters are live. A dot marks each strip changed since Play. What the game
  set on top (its `Mixer` volume, a snapshot) shows as a mark beside the fader, while the
  fader shows the file's value. With the game in its own window (the default), the panel's
  keys stay the editor's.
- **Speed:** strips off screen are not built; texts are made again only when their value
  changes; no allocation per frame; well within ADR 0107's 1 ms UI budget (measured when
  built).

| The developer wants to… | Ours | Actions |
|---|---|---|
| find what makes the game too loud | F5, Alt+A: the master's clip light is lit, and each strip's held number shows which bus went over ("+2.3" on effects) | 2 keys |
| turn the music down 6 dB while playing | drag music's fader (Shift: whole dB from the start; Ctrl: rounds), or click its number and type `-=6`; one undo step, still there after Stop. Unity: open the mixer, turn on Edit in Play Mode, drag; FMOD: connect, drag, save, build | 1 drag, or a click and 4 keys |
| hear the dialogue alone | S on the voice strip: what voice feeds and the room reverb stay; S again, or Monitor to clear every solo and mute | 1 |
| duck the music under dialogue | on music, + effect, Compressor; in the inspector, Sidechain: voice; drag the threshold while dialogue plays, the effect's row showing the reduction | 5 |
| add a canyon echo | + Bus, "canyon"; + effect, Delay; on effects, + send, canyon (−12 dB, after the fader) | about 6 |
| hear it with and without the reverb | click the reverb's dot; B bypasses every effect of a strip; bypassed effects cost nothing | 1 |
| put music and ambience under one "world" fader | + Bus, "world"; each strip's output → world (the buses that would close a loop are grey) | 5 |
| rename "effects" to "sfx" | F2 on its strip, and type; the open scene's sources follow in the same step, and other files that name it are listed | 2 + typing |
| take back a mix change made earlier | History lists "Mixer: …" steps among the scene's; click one | 1 |
| know whether sound costs too much | the header: the audio thread at 0.7 % of its period, 37 voices mixed, 210 virtual, no underruns; the profiler's sound stage beside it | 0 |

### Previews in the asset browser

- **Tiles:** a sound's thumbnail is its waveform: the peaks of each pixel column over the
  whole file, the channels mixed, with the length in a corner ("3.2 s"). It is made on the
  thumbnail thread from one full decode (`engine_audio_decode`) and cached in
  `.ofelia/thumbnails/` (ADR 0112). Below 48 px a tile shows the icon. The tooltip says the
  format, rate, channels, length, size, and whether the sound streams.
- **Playing:** ▶ over the tile under the pointer. Space plays or stops the picked file while
  the dock has focus. The arrows move the pick (new keys for the dock). Auto Play, a toggle
  in the dock's header kept in the session, plays each file as it is picked. A playhead
  crosses the tile, and a click on a playing tile seeks.
- **The inspector:** a picked sound shows its waveform per channel at full width; ▶, ■ and
  loop; "0:01.20 / 0:03.20"; a click or a drag seeks. Facts from the same decode: the peak in
  dBFS, and silence at the head and the tail.
- **How it plays:** on an editor bus into the master, dry. One preview at a time; a new one
  stops the old with the player's 20 ms fade. A seek in a long file is prepared on the
  loading thread, so the frame never waits.
- **A waveform thumbnail, or not: yes.** It shows length, silence and clipping, and tells
  takes apart. It costs one decode per file, once (to measure on ref-1 with a 3-minute Ogg
  file).

| The developer wants to… | Ours | Actions |
|---|---|---|
| find the right footstep among 40 files | click the folder, click the first tile, Space; with Auto Play on, → plays each next file as it is picked | 1 key a file |
| hear the end of a long track | pick it; click at 2:40 on the inspector's waveform: it plays from there, the seek prepared on the loading thread | 2 clicks |
| see whether a file starts late or clips | the tile shows its head; the inspector says "0.18 s of silence first" and "peak −0.1 dBFS" | 0 |
| give a lamp a hum | drag `hum.ogg` onto the lamp, in the viewport or the tree: the lamp gets an `AudioSource3D` playing it, in one undo step; onto empty floor, a new entity "hum" on the surface (E4-3's drop) | 1 drag |
| swap the shot's sound and hear it | the sound field's ▶ plays the current one; a click on the field opens `#sound:`, each highlighted file playing; Enter picks | about 4 |
| stop a preview | Escape, or ▶ again; a new preview stops the old one | 1 |

### Gizmos in the viewport

- **Icons** (E2-7's, picked the same way): a speaker for sources, filled while playing in
  Play; an ear for the listener (the main camera's icon gets one when no listener exists);
  waves for zones. The add menu gains a Sound group: Sound, Sound 3D, Listener, Reverb Zone.
- **A selected `AudioSource3D`:** its full-volume sphere and the falloff's end, each as a ring
  on the level plane through the source and as its outline facing the camera. The end is
  dashed when the sound holds its level beyond it (`Inverse`) and solid when the sound falls
  silent there (`Linear`): shapes, not colour alone. Hover says "6 m · full" or
  "40 m · −16 dB, held".
- **Handles:** a square on each ring, on the side toward the camera's right. A drag sets the
  radius in the view's plane, by the owner's snapping rules, and the inner ring pushes the
  outer one along. One undo step; Escape takes it back. These are the radius handles ADR 0110
  left, so blockout's cylinders and spheres get them too.
- **A selected `ReverbZone`:** its box, turned and scaled with its entity, with the brushes'
  side handles (the opposite side stays, the entity moves by half). Its fade is a dashed,
  rounder box around it, with one handle on top. A label gives its room and priority.
- **Hearing in place:** P plays the selected sources where they are, looping, until P, Escape
  or another selection. The editor's camera hears, and zones apply. Walls and the automatic
  room need the game's physics, so they come with Play. Nothing is written back into the
  world (no `position`, no despawn), so listening never changes the scene.
- **Listen to the Scene:** every source with `playing` set plays from its place while the
  camera flies, under the same rule.
- **In Play:** gizmo edits are entity edits, so E3's rule holds: put back at Stop unless kept
  with Ctrl+K. Ejected (Shift+F8), playing sources' icons are filled and the others hollow.

| The developer wants to… | Ours | Actions |
|---|---|---|
| see how far the waterfall is heard | click it: the full-volume ring and the falloff's end (dashed: it holds beyond); hover the end: "40 m · −16 dB, held" | 1 |
| make it carry further while hearing it | P: it loops from its place, heard from the camera through its bus; drag the outer handle: heard at once; let go: one undo step | 2 |
| set an exact distance | type into the inspector's `max_distance` (expressions as everywhere) | 1 + typing |
| know how loud it is from here | the inspector's curve marks the camera's distance: "−18 dB at 22 m" | 0 |
| fit a reverb zone to the hall | select the hall's brushes; Ctrl+A, Sound, Reverb Zone: the zone takes the selection's box; drag a side or the fade if needed | 3 |
| hear the hall before playing | the camera inside the zone, P on a source: the hall's room answers | 1 |
| find the places with no ambience | View: Sound Ranges: every source's outer ring on the ground, every zone's box, dim | 1 |
| walk the level and hear it | Sound: Listen to the Scene; fly with W, A, S, D; Escape ends it | 1 |
| find which sound hums in Play | Shift+F8 (eject): playing sources' icons are filled, the others hollow; click one | 2 |

### E11's steps

| Step | What | Why there |
|---|---|---|
| **E11-1. The layout and the panel** | the mixer file (question 1), read at start and swapped live by the player; the Audio tab and Alt+A; strips with name, fader, balance, meter, held peak, clip light, M, S, B, Monitor, voice limit and output; the room's strip; undo, autosave, Ctrl+S; edits in Play (question 3); `Mixer` volumes on top of the faders; the header's numbers; the panel's keys ahead of the global ones while it has focus | every later step edits this file; the meters and the bus calls exist |
| **E11-2. Effects and sends** | effects added, removed, reordered (a new `move_effect`) and bypassed (left out of the schedule, cleared when turned back on); parameters in the inspector from each effect's description; sends with their taps; sidechains; dynamics' gain reduction published by the audio thread | they need E11-1's file; the mixer has the effects |
| **E11-3. Previews** | waveform tiles; ▶, Space, the arrows and Auto Play in the dock (its keys ahead of the global ones while it has focus); the inspector's waveform with seeking; the editor's preview bus; sound and bus fields (question 4); sounds dropped on the viewport, the tree or an entity | it needs only E11-1's bus list, so it can go first if the team wants |
| **E11-4. Gizmos and hearing in place** | icons and the add menu's Sound group; rings with radius handles; zones' side and fade handles; P; Listen to the Scene; the falloff curve with the camera's distance; View: Sound Ranges | it stands on the overlay, `engine_editor::handles` and E11-3's preview voice |
| **E11-5. Later** | snapshots (question 2); RMS and loudness meters, computed while the panel asks; DSP time per bus (§14); virtual voices marked; VST3 windows (ADR 0107) | each makes the first four better, and none blocks them |

Measured as they land: the panel's frame cost with 16 strips playing; no allocation with the
panel open in Play; a waveform thumbnail of a 3-minute Ogg file and of a 10-second WAV on
ref-1; the layout's live swap; RMS's cost per 256-frame block. Tested as the shell is,
through the editor's frames without a GPU.

## Questions for the team

1. **Where the bus layout lives, and what becomes of `PlayerSettings::audio`.** Today the
   buses are code: `PlayerSettings::audio.buses` gives names, volumes, parents and reverb
   flags, and nothing else. The panel adds effects, sends, balances and voice limits, and
   its edits must be kept, undone and merged.
   - a. **A `.mixer` file in the project**, in our grammar with its id inside (ADR 0108),
     named by a new `PlayerSettings::audio.layout` (by default `main.mixer` at the project's
     root, outside `sounds/`, which the sound pack takes whole). `buses` goes: without a file
     the player uses the built-in four buses, and `ofeliaengine new` writes the file. The
     rest of `AudioSettings` (folder, pack, streaming, capacities, acoustics) stays in code:
     it is how the engine runs, not the mix. Cost: a file kind with its reader and writer;
     the player's live swap; an entry in the cook (with the one-button build); the dev
     guide's bus section rewritten; the mixer as the one history's first document that is
     not a scene.
   - b. **A section of a new project settings file**, the first of the settings E14 needs
     (the input map, physics layers, localization). Cost: that file's design now (which
     `PlayerSettings` move into it, and which wins when code and file differ); every
     settings change and every mix change in one file, so more merge conflicts; the same
     reader, writer and live swap as a.
   - c. **The layout stays in code.** The panel's edits are live only, with "Copy as Rust"
     to paste into `main`. Cost: no new format, but no autosave or undo across restarts, a
     rebuild (about 1.5 s) after each mix change, and effects and sends still need new code
     APIs.
   - Recommendation: a. A settings file (b) can still name the mixer file later, as Godot's
     project setting names its layout.
2. **Snapshots.** A pause menu that muffles the game, a dive that dulls everything, combat
   that lowers the ambience: sets of mixer values the game moves between.
   - a. **None in E11.** The game changes volumes through `Mixer`, and effect parameters by
     their path (`Mixer::set_param("music/Low-pass/frequency", 800.0, 0.3)`). Cost: nothing
     now; the moods live in code and are heard only in Play.
   - b. **Snapshots in the mixer file:** named sets of changed values (volumes, send levels,
     effect parameters, bypass), each with a fade time. The game picks one by name
     (`Mixer::snapshot("underwater", 0.5)`), one at a time. In the panel, choosing a snapshot
     sends the edits to it (Unity). The values it changes are marked as prefab overrides
     are, bold with a bar and ↺ (FMOD's scope). In Play, choosing one also applies it, so it
     is heard. Cost: a section of the file; fades on the game side (gains already ramp in the
     mixer; effect parameters are stepped during a fade); a list and marks in the panel;
     about one step of work. Scripts reaching it is public scripting API, so that part goes
     to the owner (ADR 0114).
   - c. **b, plus blending snapshots** stacked by priority with an intensity (FMOD). Cost: a
     stack and blend rules to explain; no game asks for it yet.
   - Recommendation: b, as E11's last step: a snapshot only adds to the file.
3. **Mixer edits made while playing.** Play puts the scene back at Stop, and Ctrl+K keeps
   chosen entity changes (E3-3). The mixer is a file, like a material, and mixing is mostly
   done while the game plays.
   - a. **Kept.** An edit in Play is an edit of the mixer file, and an undo step that
     outlives Stop. (The history drops the steps made in Play at Stop, E3-1; the mixer's
     must stay.) Strips changed since Play are marked until Stop. "Mixer: Revert Changes Made
     in Play" takes them back as one step. What the game sets itself (`Mixer` volumes,
     snapshots) is never written. Cost: a rule apart from the entities' one, shown by the
     marks; an accidental nudge stays until Ctrl+Z.
   - b. **Put back at Stop, like entities,** and kept with Ctrl+K in the panel. Cost: one rule
     everywhere, but a mixing session ended by Stop without Ctrl+K is lost: the opposite of
     what mixing in Play is for.
   - c. **Asked at Stop.** Cost: a question at every Stop, against §12's rule of no modal
     dialogs.
   - Recommendation: a. FMOD, Wwise and Unity's Edit in Play Mode* keep what the tool
     changed.
4. **How components name a sound.** `AudioSource.sound` is a path under `sounds/`, and
   scripts use the same strings (`audio.play("ui/click.ogg")`, ADR 0114). Models and
   materials are asset references (a path and an id), which renames follow. A renamed sound
   file silently breaks every source that names it.
   - a. **Keep the string, and mark the field as a sound** with a reflection hint. The
     inspector then shows a sound field: a drop target, the `#sound:` list, a small waveform
     and ▶. A rename or move in the dock rewrites marked strings in scenes and prefabs, and
     lists the scripts' literals (find in files) without touching them. Cost: the hint; the
     asset index keeping marked strings as references; scripts fixed by hand.
   - b. **Make `sound` an asset reference**, like `Model.mesh`; scripts keep strings, looked
     up in the database. Cost: a change to the scene format, the sound pack keyed by id too,
     and the public scripting API's types, which are the owner's (ADR 0114).
   - c. **Leave it a text field.** Cost: no preview or drop in the inspector, and renames keep
     breaking sources silently.
   - Recommendation: a. The format and the scripting API stay as they are, and renames stop
     breaking sources.

## Sources (checked 2026-10-28)

- Godot 4.7, https://docs.godotengine.org/en/stable/ + `tutorials/audio/audio_buses.html`,
  `tutorials/audio/audio_effects.html`, `classes/class_audioserver.html`,
  `classes/class_audiostreamplayer3d.html`, `classes/class_area3d.html`,
  `classes/class_projectsettings.html` (`audio/buses/*`);
  https://docs.godotengine.org/en/4.1/classes/class_audiobuslayout.html;
  https://docs.godotengine.org/en/4.3/tutorials/editor/default_key_mapping.html (Alt+A);
  the 3D player's gizmo, https://github.com/godotengine/godot/pull/60517 (merged
  2022-06-02, 4.0); Quick Audio Preview (Godot 4.0, MIT),
  https://godotengine.org/asset-library/asset/edit/22260.
- Unity 6.6, https://docs.unity.com/en-us/engine/6000.6/manual/ + `audio/mixer/mixer`,
  `audio/mixer/specifics`, `audio/mixer/inspectors`,
  `audio/reference/class-audio-source/source-reference`, `audio/reference/class-audio-clip`;
  reverb zones, https://docs.unity3d.com/2022.2/Documentation/Manual/class-AudioReverbZone.html.
- Unreal Engine 5.8, https://dev.epicgames.com/documentation/en-us/unreal-engine/ +
  `overview-of-submixes-in-unreal-engine`, `sound-classes-in-unreal-engine`,
  `sound-attenuation-in-unreal-engine`;
  https://dev.epicgames.com/documentation/unreal-engine/ +
  `ambient-sound-actor-user-guide-in-unreal-engine`,
  `routing-mixing-in-metering-with-audio-insights-in-unreal-engine`,
  `audio-insights-in-unreal-engine`,
  `metasounds-the-next-generation-sound-sources-in-unreal-engine`, `metasounds-quick-start`;
  the preview at the origin (2018-08-31, not reproduced in 4.20),
  https://forums.unrealengine.com/t/sound-preview-in-content-browser-is-affected-by-attenuation/432885.
- FMOD Studio 2.03, https://www.fmod.com/docs/2.03/studio/ + `mixing.html`,
  `editing-during-live-update.html`, `managing-assets.html`,
  `welcome-to-fmod-studio-new-in-203.html`; read through FMOD's documentation host,
  https://d1s9dnlmdewoh1.cloudfront.net/2.03/studio/, the same pages (fmod.com renders them
  empty to automated reading).
- Wwise 2025.1, excerpts only:
  https://www.audiokinetic.com/library/2025.1.3_9037/?source=Help&id=master_mixer_mixing_desk;
  https://www.audiokinetic.com/en/learn/videos/tschbujjahg/ (Wwise Tutorial 23, Mixing
  Desk); https://www.audiokinetic.com/en/courses/wwise101/?id=creating_new_mixing_desk/;
  the Meter view, https://www.audiokinetic.com/en/search/?page=3&term=mix&version=2025.1.12_9287;
  https://www.audiokinetic.com/library/edge/?source=Help&id=game_object_profiler_game_object_3d_viewer;
  https://www.audiokinetic.com/fr/public-library/2025.1.12_9287/?source=Help&id=positioning_attenuation_editor;
  https://www.audiokinetic.com/en/public-library/2025.1.12_9287/?source=Help&id=understanding_visual_elements_in_wwise;
  live editing, https://www.audiokinetic.com/community/wwisetour/qLO4-9OtPHw/; the states
  panel, https://audiokinetic.com/qa/2616/editing-states-window-in-mixer-layout-not-working;
  loudness meters, https://designingsound.org/2013/02/28/loudness-in-game-audio/
  (2013-02-28).
- REAPER 7: https://timinglis.com.au/reaper-metering-rms-vs-peak/ (2025-12-15, updated
  2026-04-01); https://www.homemusicmaker.com/reaper-master-meter (updated 2023-11-06);
  https://www.reddit.com/r/Reaper/comments/amd4ue/is_there_a_means_to_reset_the_clip_light_on_each/;
  https://gearspace.com/threads/reaper-show-real-time-numeric-signal-level-instead-of-peak.925948/.
- Loudness: Sony's ASWG-R001, http://gameaudiopodcast.com/ASWG-R001.pdf (its revision to
  −24 LKFS); its first version as summarized in
  https://www.stephenschappler.com/2013/07/26/listening-for-loudness-in-video-games/
  (2013-07-26).
