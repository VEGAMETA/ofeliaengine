# Saves

Your game saves and loads in one line: a quicksave on a key, autosaves on a timer,
slots for a menu. Everything your game holds in components and resources is saved
without listing it, and it all goes on from the same point after a load (ADR 0104).

## Quicksave, quickload, slots

```rust
use engine_app::Saves;

fn tick(&mut self, world: &mut World, dt: f32) {
    let saves = world.resource_mut::<Saves>().unwrap();
    if pressed(F5) { saves.quicksave(); }
    if pressed(F9) { saves.quickload(); }
    // A menu: saves.save("slot 1"), saves.load("slot 1"), saves.slots()
}
```

- **A save is taken after the frame's ticks** and written by a thread of its own. The
  frame pays for a copy: half a millisecond for 10,000 objects.
- **A load happens at the next tick boundary.** The next frame shows the loaded world.
- `saves.slots()` lists the saves, newest first. Each has a header for your load menu:
  slot, date, time played, level, the game's version.
- `saves.writing()`, `saves.last_write()` and `saves.last_load()` tell what happened. A
  load's report lists what no longer fit.

## Autosave

```rust
settings.saves.autosave = Some(Autosave { every: Duration::from_secs(300), slots: 3 });
```

- Autosaves go into `autosave 1`, `autosave 2`, `autosave 3` in turn.
- `saves.autosave_now()` saves at a checkpoint.
- `Game::can_autosave` returning `false` holds them off, during a cutscene or a fight.

## What is saved

- **Everything reflected:**
  - every component registered with `reflect_component!`;
  - every resource registered with `reflect_resource!`;
  - their strings, lists, optional values and maps;
  - the hierarchy, in the children's order;
  - references between entities.
- **The engine's own state:**
  - transforms, the camera, lights, renderers, probes;
  - the clock;
  - timers and tweens;
  - sounds: every `AudioSource` and `AudioSource3D` with its position, so music and
    sounds go on from the same place after a load (`restart_on_load` starts one over
    instead);
  - physics, through two calls (below).
- **What you leave out:**
  - `#[reflect(skip)]` on a field or a whole type (caches, effects);
  - the `Transient` component on an entity (UI, particles);
  - `#[reflect(opaque)]` on a field whose type is a runtime handle: it is never saved.
- **The level is not saved, only what changed in it.** The cook gives every object of the
  level a stable id. Loading puts the level's objects back to their saved state, removes
  the ones destroyed and brings back the ones destroyed since the save.

## Your game's long-lived objects: give them a name

```rust
world.spawn((SceneId::named("player"), Transform::default(), Health::new(100)));
```

- **With an id**, an entity is matched on load and only its saved values change. It keeps
  everything saves do not hold: its physics body, its skeleton.
- **Without one**, it is created again from the save. Its saved components come back,
  runtime parts do not: give them back in `Game::after_load`.

## Hooks

```rust
fn before_save(&mut self, world: &World, image: &mut SaveImage) {
    self.physics.save(world, image);          // gravity, bodies' velocities
    image.set_state("quest_log", &self.quests); // anything you keep outside the world
}

fn after_load(&mut self, world: &mut World, save: &LoadedSave, report: &LoadReport) {
    self.physics.restore(world, save).ok();   // bodies where they were, at their speeds
    if let Ok(Some(q)) = save.state("quest_log") { self.quests = q; }
    // Old saves: save.version_of("Inventory"), report.notes
}
```

- **`before_save`** works on the save that was just taken. Correct or leave out what you
  like there: `image.exclude(entity)`.
- **`before_load`** runs once the level's objects are back, before the save is applied.

## Old saves after your game updates

- A field you add takes its default.
- A field you remove is skipped.
- A number that changed its width is converted.
- An enum's variants are matched by name.
- For a renamed field, keep the old name: `#[reflect(rename = "old_name")]`.
- When a meaning changes, raise `#[reflect(version = 2)]` on the type and convert in
  `after_load` (`save.version_of("Type")`).

## Your own saves

Saves are not only for the player's progress:

```rust
use engine_app::save;

// Your data alone, in its own file.
let mut image = save::SaveImage::new();
image.set_state("photo", &photo);
save::write_save(&path, &image, save::Encoding::Binary, &save::SaveHeader::default())?;

// One character out of the world, added to another world later.
let only = |e| e == hero;
let image = save::capture(world, &save::CaptureOptions { only: Some(&only), ..Default::default() });
let (_, loaded) = save::read_save(&path)?;
save::apply_with(other_world, &loaded, &save::ApplyOptions::add_only())?;
```

## Timers and tweens

```rust
world.spawn((Timer::repeating(0.5), Tween::new("Transform.translation", &[0.0, 0.0, 0.0], &[0.0, 3.0, 0.0], 2.0)
    .eased(Ease::SineInOut).repeating(Repeat::PingPong)));
```

- **The player moves them before every tick,** on the game's clock.
- **They are saved like any component,** so after a load they go on from where they were.
- **Use `Timer`, not `Instant`:** no save can hold `Instant`.

## Text or binary, and reading a save

- **Development builds write text, release builds a binary.** Either build reads both.
  `settings.saves.encoding` overrides the choice.
- **A text save can be fixed by hand and loaded as it is.** A typo is reported with its
  line.
- **Inspecting a save:**
  - `ofeliaengine save dump <file>` prints any save as text;
  - `ofeliaengine save info <file>` prints its header.

## Where they go

- **`Saved Games\<studio>\<title>`,** Windows' folder for saves. Set the studio with
  `settings.saves.studio`; Steam Cloud syncs this folder.
- **A file per slot.** A save is written to a temporary file and swapped in, and the
  previous one is kept as `.bak`. If the power goes off mid-save, the last good save
  still loads.
- **A checksum detects a damaged file.** By default nothing more stops a player editing
  saves, as in most single-player games. Signing or encryption is coming as a setting.

## Not there yet

- **A video's position:** videos are not components yet.
- **Thumbnails** for the load menu.
- **A compute shader's results** are rebuilt after a load: particles spawn again, a fluid
  settles. Marking a GPU buffer to be saved is coming.
