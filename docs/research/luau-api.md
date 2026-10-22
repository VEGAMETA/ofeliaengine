# The Luau scripting API: how engines expose themselves to scripts

Checked: 2026-10-22. Roblox, Defold, Godot and Unity were studied as products, from their
documentation; Godot (MIT) was read for ideas only. Luau and mlua facts were read in the
sources we build (`luau0-src` 0.21.0+luau736, `mlua` 0.12.1). No code was carried over: the
snippets below are ours, written for the comparison. Input for
[ADR 0114](../adr/0114-luau-api.md).

CLAUDE.md §10 fixes the language and the tooling (Luau through mlua, sandboxed; bindings
generated from Rust with `.d.luau` types for luau-lsp; hot reload with state kept; a DAP
debugger; script errors never crash the game). [ADR 0032](../adr/0032-scripting.md) decided
the VM, modules, limits and the types at the boundary. This note is about the API a game
developer writes against.

## Sources

| What | Version, date | License | Read |
|---|---|---|---|
| Roblox Creator Hub | docs as of 2026-10-22 | proprietary docs | [Events](https://create.roblox.com/docs/scripting/events), [Script locations](https://create.roblox.com/docs/scripting/locations), [RunService](https://create.roblox.com/docs/reference/engine/classes/RunService), [CollectionService](https://create.roblox.com/docs/reference/engine/classes/CollectionService), [Instance](https://create.roblox.com/docs/reference/engine/classes/Instance), [Server authority model](https://create.roblox.com/docs/projects/server-authority) |
| Defold manuals | docs as of 2026-10-22 (mention 1.13.2) | Defold License (engine) | [Script](https://defold.com/manuals/script/), [Script properties](https://defold.com/manuals/script-properties/), [Message passing](https://defold.com/manuals/message-passing/), [Hot reload](https://defold.com/manuals/hot-reload/), [Writing code](https://defold.com/manuals/writing-code) |
| Godot docs | stable (4.x), 2026-10-22 | MIT (engine), CC BY 3.0 (docs) | [Overridable functions](https://docs.godotengine.org/en/stable/tutorials/scripting/overridable_functions.html), [Signals](https://docs.godotengine.org/en/stable/getting_started/step_by_step/signals.html), [Nodes and scene instances](https://docs.godotengine.org/en/stable/tutorials/scripting/nodes_and_scene_instances.html), [Groups](https://docs.godotengine.org/en/stable/tutorials/scripting/groups.html), [Debugging tools (4.2)](https://docs.godotengine.org/en/4.2/tutorials/scripting/debug/overview_of_debugging_tools.html) |
| Unity Manual and Scripting API | current docs, 2026-10-22 | proprietary | [Execution order](https://docs.unity3d.com/Manual/execution-order.html), [Domain reloading](https://docs.unity3d.com/Manual/domain-reloading.html), [Object.Destroy](https://docs.unity3d.com/ScriptReference/Object.Destroy.html), [SerializeField](https://docs.unity3d.com/ScriptReference/SerializeField.html) |
| luau-lsp | README, 2026-10-22 | MIT | [repository](https://github.com/JohnnyMorganz/luau-lsp), [editors/README.md](https://github.com/JohnnyMorganz/luau-lsp/blob/main/editors/README.md) |
| Luau | 0.736 (`luau0-src` 0.21.0+luau736) | MIT | `Ast/src/Parser.cpp`, `VM/include/lua.h`, `VM/src/linit.cpp`, `VM/src/lintlib.cpp`, `VM/src/lmathlib.cpp`, `VM/src/lveclib.cpp`, `CodeGen/include/Luau/CodeGenOptions.h` |
| mlua | 0.12.1 | MIT | `src/memory.rs`, `src/luau/require.rs`, `src/state.rs` |

Rows marked * in the tables below are from general knowledge of the products, not
re-checked in their documentation on this date.

## How the four expose the engine

| Engine | Model | What works | What annoys |
|---|---|---|---|
| **Roblox** (Luau) | Every object is an `Instance` with properties, methods and events. A `Script` runs where it sits (`RunContext`); shared code lives in `ModuleScript`s loaded with `require` | The language and a typed API come together: luau-lsp ships Roblox's definitions (by API security level). Events are one line (`part.Touched:Connect(f)`). Under the server authority model, functions bound with `RunService:BindToSimulation` are re-run on rollback, and only members marked for simulation access may be used in them | Scripts copied into objects multiply: the docs now say to avoid scripts in Workspace objects and to tag instances and drive them from one module through `CollectionService`. Connections must be disconnected. Deferred and immediate events behave differently. `Touched` fires per touching part*. PascalCase everywhere, unlike Luau's own library (`string.split`, `table.insert`) |
| **Defold** (Lua) | A script is a component of a game object, with lifecycle functions (`init`, `fixed_update`, `update`, `late_update`, `on_message`, `on_input`, `on_reload`, `final`). Objects talk by `msg.post` | A small, regular, snake_case API (`go.get`, `go.set`, `go.animate`, `factory.create`, `go.delete`). Properties declared at the top (`go.property`) appear in the editor with per-instance overrides (shown blue, with a reset). Messages are queued and delivered at known points. Hot reload re-runs the file and keeps `self` | Messages are identified by hashed strings and add indirection. A message table is capped at 2 KB, and long chains of messages may not finish within a frame. The editor's language server does not know the engine's types: annotation files come from community projects. A reloaded module's old table stays in the scripts that required it |
| **Godot** (GDScript) | One script per node, extending the node's class. Virtual functions (`_ready`, `_process`, `_physics_process`, `_input`), signals, `@export` properties | `_process` and `_physics_process` make the frame/fixed split explicit. Signals connect in the editor or in code. Groups give systems over tagged nodes (`get_nodes_in_group`, `call_group`). `queue_free` waits until the end of the frame. Saved scripts reload in the running game (Sync Script Changes) | One script per node: behaviours compose by adding child nodes. Signal connections made in the editor live in the scene, and the generated callbacks may not work with external editors. Node paths (`$Area3D`) break when the tree changes. GDScript is slow: typed, 12× slower than native Luau on ADR 0032's boids |
| **Unity** (C#) | `MonoBehaviour` components with event functions (`Awake`, `Start`, `FixedUpdate`, `Update`, `LateUpdate`, `OnTriggerEnter`, `OnDestroy`); serialized fields in the Inspector | `GetComponent<T>()`, `Instantiate` and `Destroy` are learned in a day. Serialized fields are typed, shown and saved with the scene. Trigger and collision callbacks run after the physics step. `Destroy` waits until after the current Update loop, before rendering | No order between instances of the same script without the Script Execution Order setting. Gameplay written in `Update` depends on the frame rate*. A code change reloads the domain: Unity serializes the script components, reloads the assemblies and recreates them, so unserialized state is lost, and the docs call it time-consuming. Coroutines are not saved* |

### By topic

| Topic | Roblox | Defold | Godot | Unity |
|---|---|---|---|---|
| Attaching | a script inside an instance, or one module over tagged instances (`CollectionService`) | a script component on a game object | one script per node | any number of `MonoBehaviour`s per GameObject |
| Lifecycle | the script runs top to bottom, then events; frame events (`PreSimulation`, `PostSimulation`, `PreRender`, `BindToRenderStep`); `BindToSimulation` under server authority | `init`, `fixed_update` (0 to N times a frame), `update`, `late_update`, `final` | `_enter_tree`, `_ready`, `_process`, `_physics_process`, `_exit_tree` | `Awake`, `OnEnable`, `Start`, `FixedUpdate`, `Update`, `LateUpdate`, `OnDisable`, `OnDestroy` |
| Per-object properties | attributes (`SetAttribute`, `GetAttribute`, `GetAttributeChangedSignal`) and the instance's properties | `go.property("speed", 100)`: number, boolean, string, hash, url, vector3, vector4, quaternion, resource; literal defaults only | `@export var` | public or `[SerializeField]` fields |
| Reading and writing | properties on the instance, written through (`part.Position = v`)* | `go.get_position()` returns a copy, `go.set_position(p)` writes; `go.get`/`go.set` by URL | properties on the node, written through* | `transform.position` written through; reading it gives a struct copy (assigning `transform.position.x` is a compile error in C#)* |
| Events | signals: `:Connect`, `:Once`, `:Wait`, `:Disconnect` | `on_message(self, message_id, message, sender)`; engine events arrive as messages (collision and trigger responses)* | signals (`body_entered`), declared with `signal`, sent with `.emit()` | event functions by name (`OnTriggerEnter`), `UnityEvent`, `SendMessage`* |
| Spawn, despawn | `Clone`, `Destroy` | `factory.create` with a property table, `go.delete` | `instantiate()` then `add_child`; `queue_free()` at the end of the frame, `free()` at once | `Instantiate`; `Destroy` after the current Update loop, or after a delay |
| Finding | `FindFirstChild(name, recursive)`, `WaitForChild` (yields), `GetDescendants`, tags | addresses: `"enemy#controller"`, `"."`, `"#"` | `get_node`, `$Path`, groups, exported node references | Inspector references, `GetComponent`, tags* |
| Time | `task.wait`, `task.delay`*; `time()` is the fixed simulation time under server authority, `tick()` and `os.clock()` are not rolled back | `timer.delay`; the docs recommend timers and messages over `update` | `create_timer`, `await`*; `delta` | coroutines (`WaitForSeconds`), `Time.deltaTime` |
| Input | `UserInputService` events*; `InputAction`s for anything that affects the simulation under server authority | `on_input(self, action_id, action)` with input focus | `_input`, `_unhandled_input`, `Input.is_action_pressed`* | Input System actions, polled or by events* |
| Modules | `require(ModuleScript)`; one entry script per side recommended | `require("main.util")`* | `preload`, `class_name`* | assemblies and namespaces |
| Types for the IDE | luau-lsp with Roblox's definitions; `--!strict` | a built-in Lua language server and Luacheck; engine types through annotation files | GDScript's static types, the editor's completion* | C# |
| Errors | the failing thread stops, other scripts go on* | logged; the script goes on* | logged; the node goes on* | the exception logged; `Update` called again next frame* |
| Hot reload | Studio's Reload Script (Ctrl+R) applies an edited script to the running playtest (Script tab docs, through search); what happens to its state is not documented | the file is run again, `init` is not, `self` is kept, `on_reload(self)` is called; top-level locals start again | Sync Script Changes reloads every saved script in the running game | a domain reload: components serialized, assemblies reloaded, components recreated |

## Scenarios

### A door that opens when the player comes near

Each engine's usual way, written as short as it reads naturally. Lines are counted without
blank lines and the file's header comment.

**Roblox**: a transparent part without collision as the zone, a script under the door. 12
lines.

```lua
local TweenService = game:GetService("TweenService")
local Players = game:GetService("Players")
local door = script.Parent
local zone = door.Parent.Zone
local open = TweenService:Create(door, TweenInfo.new(0.5), { CFrame = door.CFrame + Vector3.new(0, 5, 0) })
local shut = TweenService:Create(door, TweenInfo.new(0.5), { CFrame = door.CFrame })
zone.Touched:Connect(function(hit)
    if Players:GetPlayerFromCharacter(hit.Parent) then open:Play() end
end)
zone.TouchEnded:Connect(function(hit)
    if Players:GetPlayerFromCharacter(hit.Parent) then shut:Play() end
end)
```

`Touched` and `TouchEnded` come once per body part, so a robust door counts the parts inside
(or polls distance, or uses a `ProximityPrompt`).

**Defold**: a collision object of type trigger on the door, its mask seeing the player's
group. 9 lines.

```lua
go.property("lift", 2.5)
function init(self) self.closed = go.get_position() end
function on_message(self, message_id, message)
    if message_id == hash("trigger_response") then
        local to = self.closed
        if message.enter then to = to + vmath.vector3(0, self.lift, 0) end
        go.animate(".", "position", go.PLAYBACK_ONCE_FORWARD, to, go.EASING_OUTQUAD, 0.5)
    end
end
```

**Godot**: an `Area3D` with a `CollisionShape3D` under the door, the player in the group
`player`. 9 lines.

```gdscript
extends AnimatableBody3D
@export var lift := 2.5
@onready var closed_y := position.y
func _ready():
    $Area3D.body_entered.connect(func(b): _move(b, closed_y + lift))
    $Area3D.body_exited.connect(func(b): _move(b, closed_y))
func _move(body, y):
    if body.is_in_group("player"):
        create_tween().tween_property(self, "position:y", y, 0.5)
```

**Unity**: a box collider with Is Trigger on the door, the player tagged `Player` and moved
by a Rigidbody or a CharacterController. 12 lines.

```csharp
using UnityEngine;
public class Door : MonoBehaviour {
    public float lift = 2.5f, speed = 2f;
    Vector3 closed; bool open;
    void Start() { closed = transform.position; }
    void OnTriggerEnter(Collider c) { if (c.CompareTag("Player")) open = true; }
    void OnTriggerExit(Collider c) { if (c.CompareTag("Player")) open = false; }
    void Update() {
        var to = open ? closed + Vector3.up * lift : closed;
        transform.position = Vector3.MoveTowards(transform.position, to, speed * Time.deltaTime);
    }
}
```

**Ours** (ADR 0114's recommended API): "Add Component → Door" on the door, the player's entity
named `Player`. 23 lines with a distance check (example 1 of the ADR), because scenes have no
trigger volume yet. With the `Trigger` component ADR 0114 asks for (E7-3), the hooks shrink
to `on_touch` and `on_untouch` counting what is inside, as in Unity.

The door's numbers (radius, lift, speed) show in the inspector in all five. Ours and Unity's
also save the door's state with the game (the props are a component; Unity's serialized
fields are part of the scene), Defold's `self.closed` and Godot's `closed_y` are not saved.

### Three more

| The developer wants | Roblox | Defold | Godot | Unity | Ours (ADR 0114) |
|---|---|---|---|---|---|
| a coin that plays a sound and disappears when picked up | `Touched`, a `Sound` cloned and played, `Destroy` | `trigger_response`, `sound.play`, `go.delete` | `body_entered`, an `AudioStreamPlayer3D` (left behind to finish), `queue_free` | `OnTriggerEnter`, `AudioSource.PlayClipAtPoint`, `Destroy` | `on_touch`, `audio.play_at` (an entity that despawns when the sound ends, ADR 0105), `self:despawn()`: 18 lines in two files (the wallet is a component of its own) |
| a hundred props spinning | a loop over `CollectionService:GetTagged` in `Heartbeat`, or a script in each | `update` in each script | `_process` in each, or a loop over a group | `Update` in each | one system over `world.query(Spin, Transform)`: one call into Luau per tick for all; 13 lines |
| walking with WASD, the camera following the mouse | `UserInputService` or `ContextActionService`; the camera in `RenderStepped` | `on_input` with `acquire_input_focus` | `Input.get_vector` in `_physics_process`, the camera in `_process` or `_input` | Input System actions, `Update` | the game's actions by name: `input.vector("move")` in `on_tick` (the tick's view), `input.vector("look")` in `on_frame` (the frame's view): 20 lines |

## What Luau and mlua give the API (read in the sources we build)

- **Definition files** (`.d.luau`): Luau 0.736 parses `declare extern type Name [extends
  Base] with ... end`, `declare function` and `declare name: type`. The parser's comments
  say `declare class` is on its way out. luau-lsp reads them through
  `luau-lsp.types.definitionFiles` (`--definitions:@name=path` on the command line) and
  their documentation through `luau-lsp.types.documentationFiles` (`--docs`); its README
  warns that the definition syntax "is unstable and undocumented", so the generator must
  follow the Luau release the VM uses.
- **Require by string:** mlua 0.12's `Lua::create_require_function` implements Luau's
  require-by-string (`./`, `../`, aliases from `.luaurc`), which luau-lsp resolves the same
  way.
- **64-bit integers:** the VM has an `integer` type (`LUA_TINTEGER`) and library, but only
  behind the `LuauIntegerLibrary` flag, off by default. ADR 0032's rule (64-bit values as
  userdata) stands until it ships on.
- **Tagged light userdata:** a 64-bit value with a tag (up to 128 tags,
  `lua_pushlightuserdatatagged`), named for `typeof` (`lua_setlightuserdataname`), equal
  when value and tag are equal, hashed by value as a table key, never allocated. An
  `Entity` (32-bit index, 32-bit generation) fits exactly.
- **Vectors** are values (no allocation): `vector.create`, `magnitude`, `normalize`,
  `cross`, `dot`, `angle`, `lerp`, `clamp`, `min`, `max`, `floor`, `ceil`, `abs`, `sign`,
  `vector.zero`, `vector.one`. A vector's components cannot be assigned (`v.x = 1` is an
  error), so a field holding a vector is written whole. `math` has `lerp`, `clamp`, `map`,
  `round`, `sign` and `noise`.
- **Native code for host types:** the code generator takes `HostIrHooks` that let the host
  lower field access and method calls on its userdata types to IR: a later way to make
  component access as fast as a table's.
- **User-defined classes** exist behind debug flags (`DebugLuauUserDefinedClasses`): not
  something to build on yet.
- **Allocation:** mlua's Luau allocator calls Rust's `std::alloc`, the engine's global
  allocator (mimalloc with the dev build's statistics). Everything a script allocates is
  counted there, which the zero-allocation tests must account for.
- **Sandboxing** makes the standard library read-only and marks the environment safe for
  the compiler's fast paths (ADR 0032): the engine adds its libraries before sandboxing and
  never replaces a builtin such as `math` (that would turn the fast paths off).

## What we take

1. **Behaviour as a component with hook functions called by name** (Unity, Godot, Defold),
   and **per-instance properties declared at the top of the script** and shown in the
   inspector (Defold's `go.property`, Godot's `@export`, Unity's serialized fields). Ours
   are an ECS component registered at run time, so scene files, undo, saves, tweens,
   rollback and replication see them like any Rust component.
2. **Systems over tagged objects** (Roblox's `CollectionService` pattern, Godot's groups):
   ours are queries over components, made once and iterated every tick.
3. **Roblox's server authority rules** for the fixed tick: hooks that run in the simulation
   are re-run on rollback, use the simulation's own time and input, keep their state where
   rollback restores it, and leave effects to the presentation.
4. **The fixed/frame split** (Godot, Defold, Unity) with gameplay in the fixed hook by
   default, as our Rust hooks already do (`Game::tick`, `Game::present`).
5. **Messages delivered at known points** (Defold): ours call a named hook on the target
   after the current hook returns. **Timers that call back** (Defold's `timer.delay`):
   ours name the hook, so they can be saved.
6. **Despawn deferred** to a known point (Godot's `queue_free`, Unity's `Destroy`).
7. **Types generated from the engine** (Roblox's definitions for luau-lsp), `--!strict`,
   require by string with `.luaurc` aliases.
8. **snake_case** (Defold, Godot, Luau's own library), which is also our Rust's.
9. **`on_reload`** (Defold, ADR 0032).

## What we don't take, and why

- **Signals with connections** (Roblox, Godot): connections outlive their objects unless
  disconnected; a connected closure keeps running the old code after a reload; handlers run
  in connection order, which is hard to keep the same on every machine; deferred and
  immediate firing differ. Hooks by name have none of this and cost nothing when a script
  does not define them.
- **Waiting inside a hook** (`task.wait`, coroutines, `await`): a suspended coroutine cannot
  be saved or rolled back, and after a reload it goes on in the old code.
- **One `update` per frame as the main hook** (Unity's `Update`, Roblox's `Heartbeat`):
  gameplay written there depends on the frame rate and cannot be predicted or replayed.
- **Addresses and hashed message ids** (Defold): entities are values, hooks are named
  functions, and both are checked by the type checker.
- **One script per node** (Godot) and **scripts copied into objects** (Roblox): an entity
  takes any number of script components, and each refers to one shared file.
- **Paths through the tree as the main way to find things** (`$Area3D`): entity references
  set in the inspector survive renames and moves.
- **Reloading everything on a code change** (Unity's domain reload): one module reloads,
  its state is kept, and the per-object state lives in components anyway.
- **PascalCase** (Roblox): every name would differ from the scene files, the Rust code and
  the tween targets (`Transform.translation`).

## Addendum (2026-10-22, E7-2): the solvers and luau-lsp's settings

Checked with Luau 0.736 (the release mlua builds) and luau-lsp 1.70.1's settings.

- **A hook's `self`.** With `declare function component<P>(name: string, props: P):
  ScriptComponent<P>` and `ScriptComponent<P>`'s hooks typed `self: P & ScriptSelf`, the
  old solver types the `self` of `function Door.on_tick(self, dt: number)` from the
  table's field, and finds `self.Transform.translation = 5` an error. The new solver of
  0.736 leaves that `self` unknown. The editor's checker and the settings `ofeliaengine
  new` writes use the old solver.
- **luau-lsp's settings:** `luau-lsp.types.definitionFiles` takes an object, a package
  name to a path (`{"@ofelia": "types/ofelia.d.luau"}`); `luau-lsp.platform.type` is
  `roblox` unless set to `standard`; `luau-lsp.sourcemap.enabled` asks Rojo for a
  sourcemap unless turned off; `luau-lsp.fflags.enableNewSolver` is off by default.
- **Definitions take names only:** a property named by a reserved word (`repeat`) is a
  parse error in a definition file, so the generator leaves such fields out.
- No code was carried over from luau-lsp (MIT) or Luau (MIT); their behaviour was read.
