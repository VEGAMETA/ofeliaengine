# 0114. The Luau scripting API: script components with hooks, systems over queries, live views, the engine's own names

- Status: **Accepted (team decision, 2026-10-22): every recommendation of the questions at
  the end.** The owner's answer was "да" to all seven: script components and systems,
  direct writes, the engine's own names, hooks by name, `on_tick` and `on_frame`, timers by
  name, declared props.
- Date: 2026-10-22
- Related:
  - CLAUDE.md §10 (scripting), §5 (performance), §12 (the editor);
  - [ADR 0032](0032-scripting.md): the VM, modules with `state` and `on_reload`, the time
    budget and memory limit, the types at the boundary, the tiers;
  - [ADR 0107](0107-editor.md) §6 and §7: E7 (Luau in the game), E8 (code editor), E9
    (debugger);
  - [ADR 0017](0017-ecs.md), [ADR 0019](0019-networking-model.md),
    [ADR 0026](0026-system-scheduler.md): the world, fixed ticks, the schedule;
    [ADR 0022](0022-reflection.md): reflection;
    [ADR 0066](0066-physics-queries-characters-rollback.md): physics queries and events;
    [ADR 0074](0074-replication-prediction-lag-compensation.md): replication and
    prediction; [ADR 0104](0104-save-games.md): saves;
    [ADR 0105](0105-sound-in-the-player.md): sound; [ADR 0108](0108-scene-format-2.md):
    scenes and prefabs;
  - research: [docs/research/luau-api.md](../research/luau-api.md),
    [docs/research/scripting.md](../research/scripting.md); UX:
    `docs/research/ux/code-editor.md`, `docs/research/ux/script-debugger.md`.

## Context

**What exists:**
- `engine_script::ScriptVm` (ADR 0032): one sandboxed Luau VM (mlua 0.12, Luau 0.736,
  native code), modules that receive a persistent `state` table and return a table of
  functions, reload with `on_reload(old)`, a 50 ms budget per call, a 256 MiB limit, errors
  with module and line. No bindings to the engine.
- **The world** (`engine_ecs`): `Entity` (32-bit index and generation), components with
  reflection, cached queries in a canonical order, `Commands`, `Parent`/`Children`, and
  `World::register_dynamic`, made for components defined at run time by scripts.
- **Reflection** (`engine_reflect`): fields, offsets and types, with `min`, `max`, `step`,
  `tooltip`, `hidden`, `skip`, `net_quantum`, `net_skip`, `hint` (`color`, `degrees`,
  `multiline`) and `version`. The program's reflected types are listed at run time
  (`engine_ecs::reflected_types`). Reflection describes data, not functions.
- **The player's frame** (`engine_app::Game`): `tick(world, dt)` at a fixed rate (64 Hz by
  default), `present(world, alpha)` every frame, then `animate` and `ui`. Input is a world
  resource with a tick view (presses latched between ticks) and a frame view. Timers and
  tweens advance before each tick. The game owns its physics (`ScenePhysics`) and steps it
  in its tick; contact events are arrays read after the step, sorted.
- **Everything is components:** a mesh (`Model`), a sound (`AudioSource3D`, which can despawn
  its entity when it ends), a timer, a tween (`Tween.target = "Transform.translation"`),
  being off (`Disabled`). Saves, scene files and the inspector work from reflection.
- **Networking** (ADR 0074, 0106): plain-data reflected components replicate; a joined
  client predicts by re-running the game's tick for its own entities and rolls back by
  restoring their replicated components.

**What a game developer needs** (CLAUDE.md §10, ADR 0107 E7, the owner's "главное чтобы для
разработчика было удобно"): to give an object behaviour without Rust, see its numbers in
the inspector, have completion and type checking, see an error at its line without the game
stopping, and change code while playing.

**What the API must not cost:**
- the fixed tick stays deterministic (ADR 0019): saves, replays and prediction depend on it;
- no heap allocations per frame in steady state from the engine's side (CLAUDE.md §5);
- a feature costs nothing where it is not used: a hook no script defines is never called;
- names and data the same as in Rust, scene files and the inspector, so one guide serves
  all of them.

## Options

Each decision is listed with its options and their costs; the recommendation follows.

### a. How behaviour attaches to the world

1. **Script components on entities** (Unity, Godot, Defold). A script declares a component
   with hooks; adding the component to an entity gives it the behaviour.
   - Convenience: the most familiar model; one-off level logic (this door, that lever) is
     a few lines; the props show in the inspector.
   - Performance: one call into Luau per entity per hook per tick (to be measured,
     Decision §13): fine for tens to hundreds of objects, wasteful for thousands of alike
     ones.
   - Determinism: the engine fixes the order (below).
2. **Systems over queries only** (Bevy; Roblox's recommended `CollectionService` pattern).
   - Performance: one call per system per tick, however many entities.
   - Convenience: every behaviour is a loop plus a tag component, even for one door; less
     obvious to beginners; an error stops the behaviour for every entity at once.
3. **Both** (recommended): script components for an object's own behaviour, systems for
   many alike objects; one file format, one world API. The cost is a second concept in the
   guide.

Hard to change later: no. Adding systems to (1) later is easy; adding components to (2)
later means a second way to attach that every tool must learn.

### b. Reading and writing components

1. **Live views written through** (Roblox, Godot and Unity for engine objects):
   `self.Transform.translation = p` changes the entity at once.
   - Convenience: the shortest code; the next line, other scripts, the inspector and
     change detection see the write.
   - Performance: each field access is a call into Rust that checks the entity's
     generation and the world's layout and reads at the field's offset (target ≤ 100 ns,
     measured in E7-1). Views are cached per entity and component: no allocation in steady
     state.
   - Safety: a view of a despawned entity raises an error naming the component; it never
     reads freed memory.
2. **Copies with explicit get and set:**
   `local t = world.get(e, Transform) ... world.set(e, Transform, t)`.
   - A table per `get` (an allocation), or a table passed in to fill.
   - Writes are explicit and batchable, but a forgotten `set` silently does nothing: the
     most common bug of this style.
3. **Field paths without objects:** `e:get("Transform.translation")`,
   `e:set("Transform.translation", v)`. Values only, so nothing allocates, but the type
   checker cannot type a path in a string, and the code reads worse.

Recommended: **1**, with (3)'s cost model kept underneath (paths resolved once, values as
Luau values). Hard to change later: yes. Every script ever written depends on it.

### c. Naming

1. **The engine's own names** (recommended): components in PascalCase as in Rust and the
   scene files (`Transform`, `AudioSource3D`), fields, functions and hooks in snake_case
   (`translation`, `min_distance`, `world.find`, `on_tick`), as in our Rust, Luau's own
   library (`string.split`, `vector.magnitude`), Defold and Godot.
   - One name everywhere: code, scene files, the inspector, tween targets
     (`"Transform.translation"`), error messages, the dev guide.
   - The bindings are generated from reflection with no renaming table.
2. **Roblox's style:** PascalCase properties and methods (`Transform.Translation`,
   `world:FindFirstChild`, `OnTick`). Familiar to Roblox's developers and to their
   tutorials, but every name then differs from the scene file and the Rust code, the
   generator keeps a renaming table, and the docs show two names for each field.

Hard to change later: yes, completely.

### d. Events

1. **Hook functions called by name** (Unity's `OnTriggerEnter`, Defold's lifecycle, Godot's
   virtual functions), plus **messages** between scripts: `e:send("open")` calls the hook
   `open` on the target's scripts after the current hook returns (Defold's message queue,
   with the hook's name as the message).
   - Nothing to connect or disconnect; nothing allocates; the engine knows at load which
     hooks a script has, so contact events cost nothing when no script has `on_touch`.
   - Reload: the next call goes to the new function.
   - Order: fixed by the engine; the same on every machine.
   - Limits: one handler per hook per script; a script cannot listen to an arbitrary other
     object's events unless that object sends to it.
2. **Signals** (Roblox, Godot): `e.touched:connect(function(other) ... end)`.
   - Any number of listeners, on any object.
   - Connections must be disconnected or they outlive their owner; a connected closure runs
     the old code after a reload; handlers run in connection order; closures allocate;
     Roblox's deferred and immediate signals behave differently.
3. **Both.** The flexibility of signals with all of their costs.

Recommended: **1**. Hard to change later: adding signals later is possible; taking them
away is not.

### e. Fixed ticks and frames

1. **Gameplay in `on_tick`, presentation in `on_frame`** (recommended), as our Rust hooks
   (`Game::tick`, `Game::present`, ADR 0019's addendum). `on_tick` runs 64 times a second
   with a fixed `dt`, sees the tick's input and changes the game's state; `on_frame` runs
   every rendered frame for what follows the hand at once (the camera) and what only shows
   (bobbing, UI).
   - Saves, replays and networked prediction work, and physics sees steady steps.
   - The developer learns two hooks and when to use which.
2. **One `update(dt)` every frame,** with an optional fixed hook (Unity's `Update` and
   `FixedUpdate`, Roblox's `Heartbeat`, Defold's `update`). Familiar, but gameplay written
   there depends on the frame rate and cannot be predicted, replayed or saved exactly.

Hard to change later: yes; scripts would have to move their logic.

### f. Waiting

1. **Timers that call a hook by name** (recommended): `self:after(3, "close")` calls
   `close(self)` three seconds of game time later. Kept in a component: saved, rolled back,
   and after a reload it calls the new `close`.
2. **Also `wait(3)` inside a hook** (Roblox's `task.wait`, Unity's coroutines, Godot's
   `await`). A sequence reads top to bottom, but a suspended coroutine cannot be saved or
   rolled back: a save during the wait loses the rest of the sequence, a networked client
   cannot re-run it, and after a reload it goes on in the old code.

Hard to change later: adding `wait` later (for presentation-only sequences, say) is
possible.

### g. Where a script's per-object state lives

1. **Declared props, in the script's component** (recommended): everything a script keeps
   per object is declared at the top with its default (`radius = 3.0`). It is saved with
   the game, shown in the inspector, set per instance in the scene file, kept across a
   reload, rolled back by prediction, and replicable. Scratch data that must not be saved
   goes in `self.locals`, a plain table per instance, explicitly unsaved.
2. **Free fields on `self`** (Godot, Unity, Defold): `self.cooldown = 0` anywhere. Easier
   to start with, but such fields are not saved, not shown, not rolled back, and the type
   checker cannot see them.

Hard to change later: yes; it decides what saves and networking can promise.

## Decision

### 1. Files, components and systems

- **Scripts are `.luau` files under the project's `scripts/` folder.** All of them load
  when the game (or the editor) starts, before any scene: scene files name script
  components, so those must be registered first. A release packs them like sounds
  (ADR 0105: a folder while developing, a pack in a release).
- **`component(name, props)` declares a component** registered with the world through
  `World::register_dynamic`, with a `TypeInfo` built from `props` at run time:
  - plain defaults give the type: a number is an `f32`, a boolean a `bool`, a vector a
    `Vec3`, a string a `String`;
  - `prop.*` helpers give the rest and the reflection attributes, under reflection's own
    names: `prop.number(2, { min = 0.1, max = 10, step = 0.1, tooltip = "…", hint =
    "degrees" })`, `prop.integer(0)`, `prop.entity()`, `prop.asset("scene")`,
    `prop.choice("Idle", { "Idle", "Open" })` (a C-like enum), with `hidden`, `skip` (not
    saved), `net_skip` and `net_quantum`;
  - the layout has no hidden padding, so a component of plain props (no strings) is plain
    data: snapshotted, hashed and replicable byte-wise.
- **A component with hooks is a script component.** Adding it to an entity ("Add Component
  → Door" in the inspector, `Door.radius = 3` in the scene file) gives the entity that
  behaviour. A component without hooks is data (a tag, a wallet). An entity takes any
  number of script components.
- **`system(name, hooks)` declares a system:** `on_tick(dt)` and `on_frame(dt, alpha)` called
  once per tick or frame, iterating queries.
- A file may declare several components and systems. What it returns is its module table
  for `require` (Luau's require by string: `require("./wallet")`, aliases from `.luaurc`;
  mlua 0.12's `create_require_function` implements it).

### 2. Hooks

| Hook | When |
|---|---|
| `on_start(self)` | once, before the instance's first tick: at Play or at the scene's load for its entities, after the spawning stage for spawned ones |
| `on_tick(self, dt)` | every fixed tick, `dt` the tick's length |
| `on_frame(self, dt, alpha)` | every rendered frame after the ticks; `alpha` is how far the frame lies between the last two ticks |
| `on_touch(self, other, point, normal)` | another body began touching this entity's body or `Trigger` (delivered at the start of the next tick) |
| `on_untouch(self, other)` | it stopped touching |
| `on_load(self)` | a save was applied: the components hold the saved values |
| `on_stop(self)` | the entity despawned or lost the component, or the world is dropped (Stop, quitting) |
| any other name | called by `send` and `after` |
| `on_reload(old)` | module level, after a reload (ADR 0032) |

`self` is the instance: its props (`self.radius`), its entity's components
(`self.Transform`), the entity's methods (`self:despawn()`), `self.entity` and
`self.locals`. Lowercase names are props and built-ins, PascalCase names are components, so
they never clash.

### 3. The world, entities and components

- **`Entity` is a value:** a tagged light userdata holding the 64-bit handle. It is never
  allocated, compares by value, works as a table key, and `typeof` says `Entity`.
- **Components are live views:** `e.Transform` gives a view, its fields read and write the
  world at once, vectors are Luau `vector`s (written whole: Luau vectors cannot be assigned
  by component). Reading a component the entity lacks is an error that names it; check
  with `e:has(Transform)`.
- **Entity built-ins:** `name`, `parent`, `alive()`, `despawn()` (with its children, at the
  end of the stage), `has`, `add(Component, values?)`, `remove`, `children()` (an
  iterator), `find_child(name)`, `send(hook, ...)`.
- **`world`:** `find(name)` (through a name index the host keeps, so it is cheap every
  tick; names need not be unique, and of several the first in the fixed order wins; Rust's
  `find_named` scans),
  `spawn({ Component = { field = value } })`, `instantiate(prefab, translation?)` (a prefab
  scene, ADR 0108), `query(A, B, ...)` (made once, iterated with `for e, a, b in q do`;
  `:with(C)` and `:without(D)` filter).
- **Structural changes** (spawn, add, remove) apply at once: the new entity's components can
  be written on the next line. A query's loop visits the entities that matched when it
  started. Despawns wait for the end of the stage, as Godot's `queue_free` and Unity's
  `Destroy` do.
- **Types at the boundary** (ADR 0032, made concrete):

| Rust (reflection) | Luau |
|---|---|
| `f32`, `f64` | `number` |
| `i8` to `i32`, `u8` to `u32` | `number`, which must be a whole number in the type's range; clamped to `min`/`max` as the inspector does (ADR 0022) |
| `i64`, `u64` | userdata (ADR 0032); Luau's `integer` once it ships enabled (it is behind a flag in 0.736) |
| `bool` | `boolean` |
| `Vec3`, `Vec3A`; `Vec2` | `vector`; `vector` with z = 0 |
| `Quat` | `Quat` userdata (allocated when made); `Transform`'s methods cover common rotations without making one |
| C-like enum | `string`, typed as the union of its variants (`"Once" \| "Loop" \| "Despawn"`) |
| `String` | `string` |
| `Entity` | `Entity` |
| `AssetRef` | `Asset` (made from a path through the asset database) |
| nested struct, array, `Vec`, map | a view (arrays and lists indexed from 1, as Luau's) |
| `Option<T>` | `T` or `nil` |
| opaque | not visible |

`hidden` and `skip` fields are visible to scripts (those attributes are for the inspector
and saves); angles stay in radians, as in Rust (`hint = "degrees"` only changes the
inspector).

### 4. The libraries

| Library | v1 |
|---|---|
| `world` | `find`, `spawn`, `instantiate`, `query` |
| `input` | `pressed`, `just_pressed`, `just_released`, `axis`, `vector`, `strength`, by the game's action names; in tick hooks the tick's view, in frame hooks the frame's view, chosen by the hook that runs |
| `physics` | `raycast(origin, direction, max, filter?)` returning `entity, point, normal, distance` (several results, no table), `overlap_sphere` as an iterator; filters made once (`physics.filter({ "enemies" })`) |
| `audio` | `play(sound, bus?)` and `play_at(sound, position, bus?)`: an entity with an `AudioSource` or `AudioSource3D` that despawns when the sound ends (ADR 0105) |
| `time` | `now()` (seconds of game time: ticks × `dt`, saved), `tick()`, `dt` |
| `random` | `number()`, `integer(a, b)`, `vector_in_sphere()`: the world's generator, seeded, saved and rolled back (`math.random` stays, outside the simulation) |
| `net` | `is_server()`, `is_client()`, `resimulating()` |
| `print`, `warn` | the engine's log, with the script's file and line |

Later, each with its own step: in-game UI, navigation, animation control beyond components,
localized text, saves on request, video. A game's own Rust code adds functions with the
same macro as the engine's (§6); its reflected components are visible to scripts with no
work.

### 5. Examples

**A door that opens when the player comes near** (a distance check; with a `Trigger` it
would count `on_touch` and `on_untouch` instead):

```luau
--!strict
-- scripts/door.luau: rises while the player is near, sinks back when they leave.
local Door = component("Door", {
    radius = prop.number(3, { min = 0, tooltip = "Metres from the closed door" }),
    lift = 2.5, -- metres
    speed = 2.0, -- metres a second
    closed = vector.zero, -- where it rests, taken at start
})
type Self = Script<typeof(Door.props)>

function Door.on_start(self: Self)
    self.closed = self.Transform.translation
end

function Door.on_tick(self: Self, dt: number)
    local player = world.find("Player")
    local near = player ~= nil
        and vector.magnitude(player.Transform.translation - self.closed) < self.radius
    local goal = if near then self.closed + vector.create(0, self.lift, 0) else self.closed
    local t = self.Transform
    local to_goal = goal - t.translation
    local step = math.min(vector.magnitude(to_goal), self.speed * dt)
    if step > 0 then
        t.translation += vector.normalize(to_goal) * step
    end
end

return Door
```

`closed` is a prop, so a save taken while the door is open still knows where it rests (after
a load, `on_start` runs on the reloaded level, then the save's values arrive).

**A pickup that plays a sound and despawns** (the coin has a `Trigger`; the wallet is a
data component of its own):

```luau
--!strict
-- scripts/wallet.luau: what the player carries.
return component("Wallet", {
    coins = prop.integer(0, { min = 0 }),
})
```

```luau
--!strict
-- scripts/coin.luau: touched by something with a Wallet, it pays, rings and goes.
local Wallet = require("./wallet")

local Coin = component("Coin", {
    value = prop.integer(1, { min = 1 }),
    sound = "pickups/coin.ogg",
})
type Self = Script<typeof(Coin.props)>

function Coin.on_touch(self: Self, other: Entity)
    if not other:has(Wallet) then
        return
    end
    other.Wallet.coins += self.value
    audio.play_at(self.sound, self.Transform.translation)
    self:despawn()
end

return Coin
```

**Spinning props, as a system** (one call into Luau per tick for all of them):

```luau
--!strict
-- scripts/spin.luau: everything with Spin turns about its up axis.
local Spin = component("Spin", {
    speed = prop.number(math.rad(90), { hint = "degrees", tooltip = "Turn a second" }),
})

local spinning = world.query(Spin, Transform) -- made once, iterated every tick
local UP = vector.create(0, 1, 0)

system("Spin", {
    on_tick = function(dt: number)
        for _, spin, t in spinning do
            t:rotate(UP, spin.speed * dt)
        end
    end,
})

return Spin
```

The same as a per-entity hook is `function Spin.on_tick(self, dt)
self.Transform:rotate(UP, self.speed * dt) end`: shorter, one call per spinner.

**Reading input** (the game's actions; the tick's view for movement, the frame's for the
camera):

```luau
--!strict
-- scripts/walker.luau: walks with "move" (faster with "run"); its camera looks with "look".
local Walker = component("Walker", {
    speed = 4.0, -- metres a second
    sensitivity = 0.002, -- radians per count of mouse motion
})
type Self = Script<typeof(Walker.props)>
local RIGHT = vector.create(1, 0, 0)

function Walker.on_tick(self: Self, dt: number)
    local move = input.vector("move") -- the tick's view: a tap shorter than a tick still counts
    local speed = if input.pressed("run") then self.speed * 2 else self.speed
    local t = self.Transform
    t.translation += (t:right() * move.x + t:forward() * move.y) * (speed * dt)
end

function Walker.on_frame(self: Self, dt: number)
    local look = input.vector("look") -- the frame's view: the camera follows the hand at once
    local camera = self:find_child("Camera")
    if camera then
        camera.Transform:rotate(RIGHT, -look.y * self.sensitivity)
    end
end

return Walker
```

The camera's pitch changes in `on_frame` because it only shows; anything the game's state
depends on (the walker's position) changes in `on_tick`.

**The matching `.d.luau` excerpt** (generated; Luau 0.736's definition syntax; its exact
form is checked against luau-lsp in strict mode in E7-1):

```luau
-- types/ofelia.d.luau: generated by `ofeliaengine scripts types` and by the editor at every
-- start, rebuild and script reload. Do not edit.

type Value = nil | boolean | number | string | vector | Entity

-- What `has`, `add`, `remove` and `world.query` take: `Transform`, or a script's component.
declare extern type ComponentType with
    name: string
end
-- `Quat`, `Asset`, `Query` and `Filter` are declared the same way (not shown).

-- An entity: a 64-bit value, compared by value, usable as a table key.
declare extern type Entity with
    name: string
    parent: Entity?
    function alive(self): boolean
    function despawn(self): ()
    function has(self, component: ComponentType): boolean
    function add(self, component: ComponentType, values: { [string]: any }?): ()
    function remove(self, component: ComponentType): ()
    function children(self): () -> Entity?
    function find_child(self, name: string): Entity?
    function send(self, hook: string, ...: Value): ()
    -- Components (the program's reflected types and scripts/*.luau). Reading one the
    -- entity lacks is an error: check with `has`.
    Transform: Transform
    AudioSource: AudioSource
    Wallet: Wallet
end

-- Where an entity is, relative to its parent.
declare extern type Transform with
    translation: vector
    rotation: Quat
    scale: vector
    function rotate(self, axis: vector, angle: number): ()
    function look_at(self, point: vector, up: vector?): ()
    function forward(self): vector
    function right(self): vector
    function up(self): vector
end

-- A sound heard as it is: music, the UI, a narrator (ADR 0105).
declare extern type AudioSource with
    sound: string
    bus: string
    volume: number
    mode: "Once" | "Loop" | "Despawn"
    playing: boolean
    position: number
    restart_on_load: boolean
end

-- scripts/wallet.luau
declare extern type Wallet with
    coins: number -- a whole number, at least 0
end

type Script<P> = P & Entity & {
    entity: Entity,
    locals: { [string]: any },
    after: (self: any, seconds: number, hook: string, ...Value) -> (),
}
type ComponentScript<P> = ComponentType & { props: P, [string]: any }
type PropOptions = {
    min: number?, max: number?, step: number?, tooltip: string?,
    hint: ("degrees" | "color" | "multiline")?,
    hidden: boolean?, skip: boolean?, net_skip: boolean?, net_quantum: number?,
}

declare Transform: ComponentType
declare AudioSource: ComponentType
declare function component<P>(name: string, props: P): ComponentScript<P>
declare function system(name: string, hooks: {
    on_tick: ((dt: number) -> ())?,
    on_frame: ((dt: number, alpha: number) -> ())?,
}): ()
declare prop: {
    number: (default: number, options: PropOptions?) -> number,
    integer: (default: number, options: PropOptions?) -> number,
    entity: (options: PropOptions?) -> Entity?,
    asset: (kind: string, options: PropOptions?) -> Asset?,
}
declare world: {
    find: (name: string) -> Entity?,
    spawn: (components: { [string]: { [string]: any } }?) -> Entity,
    instantiate: (prefab: Asset, translation: vector?) -> Entity,
    query: (...ComponentType) -> Query,
}
declare input: {
    pressed: (action: string) -> boolean,
    just_pressed: (action: string) -> boolean,
    just_released: (action: string) -> boolean,
    axis: (action: string) -> number,
    vector: (action: string) -> vector,
}
declare physics: {
    raycast: (origin: vector, direction: vector, max_distance: number, filter: Filter?)
        -> (Entity?, vector, vector, number),
}
declare audio: {
    play: (sound: string, bus: string?) -> Entity,
    play_at: (sound: string, position: vector, bus: string?) -> Entity,
}
declare time: { now: () -> number, tick: () -> number, dt: number }
```

The `prop.*` helpers are typed as returning their default's type, so `typeof(Door.props)`
gives `{ radius: number, ... }` to the checker; at run time they return descriptors the
engine reads. Tooltips also go into a documentation file for luau-lsp's hovers.

### 6. Bindings and `.d.luau` from reflection

- **Components are bound from their `TypeInfo`, not from generated code per type.** At start
  the script host walks the program's reflected types and the script components and gives
  each a descriptor: its component id, and per field the offset, the kind and the
  attributes. One accessor per kind reads and writes at the offset, as scene loading and
  the inspector already do. Field names are matched against Luau's interned strings, so a
  lookup is a hash probe. If E7-1's measurements show the interpretive accessor too slow,
  a proc macro generates typed accessors per component instead, with the same Luau side.
- **Functions are bound with a proc macro** (CLAUDE.md §10): `#[script]` on a Rust function
  or an `impl` block (`Transform::rotate`, `physics.raycast`) generates the mlua
  registration and the `.d.luau` line, with its `///` comment as the documentation. A
  game's own Rust code uses the same macro.
- **The generated files,** written by `ofeliaengine scripts types` and by the editor at
  start, after a rebuild and after each script reload (a few milliseconds):
  - `types/ofelia.d.luau`: the engine's libraries and types, every reflected component
    (the engine's and the game's Rust ones) and every script component;
  - `types/ofelia.docs.json`: their documentation (luau-lsp's `documentationFiles`);
  - `ofeliaengine new` writes `.luaurc` (strict mode, the `@scripts` alias) and the VS Code
    settings that point luau-lsp at both files.
- **The definition syntax follows the VM's Luau release** (0.736: `declare extern type`).
  luau-lsp calls its definition syntax unstable, so the generator is updated with each Luau
  update, and a test type-checks the examples of this ADR and of the dev guide against the
  generated file (with Luau's analysis library once E8 vendors it; before that, the
  generated text is compared with a reviewed copy).

### 7. Where scripts run in the frame

```text
frame
├─ ticks (0..n per frame, 64 Hz by default)
│  ├─ input sampled for the tick; timers and tweens advance        (the player, today)
│  ├─ scripts:
│  │  ├─ `after` timers that are due
│  │  ├─ the last physics step's contacts: on_touch, on_untouch
│  │  ├─ on_start of new instances, in spawn order
│  │  ├─ on_tick of script components, in the fixed order
│  │  ├─ systems' on_tick, in declaration order
│  │  │  (each hook's messages delivered when it returns)
│  │  └─ despawns applied, on_stop
│  ├─ Game::tick (Rust; steps physics)                              (today)
│  └─ transforms propagated                                         (today)
├─ presentation: scripts' on_frame (input's frame view), then Game::present
├─ animate, UI                                                      (today)
└─ render
```

- **The fixed order:** the scene tree's order for the scene's entities, then spawn order;
  script components of one entity in name order. It depends on neither archetypes nor
  threads, and it is the same on every machine.
- **Messages** sent by a hook are delivered after it returns, in send order; messages sent
  while delivering go in the same pass, up to 16 passes a stage (then the rest wait for the
  next stage, and the editor says so). A message to a despawned entity is dropped; a hook
  no script of the target has is reported once in the editor.
- **Scripts run on the game thread,** before the Rust game's hook in each stage, so a
  script's kinematic target or velocity is used by the same tick's physics step. Parallel
  Luau (separate VMs on the job system) is not part of this version.
- **In the editor,** scripts load in edit mode (their components show in the inspector and
  the add menu) but their hooks run only in Play; pause and step (ADR 0107 E3-2) step the
  scripts with the ticks. Scripts that run in the editor itself (tools) are a later step.

### 8. Determinism, saves and networking

Rules for everything that runs in ticks (`on_tick`, `on_touch`, `on_untouch`, `on_start`,
`after` and messages sent from ticks), after Roblox's server authority rules:
1. **State lives in components** (props or Rust components): saved, rolled back,
   replicable. The module's `state` and `self.locals` are neither; the dev guide says so,
   and the editor shows `self.locals` read-only while paused.
2. **Time is the game's:** `time.now()`, `dt`. The wall clock (`os.clock`, `os.time`) is
   for logs.
3. **Randomness is the world's** `random`, saved and rolled back.
4. **Effects are presentation:** sounds and particles started from a tick hook while a
   networked client re-simulates are dropped by the engine (they already played in the
   first prediction); `net.resimulating()` tells scripts the same.
5. **On a joined client,** tick hooks run for the predicted entities only (the client's own,
   ADR 0074), again on every re-simulation; the rest of the world comes from snapshots.
   Script components replicate when the game's schema names them, as plain data (a later
   step: `NetSchema` takes them by name).
6. **Saves** keep script components like Rust ones (ADR 0104): the save finds them through
   the script host's registry beside the program's reflected types. `after` timers are a
   component too.

### 9. Errors

- **A runtime error stops that instance:** its hooks are no longer called; its entity keeps
  its other components (it still draws, collides and sounds). A failing system stops.
  Errors include Luau errors, wrong types at the boundary (a string written into a number
  field), a view of a despawned entity, and the time budget (ADR 0032: 50 ms) running out.
- **Where it shows:**
  - the output (E6) with the message, `door.luau:14`, the hook, the entity's name and the
    stack; a click opens the script at the line (E8);
  - a mark on the entity in the scene tree, and on the component in the inspector:
    "Stopped: <message> (door.luau:14) — Resume";
  - the same error from many instances is one entry with a count.
- **Resuming:** saving the script (a reload) resumes every instance it stopped; Resume in
  the inspector resumes one.
- **A compile error** keeps the old code running (ADR 0032) and shows in the output and
  under the line in the code editor.
- **With the debugger** (E9), "pause on errors" stops the game at the failing line instead.
- **In a shipped game** the error goes to the log (and to crash reports' log tail, ADR
  0061), the instance stays stopped, and the game goes on.
- Until E6's output panel exists, errors go to the log only.

### 10. Hot reload

- **A saved script reloads in the running game** (the asset watcher, ADR 0107 E4-5) through
  `ScriptVm::load_module`: the module's `state` is kept, `on_reload(old)` runs, the next
  hook call goes to the new code. Target: ≤ 100 ms from save to running (CLAUDE.md §5).
- **Props are data, so they survive:** a prop added gets its default on every instance, a
  removed one is dropped (a scene file still holding it warns on load, as for Rust
  components), a changed default changes only new instances. A changed layout is a new
  version of the component: the host moves every instance's values over by name in one
  structural change.
- **Dependents reload too:** modules that `require` a reloaded one reload after it, in
  dependency order (Defold's stale module tables avoided).
- **A broken edit** keeps the old version running (ADR 0032).
- **Native code** is generated again for the reloaded functions; a function with a
  breakpoint runs interpreted (ADR 0107 §6).

### 11. Performance rules

- **The price of Luau** (ADR 0032, ref-1): native Luau 4.8× Rust on a numeric hot loop
  (boids: 3.25 ms against 0.68), the interpreter 30×. Script components are for event-driven
  and per-object logic; a loop over thousands of entities every tick goes to Rust.
- **A hook costs a call across the boundary,** per instance per tick: only hooks a script
  defines are called. Many alike objects take a system (one call per tick).
- **A component field is a call into Rust:** read it into a local once per hook when it is
  used several times.
- **Queries and physics filters are made once,** at the module's top, and iterated or used
  every tick without allocating.
- **The engine's API allocates nothing per call in steady state:** entities and vectors are
  values, views are cached, results come back as several values instead of tables. The
  zero-allocation test runs this ADR's examples for 600 ticks after warm-up.
- **A script's own allocations** (tables, strings, closures it makes) go through the
  engine's allocator (mlua uses Rust's global allocator): the dev build's statistics count
  them per script, and Tracy shows them. Luau's collector runs incrementally as scripts
  allocate; the player also gives it a bounded step at the end of the frame, sized by
  measurement, so collection never lands on one frame.
- **Tracy** has a zone per stage and per component's hook (`script Door.on_tick`); the
  editor's profiler (E6) shows each script's time and allocations.
- **The debugger costs only in the editor:** there hooks are entered as resumable threads
  (ADR 0107 §6); the shipped player calls them directly.
- **Later, if measured worth it:** Luau's code generator lets the host lower field access on
  its userdata to native code (`HostIrHooks`), which would make `t.translation` as cheap as
  a table field.

### 12. What the engine needs (the E7 steps)

1. **E7-1, the script host** (`engine_app`): one `ScriptVm`; `scripts/` loaded at start (a
   pack in a release); components from props through `World::register_dynamic`; the stages
   in the player's frame; entities, views, `world`, `input`, `time`, `print`; errors to the
   log. The measurements below, then this ADR's numbers filled in.
2. **E7-2, types:** `ofeliaengine scripts types`, the documentation file, `.luaurc` and VS
   Code settings from `ofeliaengine new`; the type test.
3. **E7-3, physics for scripts:** the player owns the scene's physics world when the game
   does not (`PlayerSettings::physics`); `Trigger`, a sensor shape on an entity set in the
   editor; `on_touch`, `on_untouch`, `physics.raycast`, `physics.overlap_sphere`.
4. **E7-4, the editor:** script components in the add menu and the inspector, "New Script…"
   from a template with the cursor in `on_tick`, reload on save, stopped instances marked
   with Resume.
5. **E7-5, the rest of v1:** `send`, `after`, `audio`, `random`, `world.instantiate`, saves
   of script components, the networking rules of §8.

Then E8 (the code editor) and E9 (the debugger), as ADR 0107 plans.

### 13. Measurements before acceptance (E7-1, ref-1, release)

| What | Target | If missed |
|---|---|---|
| an empty `on_tick`, per instance | ≤ 0.5 µs | batch the calls per component (one call carrying the instances) |
| a view's field read or write | ≤ 100 ns | generated accessors (§6), then `HostIrHooks` |
| 1,000 doors (example 1), one tick | ≤ 1 ms | as above |
| 10,000 spinners as a system, one tick | ≤ 2 ms | a bulk `buffer` column API for systems (ADR 0032) |
| allocations from the engine's API, the four examples, 600 ticks after warm-up | 0 | fix before merging |
| reload of a script used by 1,000 entities | ≤ 100 ms | move values lazily, on each instance's next hook |

## Consequences

- **Easier:** a game developer gives an object behaviour without Rust; the inspector, scene
  files, saves, undo, tweens and (later) replication handle script components like Rust
  ones; names and data are the same in every tool; errors stop one object, not the game;
  luau-lsp in VS Code and the built-in editor see the same types.
- **Harder:** the engine keeps a run-time component registry beside the program's static
  list, and every system that finds components by name (saves, scenes, the inspector, the
  network schema) consults both. Script components' layouts change at run time in the
  editor, which the world supports only through a structural change per instance.
- **Fixed by this decision** (hard to change once games exist): components with hooks, live
  views, snake_case names, hooks and messages instead of signals, `on_tick`/`on_frame`,
  `after` instead of `wait`, per-object state as declared props.
- **To revisit:**
  - Luau's `integer` type when it ships enabled (64-bit fields as numbers, not userdata);
  - `HostIrHooks` if views are hot in profiles;
  - parallel systems in separate VMs if a game needs more script time than one thread has;
  - scripts that run in the editor (tools) and in-game UI from scripts, each with its own
    ADR.

## Questions for the owner

Each question has lettered options and a recommendation. "You" is the game developer.
Questions 1 to 7 are decisions a to g above, with their options in the same order.

1. **How do scripts attach to the world?**
   - a) **Only as components on objects.** You select the door, "Add Component → Door",
     and write what one door does. Simple and familiar (Unity, Godot). A thousand coins
     cost a thousand calls a tick.
   - b) **Only as systems.** You write one script that loops over all doors. Fast for many
     objects, but even a single lever needs a loop and a tag.
   - c) **Both** (recommended): components for what one object does, which is most game
     logic; a system when there are many alike objects. Same files, same functions.
2. **How does a script change an object's data?**
   - a) **Directly** (recommended): `self.Transform.translation = p` moves the object at
     once, as in Roblox, Godot and Unity. Each such access costs a small call into the
     engine (about 100 ns, to be measured).
   - b) **Read a copy, change it, write it back:** `local t = get(e, Transform) … set(e,
     Transform, t)`. Explicit, but forgetting the `set` silently does nothing.
   - c) **By text paths:** `e:get("Transform.translation")`. Nothing to allocate, but no
     completion or type checking for the paths.
3. **How are names written?**
   - a) **As in our Rust and our scene files** (recommended): `Transform.translation`,
     `min_distance`, `world.find`, `on_tick`. The same name in code, scene files, the
     inspector and the docs.
   - b) **As in Roblox:** `Transform.Translation`, `MinDistance`, `world:Find`, `OnTick`.
     Familiar to Roblox developers, but every name then differs from the scene files and
     the Rust code.
4. **How does a script react to events and talk to other scripts?**
   - a) **By functions with fixed names** (recommended): you write
     `function Coin.on_touch(self, other)` and the engine calls it; to tell another object
     something, `door:send("open")` calls its `open`. Nothing to connect or disconnect;
     after a code change the new function is called.
   - b) **By signals, as in Roblox:** `coin.touched:connect(function(other) … end)`. Any
     number of listeners, but connections must be cleaned up, and after a code change the
     old function keeps running until reconnected.
   - c) **Both.**
5. **Fixed ticks or every frame?**
   - a) **Gameplay in `on_tick`, 64 times a second; the camera and visual-only things in
     `on_frame`** (recommended). As our Rust code does. Saves, replays and network play
     stay exact; you learn which of the two hooks to use.
   - b) **One `update(dt)` every frame,** as in Unity and Roblox, plus an optional fixed
     hook. Familiar, but gameplay written there depends on the frame rate, and saves and
     network play cannot be exact.
6. **How does a script wait?**
   - a) **With a timer that calls a function by name** (recommended):
     `self:after(3, "close")` calls the door's `close` in three seconds of game time. It is
     saved with the game and survives a code change. A sequence of steps is written as
     several functions.
   - b) **Also with `wait(3)` in the middle of a function,** as in Roblox (`task.wait`) and
     Unity's coroutines. A sequence reads top to bottom, but a save in the middle of a wait
     loses the rest of the sequence, network play cannot replay it, and after a code change
     the old code finishes the wait.
7. **Where does a script keep each object's data?**
   - a) **In declared properties** (recommended): you list them at the top of the script
     (`radius = 3.0`). They show in the inspector, are saved with the game, survive code
     changes and work with network play. Temporary data that should not be saved goes in
     `self.locals`.
   - b) **Anywhere on `self`,** as in Godot and Unity (`self.cooldown = 0` in any
     function). Quicker to start, but such data is not saved, not shown in the inspector,
     and the type checker cannot see it.

## As built (E7-1a, 2026-10-22): the script host

`engine_app::scripts::Scripts`, made by the player before any scene when the project has a
`scripts/` folder (`PlayerSettings::project`). The dev guide's page:
`docs/dev-guide/scripting.md`.

- **Loading:** every `.luau` under `scripts/`, in path order, is a module named by its path
  (`npc/guard`). The VM is `engine_script`'s. Its new `ScriptVm::new_with` installs the
  host's globals before the sandbox seals them, and `call_function` calls a component's
  hook under the time budget. A script that does not load is in the log, and the rest
  load.
- **`component(name, props)`** gathers the declaration while its module runs. Once the module
  ran, the host:
  - lays the props out by name;
  - builds their `TypeInfo` at run time (leaked: one per component);
  - registers the component (`World::register_dynamic`, named `script::<Name>`, plain data);
  - finds its hooks on the component's table.

  Props of this step:
  - numbers (`f32`), `prop.integer` (`i32`), booleans, vectors (`Vec3`), `prop.entity()`;
  - `prop.number`'s `min`, `max`, `step`, `tooltip`, `hidden` and `skip` go into the field's
    `Attrs`, so a write is clamped as the inspector clamps.

  Strings wait for a component layout that drops them (their own step).
- **Stages:**
  - in every fixed tick, after the timers advance and before `Game::tick`: instances that
    came are started (`on_start`), then every `on_tick`;
  - in every frame, before `Game::present`: `on_frame`;
  - both only while the world simulates (a step asked for included).

  The instances are kept in their entities' order (index, then component), so the order
  is the spawn order, the same on every machine. The scene tree's order (§7) waits for
  scenes to hold script components.
- **`self`** is a userdata made once per instance:
  - its props by lowercase name;
  - its entity's components by PascalCase name, as views whose fields read and write the
    world through reflection (`Vec3`, `Vec3A` and `Vec2` as vectors, `Quat` as a `quat`,
    entities, enums by variant name, strings);
  - `self.entity` and `self.locals` (a user value, never saved).

  `on_stop` runs once the component went, so its props are gone.
- **The world** is open to the scripts through a thread-local pointer for the length of a
  stage.
- **Libraries of this step:**
  - `world.find`, through a name index made at a stage's first call;
  - `time.now()`, `time.tick()`, `time.dt()` (functions, as the sandbox seals tables);
  - `quat.identity`, `quat.axis_angle`, `quat.euler`; `*` on quats and vectors;
  - `print` and `warn` to the log with `scripts/<file>.luau:<line>`.
- **Errors** stop the instance. They go to the log once each, as `scripts/door.luau:4: too
  fast (Door.on_tick)`, which the editor's Output turns into a click to the line.
- **Measured** (§13; `engine_app` `scripts::tests::measure_hooks_and_views`, ref-1, release,
  medians of 51 ticks):

| What | Target | Now |
|---|---|---|
| an empty `on_tick`, per instance | ≤ 0.5 µs | **0.19 µs** |
| a prop's read and write (`self.a = self.a + 1`) | ≤ 100 ns each | 354 ns the pair (about 177 each) |
| a component field's read and write (`t.translation = t.translation + v`) | ≤ 100 ns each | 299 ns the pair (about 150 each) |
| 1,000 doors (example 1), one tick | ≤ 1 ms | 3.3 ms |

  The hooks meet their target; the views do not. Most of a view's access is mlua's
  dispatch of a metamethod: references made for the userdata and the key, the arguments
  converted. Two changes moved it a little: the world pointer in a thread-local instead
  of the VM's app data (391 → 354 ns), and the name index (doors 3.55 → 3.29 ms). §13's
  plan for this miss stands, as **E7-1c**:
  - views as Luau tagged userdata with their own metatables;
  - field names interned as Luau atoms (`useratom`), so an access is a switch on an atom
    and an offset;
  - `HostIrHooks` after that if needed.
- **Not in this step:**
  - systems;
  - script components in scenes and saves (E7-1b);
  - `input`, `world.spawn`, `world.query`, entity methods;
  - the zero-allocation test (views and entities are still made per access);
  - reloading;
  - the types file (E7-2);
  - the editor (E7-4).

## As built (E7-1b, 2026-10-22): script components in scene files

- **`engine_ecs::RuntimeType`:** a component type made while the program runs. It holds
  its `TypeInfo`, its registry name (`script::Door`), its layout and its default bytes.
  - `RuntimeType::id(world)` finds it in a world by that name, or registers it there
    (`None` when another layout holds the name).
  - `add_runtime_type` adds it to a process-wide list beside the program's
    `reflected_types`, replacing one of its stable name; `runtime_types` reads the list.
  - The script host puts each component it declares there, and registers it in its own
    world through it.
  - `Components::id_by_name` finds a component by its registry name.
- **`SceneRegistry::from_program`** now takes those types too (`Access::Dynamic`):
  - loading starts from the default bytes, fills the lines, and inserts the value
    (`Door.lift = 3`);
  - saving reads it through its id;
  - an override removes it;
  - in a world that has not seen it, the first use registers it.

  The player and every scene path of the editor build their registries this way, so a
  scene with script components keeps them through the editor's loads, saves, tabs,
  prefabs and undo. Undo records them as plain data.
- **Order:** the scripts load before the player loads its first scene, so the scene's
  `Door.*` lines find their component.
- **Saves** (ADR 0104) and the network schema read their own registries: script components
  join them in E7-5.

## As built (E7-4a, 2026-10-22): scripts in the editor

The owner's request (2026-10-22): attach a script to any entity as in Godot, open it from
there, and see the script's file in the inspector.

- **Add Component…** (Ctrl+Shift+A, or the button under the inspector's components, Unity's
  place) lists the components a selected entity lacks, by name:
  - the program's reflected components that have a default;
  - the scripts' (`engine_ecs::runtime_types`), each with its file;
  - New Script… last.

  A pick gives each selected entity that lacks it the component at its default, with what
  it requires, as one undo step. `SceneRegistry::insert_default(world, entity, name)` does
  what a scene's `Name = {}` line does, for each way a type reaches a registry: its Rust
  type, the program's list, or made at run time.
- **The × on a component's header** takes it from the selected entities, as one step. A
  transform has none, since it places the entity (Unity's rule).
- **Attach Script…** (the scene dock's script button, Godot's place) lists New Script…
  first, then the scripts' components.
- **New Script…** writes `scripts/<snake>.luau` from a template: `--!strict`, the component
  without props, `on_start` and `on_tick`.
  - It is named after the active entity: `Front door` gives `FrontDoor` in
    `scripts/front_door.luau`, with a number added when a component or a file has the name.
  - The player's host loads it at its next frame (`Player::load_script`,
    `Scripts::load_file`), as a module of its own, refused when loaded already.
  - The editor then gives the component to the selection as one step, and opens the file
    with the caret in `on_tick`. Undo takes the component off; the file stays.
- **Where the file shows:**
  - `RuntimeType::source` holds a script component's file;
  - the editor keeps a table from component id to file (`runtime_types_version` tells it is
    stale);
  - the inspector shows the file under the component's header, and the tree a script's mark
    on its entity's row;
  - a click on either opens the script (an entity with several offers them in the palette).
- **A component without fields** (a tag, a script's without props) shows in the inspector
  by its name, so it can be seen and taken off.
- **Script props' defaults** come from the type's default bytes: a prop away from its
  default is marked changed, and reset puts the default back.
- **Tested with the real player** (`engine_editor/tests/scripts.rs`): New Script loaded and
  attached; a project's script attached from the list turns its entity in Play, and Stop
  puts the entity back.
- **Not yet:**
  - a saved script takes effect when the editor starts again; reloading on save is E7-4b;
  - the editor's Luau analysis does not know the engine's API, so `component` reads as an
    unknown global until the types file (E7-2).

## As built (E7-4b, 2026-10-22): a script reloads when it is saved

- **When:** Ctrl+S on a game's script (`scripts/…`) runs it again at the player's next
  frame. A file changed by another editor runs again when the file watcher's scan finds it
  written anew. The host keeps a hash of each module's text, so a text that already ran (the
  watcher's turn after Ctrl+S) does nothing.
- **How** (`Scripts::reload_file`, `Player::reload_script`):
  - the module runs again in its VM, keeping its `state` table; a module that does not
    compile or run leaves the old version running, and the error names the file and line
    (in the Output, a click opens it);
  - a component whose props kept their places (the same names at the same offsets, of the
    same types) takes the new hooks in place; its new attributes (ranges, tooltips) replace
    the description the world holds (`World::set_type_info`, for components made at run
    time);
  - a component whose props changed moves to a component of the new layout, registered
    under the next name (`script::Door#2`; names stay unique, and a world cannot drop a
    component). `engine_ecs::Relayout` describes the move: by prop name, a prop of the same
    type keeps its value, the others take the new defaults. Every entity's value moves at
    once; scene files are unchanged (they name props);
  - instances go on with their `self.locals`; `on_start` does not run again;
  - stopped instances of the reloaded components start again (the error may be fixed),
    and their errors are reported again when they come back;
  - a component the script no longer declares keeps its data and loses its hooks.
- **The editor follows a move:** `History::relayout` rewrites every record of the old
  component, done and undone, and Play's snapshot and mark do the same. Undo then puts back
  values of the layout the world holds now, entity references kept by scene id.
- **Measured** (ref-1, release, `scripts::tests::measure_reload --ignored`): a script used by
  1,000 entities reloads in 0.21 ms when its props keep their places, and in 0.33 ms when they
  change (§13's target: 100 ms). No lazy move is needed.
- **Not yet:** the Resume mark on an instance stopped by an error (a reload starts it
  again). A component's props change their kinds only by name: a prop that changes type
  starts from its default. Each reload keeps its old type descriptions for the process's
  life (a few hundred bytes); the editor reloads, a game does not.

## As built (E7-2, 2026-10-22): the engine's types

- **`engine_app::script_types::definitions()`** writes the API in Luau 0.736's definition
  syntax:
  - `Quat` (an extern type whose `__mul` composes rotations and turns vectors),
    `PropOptions`;
  - every reflected component of the program (the engine's and the game's Rust ones) and
    every script component, as an extern type of the fields scripts can read: numbers,
    booleans, `vector` (a `Vec3`, `Vec3A` or `Vec2`), `Quat`, `Entity?`, strings, an enum's
    variants as a union of their names, structs of those. Lists, maps, options and asset
    references are left out, as are fields named by a reserved word (`Timer.repeat`: a
    script reads it as `timer["repeat"]`);
  - `Entity` (`name`, `alive`, its components by name), `ScriptSelf` (`entity`, `locals`,
    the components), `ScriptComponent<P>` with the hooks typed `self: P & ScriptSelf`;
  - `component<P>`, `prop`, `world`, `time`, `quat` and `warn`.
- **The editor** makes them again whenever the run-time types change (a script declared
  or reloaded). It writes `types/ofelia.d.luau` when the project has `scripts/`, and
  gives them to its Luau checker: findings, completion and hovers see the engine's API.
- **The checker uses Luau's old solver** (it used the new one, E8-3b). The new solver of
  0.736 does not type an un-annotated hook's `self` from `component`'s declaration; the
  old one does, and it is luau-lsp's default too (checked with luau-lsp 1.70.1,
  2026-10-22).
- **`ofeliaengine new`** writes `.vscode/settings.json` for luau-lsp: the standard
  platform (not Roblox's), no Rojo sourcemap, the definitions under `@ofelia`, the old
  solver. `/types/` is in the game's `.gitignore`: the editor writes it again.
- **Tests:** the guide's door and the New Script template check clean against the
  definitions; a field given a value of the wrong type is an error on its line; the
  definitions load; a component is the type of its readable fields, a reserved word left
  out; a new game has the settings.
- **Not done:**
  - `ofeliaengine scripts types`: only a build with the game's code knows its Rust
    components, so the editor writes the file;
  - the documentation file (`types/ofelia.docs.json`, luau-lsp's `documentationFiles`):
    its format is to be read from luau-lsp's sources first;
  - `.luaurc`: each script says `--!strict` itself, as the template writes it.

## As built (E7-1c, 2026-10-22): the views' fast path

- **`engine_app::script_views`**, on Luau's own API through mlua's `ffi`:
  - **atoms:** Luau's `useratom` callback gives every name-like string the next number
    from the thread's table, and each struct's fields are found by their names' atoms
    (`Slot`); a name made before the callback was set is matched by its bytes;
  - **tags:** `self` (64), views (65) and entities (66) are userdata of tags of their own,
    made with their tag's metatable (`lua_setuserdatametatable`), whose metamethods are C
    functions. They read the userdata, the key's atom and the stage's world directly;
  - numbers, booleans and vectors are read and written there; entities answer `name`,
    `alive`, their components, `==` and `tostring`; `world.find` is a C function too.
    Rotations, entity props, strings, enums and nested structs written whole go to the
    host's own functions, called from the C functions;
  - a view of `self`'s entity's component (up to four), `self.locals` and `self.entity`
    are made once and kept on `self`; the references go with the instance, and the views
    with a reload;
  - errors are raised at the calling script's line (`luaL_where`), once nothing with a
    destructor is alive in the frame.
- **The time budget** (`engine_script`): Luau's interrupt is a C function now, not mlua's
  closure. It counts interrupts and reads the clock every 64 (`CHECK_EVERY`).
- **Measured** (`scripts::tests::measure_hooks_and_views`, ref-1, release, medians of 51
  ticks, three runs):

  | What | Target | E7-1a | Now |
  |---|---|---|---|
  | an empty `on_tick`, per instance | ≤ 0.5 µs | 0.19 µs | **0.17 µs** |
  | a prop's read and write (`self.a = self.a + 1`) | ≤ 100 ns each | 354–386 ns the pair | **71–73 ns the pair** |
  | a component field's read and write (`t.translation = t.translation + v`) | ≤ 100 ns each | 296–299 ns the pair | **75–76 ns the pair** |
  | 1,000 doors (example 1), one tick | ≤ 1 ms | 3.3–3.4 ms | **0.65 ms** |

  §13's targets are met. `HostIrHooks` is not needed.
- **Found:** in the `shipping` profile (`panic = "abort"`), an error raised through a
  Rust callback aborts the game, because Luau's errors are C++ exceptions and
  unwinding into a `C-unwind` Rust frame aborts under that profile. A probe built that
  way stopped at the first binding error ("panic in a function that cannot unwind").
  [ADR 0118](0118-shipping-panic-strategy.md) proposes the fix.
- **Tests:** the earlier ones, plus entities on the fast path (`world.find`, `==`,
  `name`, `alive`, `tostring`, a component through an entity, an entity prop written) and
  the fast path's errors at their lines.
- **Not in this step:** an entity from `world.find` and a view through another entity
  are still made at each access (a garbage-collected block each); the zero-allocation
  test.

## As built (E7-3a, 2026-10-23): physics for scripts, queries

- **Which world scripts query:** the game's, through a new hook, `Game::physics(&self) ->
  Option<&ScenePhysics>`; else the player's own. The player makes its own when
  `PlayerSettings::physics` gives settings:
  - it loads the pack's bodies into it;
  - it keeps the scene's models and brushes in step before the ticks (`sync_scene`);
  - it steps the world after each tick's `Game::tick` and interpolates in the presentation;
  - sound travels through it when the game gives no acoustics.

  `Player::physics` and `physics_mut` reach it. The editor's Play forwards `Game::physics`.
- **Bodies name their entities:** `engine_physics::ecs::entity_user_data(e)` (the
  entity's bits plus one, so 0 names none) in a body's user data, read back by
  `user_data_entity`. `ScenePhysics` gives it to models' bodies, brush groups' and a
  pack's moving bodies; `ScenePhysics::entity_of(body)` reads it. A game's own bodies
  carry it the same way. The backends gained `user_data(body)` and
  `set_user_data(body, data)`.
- **The library** (`physics`), through mlua: a query costs microseconds, so the fast path
  is not needed here.
  - `raycast(origin, direction, max_distance, filter?)` returns the hit's entity (nil for
    a body without one), point, normal and distance, or nothing on a miss;
  - `overlap_sphere(center, radius, filter?)` is an iterator over the entities whose bodies
    overlap the sphere, each once;
  - `filter({ "Moving", … })` names collision layers. It is made once at a module's top,
    and resolved against the world's `PhysicsSettings::layer_names` the first time it is
    used there.

  Without a physics world, a query is an error that says where one comes from. The types
  file declares them (`PhysicsFilter`).
- **Tests:** scripts' queries against a world of named bodies (an entity, point, normal and
  distance, an overlap, a filter by layer name, a miss, no world); in the player on both
  GPUs, its own world drops a prop onto a floor for a game without physics, and a script's
  ray sees the prop; a body's user data read and written in Jolt; the types file checks a
  script that queries.
- **Next (E7-3b):** `Trigger`, a sensor shape on an entity, and `on_touch` / `on_untouch`
  from the last step's contacts.

## As built (E7-3b, 2026-10-23): triggers and contacts

- **`Trigger`** (`engine_app`, a reflected component): `shape` (a box, a sphere, a capsule
  standing on y) and `size` (metres). It stands around its entity's origin, placed, turned
  and scaled with it, and pushes nothing. Scenes save it, the inspector edits it, Add
  Component lists it.
- **`ScenePhysics::sync_triggers`** (part of `sync_scene`), as `sync_models` works:
  - a kinematic sensor body per active entity with a trigger, on the `Moving` layer, its
    user data the entity;
  - its shape made for the entity's scale;
  - moved (`set_transform`) when the entity moves, made again when the trigger or the
    scale changes, gone with the component, the entity, or when it is turned off;
  - it visits only what changed (change ticks, the removal log of `Trigger`, a rescan on
    the activity and resync epochs).
- **Contacts:** at the start of a tick's script stage the host hears the last physics
  step's contact events, once per step (`ScenePhysics::steps`). For each side that names an
  entity, every script instance there with `on_touch` or `on_untouch` is called:
  - `on_touch(self, other, point, normal)`, the normal pointing from the instance's entity
    toward the other;
  - `on_untouch(self, other)`;
  - `other` is nil for a body without an entity;
  - an instance not yet started is started first.

  The types file declares both hooks.
- **The editor** draws triggers with the colliders (View: Colliders, Alt+C), in the
  sensors' yellow, as their bodies are.
- **Tests:** a ball dropped through a 2 m trigger is heard once coming in, with its
  entity and a point near the trigger, and once going out; the trigger's body goes with
  the component. The editor draws a spherical trigger (three circles) in the sensors'
  batch on both GPUs.
- **Not in this step:** contacts in the joined client's re-simulated ticks (networked
  prediction, §8); a trigger's own collision layer.

## As built (E7-1d, 2026-10-23): entities as values, nothing allocated

- **Entities are values** (§3): light userdata of tag 0 holding the handle's bits plus one
  (`script_views::entity_value`), no longer userdata blocks:
  - never allocated; `==`, `rawequal` and table keys go by the handle, so two finds of one
    entity are one key; `typeof` says `Entity` (`lua_setlightuserdataname`), `type` says
    `userdata`;
  - tag 0, because mlua pushes light userdata back with tag 0: an entity that passes
    through the host's own functions (a contact's `other`, a ray's hit, an entity prop)
    stays itself;
  - their metatable is the VM's one for every light userdata (`__index`: `name`, `alive`,
    components; `__tostring`); no other light userdata reaches scripts;
  - entity props and fields are read and written on the fast path.
- **Views through entities are kept:** `e.Transform`, and a struct inside a view, is
  looked up in a table of 256 places by its entity, type, component and offset. The view
  there is given again; otherwise a new one takes the place. Views never change, so one
  given twice is the same (`rawequal(e.Transform, e.Transform)`).
- **`world.find`'s index** follows the names that came or changed (`Changed<Name>`, at a
  stage's first find) instead of being made again every stage, which made a string per
  named entity. It is keyed by the name's hash and checked against the found entity's own
  name; a first one renamed or gone, or two names of one hash, make it again.
- **The host allocates nothing in a stage:**
  - the hooks' module name is shared (`Rc<str>`; it was a `String` copied at every call);
  - the instances' order and the components hearing contacts are scratch lists;
  - the clock is set in place (mlua's `set_app_data` boxes its value anew).
- **The slow path is handed the field's place:** a field the fast path found but does not
  read or write (rotations, enums, strings) goes to the host's function with its place
  among the struct's fields after the arguments, so the key's text is not read through
  mlua. `LuaString::to_str` clones the string's reference, and the first clone of one
  allocates its shared count (16 bytes, mlua 0.12.1).
- **`physics.raycast`** returns four values, nils on a miss, instead of a list.
- **Measured** (ref-1, release; `scripts::tests::measure_hooks_and_views`, medians of 51
  ticks, three runs each, main and this branch one after the other):

  | What | Main | Now |
  |---|---|---|
  | an empty `on_tick`, per instance | 0.164–0.166 µs | **0.138–0.139 µs** |
  | a prop's read and write | 72–75 ns the pair | 72–74 ns |
  | a component field's read and write | 71–75 ns the pair | 76.5–77 ns |
  | 1,000 doors (example 1), one tick | 0.63 ms | **0.58–0.61 ms** |

  A field's pair is 3–5 ns slower. With entity fields back on the slow path it is the
  same, so the new code on its path is not the cause; most likely the code's layout.
  A `Slot` grown by its place (56 bytes) made it 87 ns, so the place is found when
  handing over instead.
- **Allocations** (§13; `engine_app/tests/zero_alloc_scripts.rs`: the door example on
  1,000 entities and ten watchers using `world.find`, entities as table keys, an entity
  prop, `self.entity`, `self.locals`, `time`, a ray and a trigger's contacts; 600 ticks and
  frames after 240): **1,237,831 → 0**. The other three examples need E7-5's parts
  (`has`, `despawn`, `audio`, `world.query`, systems, `input`, `Transform`'s methods)
  and join the test with them.
- **What still allocates, by design:** a `quat` made or read (a userdata), `tostring`, a
  `physics.overlap_sphere` loop (its iterator), an enum field written by name (its text
  read through mlua).
- **Tests:** entities as values and keys (`==`, `rawequal`, a set of entities, `typeof`,
  `type`); the kept views over 300 entities (more than the table's places); the name index
  through a second of a name, the first despawned, a rename and a newcomer; the errors
  naming `Entity`.

## As built (E7-5a, 2026-10-23): entities' methods, components as values

- **Methods are `__namecall`s** on the fast path's metatables (entities' light userdata,
  `self`, views), found by the method's atom:
  - an entity's: `has(C)`, `add(C, values?)`, `remove(C)`, `despawn()`, `children()`,
    `find_child(name)`, and the field `parent`;
  - `self` has its entity's methods, then the functions on its component's table:
    `self:open()` calls `Door.open(self)` (the entity's names come first);
  - `Transform`'s on its views: `rotate(axis, angle)` (in the parent's space, as Godot's
    `rotate`), `look_at(point, up?)` (its forward, −z, toward a point in the parent's
    space; unchanged when there is nowhere to look), `forward()`, `right()`, `up()`.
- **Components as values:** each of the program's components is a global table of its
  name (`Transform`, `Trigger`), which the sandbox seals; a script's `component` table is
  one by its `name`; a name works too (`e:has("Wallet")`, for another script's component
  until `require` comes). `typeof` says `table`: Luau honours `__type` for userdata only.
- **What each does:**
  - `has`: whether the entity has it (false for one that is gone);
  - `add`: the default (`SceneRegistry::insert_default`, by scene files' names) unless it
    has one, then `values` written field by field, at once; a script component's
    instance starts at the next stage;
  - `remove`: at once;
  - `add` and `remove` refuse `Parent` and `Children`, which keep the hierarchy;
  - `despawn`: at the end of the stage (§7), with its children. Their instances'
    `on_stop` runs first, the props still there, then `despawn_recursive`; despawns asked
    in those `on_stop`s go too, up to 16 passes;
  - `children`: a loop that keeps nothing. Each step goes from the child before
    (`Parent::next_sibling`, new), so it allocates nothing; a child that leaves ends it;
  - `find_child`: the nearest descendant of the name, breadth first (a child before a
    grandchild), its queue a scratch list.
- **Errors** name what went wrong: a method without its colon (`e.has`), a method that is
  none (`self:fly()`: neither an entity's nor a function of the component), a component
  that is none (`has: no component is called X`), `Transform has no method spin`.
- **`alive` stays a field** (as built in E7-1c), not §3's method.
- **Types:**
  - `Entity` has `parent` and the methods, `self` (`ScriptSelf`) the methods, `Transform`
    its methods;
  - `type ComponentType = { name: string }`, and a `declare` for each of the program's
    components;
  - luau-lsp does not see a component's own functions on `self`: `self:open()` is flagged
    in strict mode, and `Door.open(self)` keeps the types. An indexer on `self` would
    lose the check of props' names.
- **Measured:** nothing allocated. The zero-allocation test's walkers use `rotate`,
  `forward`, `right`, `up`, `look_at`, `find_child`, `children`, `has`, `parent` and a
  function of their own every tick: 0 in 600 ticks. The views' measures are unchanged
  within their spread (main and this branch one after the other, three runs each):

  | What | Main | This branch |
  |---|---|---|
  | a prop's pair | 72–75 ns | 76–78 ns |
  | a field's pair | 86–91 ns | 82–84 ns |
  | 1,000 doors | 0.59–0.61 ms | 0.59–0.60 ms |

  A field's pair measured 76.5–77 ns on main's code an hour earlier: the layout of a build
  moves these by about ten nanoseconds.
- **Tests:**
  - every method and `parent`, `add` with values read on the next line, `remove`, an own
    function through `self:`;
  - `Transform`'s methods turned and read back;
  - a despawn at the stage's end with its child, its `on_stop` seeing its props and the
    tick's write;
  - the four errors;
  - the editor's checker takes a strict script using them all.
- **Not in this step:** `require` (next: the project's modules by path, `.luaurc` aliases,
  dependents reloaded after them); `send`.

## As built (E7-5b, 2026-10-23): `require` between scripts

- **`require(path)` is Luau's require by string** over the project's modules. It is
  mlua's `create_require_function` with the host's own `Require`
  (`engine_app::script_require`): a tree of the modules under `scripts/`, named by their
  paths (`npc/guard`). Luau's own navigation resolves `./`, `../`, `@self` and the aliases
  of the `.luaurc` files above the requiring script, read from the project's folder.
- **It replaces the VM's default,** mlua's file system requirer. That one never ran for
  these scripts: their chunks are named `=module`, and it takes `@` names only.
- **What it returns** is what the module returned (a component's table, a table of
  functions), cached per module by mlua's `require`.
- **Load order:** what a module requires runs before it.
  - The host reads every script and finds the paths of its `require` calls written as
    strings (comments and other strings skipped).
  - It resolves them as the navigation does and loads in that order, the rest in path
    order.
  - Scripts that require each other are reported: one runs before what it requires, whose
    `require` fails naming it ("has not run yet").
- **Reloads** (§10): a script run again drops its cached result. The scripts that require
  it, and those that require them, run again after it in their order, from the text that
  last ran. The editor names them ("ran again, as they require it: …").
- **An `init.luau`** is its folder's module (`require("./npc")`), as Luau has it: its `./`
  is beside the folder, and `@self` is the folder.
- **Tests:**
  - a script before its requirement in path order;
  - `../`, an `init` with `@self`, an alias from the project's `.luaurc`;
  - two scripts requiring each other;
  - a reload whose dependent runs again and sees the new result;
  - the path scanner, the navigation and the order on their own;
  - the editor's message.
- **Not in this step:**
  - a path made at run time, which the scan cannot see: its `require` works only once its
    module has run, and errors before;
  - comments in `.luaurc` files: the order's scan reads their aliases as JSON, though
    Luau's own reader takes comments.

## As built (E7-5c, 2026-10-23): queries, systems, spawning

- **`world.query(A, B, …)`** makes a query, a userdata of its own tag (67) holding its
  components by name. The names are found at its first loop, since a script's own
  component is registered once its module ran. `q:with(C)` and `q:without(D)` filter and
  return the query.
- **`for e, a, b in q do`** is Luau's generalized iteration (`__iter`):
  - the loop's start takes a snapshot of the entities whose archetypes match;
  - each step goes from the entity before (the query remembers where it is, so a step
    costs nothing to find) and passes over an entity that went or lost a column since;
  - `a`, `b` are the query's own views, one a column, moved to each entity in turn:
    nothing is made per step, and a view kept past its step shows the entity of the
    step after. The guide says to keep the entity instead.
- **A query collected** frees its box in its userdata's destructor
  (`lua_setuserdatadtor`). Its views' registry references wait in a list, by VM, until
  the host's next stage lets go of them: a destructor runs inside Luau's collector,
  where the registry is not written.
- **`system(name, { on_tick, on_frame })`** declares a system at a script's top. Its hooks
  run after the components' instances, in the order declared (§7). An error stops the
  system, reported once ("(Spin.on_tick)"); running its script again starts it again,
  and one the script no longer declares goes.
- **`world.spawn({ Component = { field = value } })`** makes an entity, at once:
  - each component's default comes from `SceneRegistry::insert_default` (a `Transform`
    brings its `GlobalTransform`), then the fields given are written;
  - a component of one field takes its value as it is (`Name = "Bullet"`);
  - a script component's hooks start at the next stage;
  - an error despawns what was half made.
- **Measured** (ref-1, release, `scripts::tests::measure_hooks_and_views`, medians of 51
  ticks, two runs): 10,000 spinners as a system (example 3) take **1.20–1.21 ms** a tick
  (target 2 ms); the same as a hook each takes 2.64–2.68 ms. The zero-allocation test
  runs example 3 as written over 1,000 entities: 0 allocations in 600 ticks.
- **Tests:**
  - a system turning what a query gives, and a filtered query in a frame's system;
  - a spawned entity with its components at once, which a loop that began after it
    visits;
  - a broken system stopped and reported once;
  - queries made and dropped in a hook: collected, their views let go of at the next
    stage;
  - the editor's checker takes the spin example and `spawn` in strict mode.
- **Not in this step:** a query over an entity's children; the scene tree's order for
  queries (they visit archetypes in their order, the same on every machine for the same
  world).
