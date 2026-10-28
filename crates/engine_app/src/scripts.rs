//! Luau scripts in the game (ADR 0114; E7-1a, the script host).
//!
//! The project's `scripts/` folder is loaded into one VM ([`engine_script::ScriptVm`])
//! before any scene: each `.luau` file a module named by its path (`npc/guard`). A script
//! declares components with `component(name, props)`:
//!
//! ```luau
//! local Door = component("Door", { lift = 2.5, speed = 2.0, closed = vector.zero })
//!
//! function Door.on_start(self)
//!     self.closed = self.Transform.translation
//! end
//!
//! function Door.on_tick(self, dt: number)
//!     local t = self.Transform
//!     t.translation += vector.create(0, self.speed * dt, 0)
//! end
//!
//! return Door
//! ```
//!
//! - **A declared component is the world's** (`World::register_dynamic`): its props, plain
//!   data laid out by name, described by a `TypeInfo` made at run time. So the inspector
//!   and scene files can see it as they see Rust components.
//! - **Hooks run in the player's frame:**
//!   - `on_start` once, before an instance's first tick;
//!   - `on_tick(self, dt)` in every fixed tick, before the game's own;
//!   - `on_frame(self, dt, alpha)` in every frame, before its presentation;
//!   - `on_touch(self, other, point, normal)` and `on_untouch(self, other)` at the start of
//!     a tick, for the last physics step's contacts of the entity's bodies and
//!     [`crate::Trigger`]s (`other` nil for a body without an entity; ADR 0114 E7-3);
//!   - `on_stop` once the component went from a living entity: its props are gone with
//!     it, `self.entity` and `self.locals` stay.
//!
//!   Instances run in their entities' order (spawn order, the same on every machine).
//! - **`self`** holds the instance's props (`self.speed`), its entity's components, live
//!   (`self.Transform.translation`), `self.entity` and `self.locals`, a table kept between
//!   hooks and never saved.
//! - **The libraries of this step:**
//!   - `world.find(name)`;
//!   - `physics.raycast(origin, direction, max_distance, filter?)`,
//!     `physics.overlap_sphere(center, radius, filter?)` and `physics.filter(layers)`, on
//!     the game's physics world ([`crate::Game::physics`]) or the player's own
//!     (`PlayerSettings::physics`; ADR 0114 E7-3);
//!   - `time.now()`, `time.tick()`, `time.dt()`;
//!   - `quat.identity()`, `quat.axis_angle(axis, angle)`, `quat.euler(x, y, z)`;
//!   - `prop.number(default, { min, max, step, tooltip })`, `prop.integer`, `prop.entity`;
//!   - `print` and `warn`, to the log with the script's file and line.
//! - **A runtime error stops that instance:** its hooks are called no more. The error goes
//!   to the log once with `scripts/door.luau:14` (a click in the editor's Output opens it).
//!   A script that does not load is reported, and the others load.
//! - **A changed script reloads** ([`Scripts::reload_file`], E7-4b): its instances go on
//!   with their state. Props that kept their places take the new hooks in place; props that
//!   changed move each entity's values by name to a component of the new layout.

use crate::script_require;
use crate::script_views::{self, Tables};
use engine_core::math::{Quat, Vec2, Vec3, Vec3A};
use engine_ecs::{ComponentId, Entity, Relayout, World};
use engine_reflect::{Attrs, Field, Primitive, Reflect, TypeInfo, TypeKind};
use engine_script::mlua::{
    self, AnyUserData, Function, IntoLuaMulti, Lua, MetaMethod, Table, UserData, UserDataMethods,
    Value, Vector,
};
/// The debugger's breakpoints as the host takes them (ADR 0119): the editor's.
pub use engine_script::{
    BreakAction, BreakState, Breakpoint, ErrorStop, ErrorStops, Frame, Hits, Place, Resume,
    ScriptDebugger, Stopped, Variable,
};
use engine_script::{ScriptError, ScriptSettings, ScriptVm};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

/// What a prop holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Number,
    Integer,
    Boolean,
    Vector,
    Entity,
    /// `prop.asset(kind)`: an asset reference (a prefab scene, a model), none at first.
    Asset,
    /// A string (`prop.string`, or a plain one): a `String` the component owns.
    Text,
}

impl Kind {
    fn info(self) -> &'static TypeInfo {
        match self {
            Self::Number => <f32 as Reflect>::TYPE_INFO,
            Self::Integer => <i32 as Reflect>::TYPE_INFO,
            Self::Boolean => <bool as Reflect>::TYPE_INFO,
            Self::Vector => <Vec3 as Reflect>::TYPE_INFO,
            Self::Entity => <Entity as Reflect>::TYPE_INFO,
            Self::Asset => <engine_scene::AssetRef as Reflect>::TYPE_INFO,
            Self::Text => <String as Reflect>::TYPE_INFO,
        }
    }
}

/// A prop as a script declared it.
#[derive(Debug, Clone)]
struct Prop {
    name: String,
    kind: Kind,
    default: Value,
    attrs: Attrs,
    offset: usize,
}

/// A component a script declared, registered with the world.
struct Declared {
    name: String,
    /// The module that declared it, and its file (errors name it); shared, as each hook's
    /// call names it.
    module: Rc<str>,
    /// Its hooks, found on its table once its module ran.
    hooks: Hooks,
    info: &'static TypeInfo,
    id: ComponentId,
    /// Its props' defaults, laid out, its texts empty.
    defaults: Vec<u8>,
    /// Its text props: where they are and their defaults.
    texts: &'static [engine_ecs::TextField],
    /// Its table, where its module put its functions: a message or a timer names one.
    table: Table,
    /// What the profiler keeps of its hooks.
    profiled: Profiled,
}

#[derive(Default)]
struct Hooks {
    start: Option<Function>,
    tick: Option<Function>,
    frame: Option<Function>,
    stop: Option<Function>,
    touch: Option<Function>,
    untouch: Option<Function>,
    load: Option<Function>,
    /// Whether its table has any function: a message's or a timer's hook makes instances.
    handlers: bool,
}

impl Hooks {
    fn any(&self) -> bool {
        self.handlers
            || self.load.is_some()
            || self.start.is_some()
            || self.tick.is_some()
            || self.frame.is_some()
            || self.stop.is_some()
            || self.touch.is_some()
            || self.untouch.is_some()
    }
}

/// An instance: a component on an entity, its `self` made once.
struct Instance {
    this: AnyUserData,
    started: bool,
    /// An error stopped it: its hooks are called no more.
    stopped: bool,
}

/// A message's or a timer's call: the function of its hook's name, its values (at most
/// [`script_views::MAIL_VALUES`]; ADR 0114 §3: nil, booleans, numbers, strings, vectors
/// and entities).
struct Call {
    hook: mlua::LuaString,
    count: usize,
    values: [Value; 4],
}

/// `e:send(hook, …)`: a call of `hook` on every script of `target` whose component has a
/// function of that name.
struct Message {
    target: Entity,
    call: Call,
}

/// `self:after(seconds, hook, …)`: a call of `hook` on instance (`entity`, component `id`)
/// at tick `due` (`seq`: the order set, for timers of one tick).
struct Timer {
    due: u64,
    seq: u64,
    entity: Entity,
    id: ComponentId,
    call: Call,
}

/// What scripts sent and set, waiting (the VM's app data, which the host's functions
/// push to): messages in the order sent, timers by when they are due.
#[derive(Default)]
struct Mailbox {
    messages: Vec<Message>,
    /// Those past a stage's 16 passes, for the next stage.
    later: Vec<Message>,
    timers: Vec<Timer>,
    seq: u64,
}

/// The values a message or a timer carries, checked: plain values only.
fn mail_values(what: &str, values: &[Value]) -> mlua::Result<()> {
    for v in values {
        let plain = matches!(
            v,
            Value::Nil
                | Value::Boolean(_)
                | Value::Number(_)
                | Value::Integer(_)
                | Value::String(_)
                | Value::Vector(_)
        ) || script_views::value_entity(v).is_some();
        if !plain {
            return Err(mlua::Error::runtime(format!(
                "{what} carries plain values (nil, booleans, numbers, strings, vectors, entities), not a {}",
                v.type_name()
            )));
        }
    }
    Ok(())
}

/// `e:send(hook, …)` (ADR 0114 §3): the message kept, delivered after the hook that sent
/// it returns.
#[allow(clippy::type_complexity)]
fn entity_send(
    lua: &Lua,
    (target, hook, count, a, b, c, d): (Value, mlua::LuaString, usize, Value, Value, Value, Value),
) -> mlua::Result<()> {
    let Some(target) = script_views::value_entity(&target) else {
        return Err(mlua::Error::runtime("send: not an entity"));
    };
    let values = [a, b, c, d];
    let count = count.min(values.len());
    mail_values("send", &values[..count])?;
    let Some(mut mail) = lua.app_data_mut::<Mailbox>() else {
        return Err(mlua::Error::runtime("send: no mailbox"));
    };
    mail.messages.push(Message {
        target,
        call: Call {
            hook,
            count,
            values,
        },
    });
    MAIL.set(true);
    Ok(())
}

/// `self:after(seconds, hook, …)` (ADR 0114 §3): `hook` called on this instance once that
/// many seconds of game time passed, at the start of a tick (the next one at least).
#[allow(clippy::type_complexity)]
fn self_after(
    lua: &Lua,
    (ud, seconds, hook, count, a, b, c, d): (
        AnyUserData,
        f64,
        mlua::LuaString,
        usize,
        Value,
        Value,
        Value,
        Value,
    ),
) -> mlua::Result<()> {
    // SAFETY: the fast path hands over `self`s of its tag only.
    let this = unsafe { *script_views::this_at(ud.to_pointer()) };
    let values = [a, b, c, d];
    let count = count.min(values.len());
    mail_values("after", &values[..count])?;
    let clock = lua
        .app_data_ref::<Clock>()
        .map_or_else(Clock::default, |c| *c);
    let ticks = if clock.dt > 0.0 && seconds.is_finite() {
        (seconds.max(0.0) / f64::from(clock.dt)).ceil().max(1.0) as u64
    } else {
        1
    };
    let Some(mut mail) = lua.app_data_mut::<Mailbox>() else {
        return Err(mlua::Error::runtime("after: no mailbox"));
    };
    mail.seq += 1;
    let (due, seq) = (clock.ticks + ticks, mail.seq);
    let at = mail
        .timers
        .partition_point(|t| (t.due, t.seq) <= (due, seq));
    mail.timers.insert(
        at,
        Timer {
            due,
            seq,
            entity: this.entity,
            id: this.id,
            call: Call {
                hook,
                count,
                values,
            },
        },
    );
    Ok(())
}

/// The `after` timers of an entity's scripts, as a save keeps them (ADR 0114 §8): put on
/// the entity just before a save is taken ([`Scripts::before_save`]) and taken away after
/// it, as a snapshot takes plain data only; read back after a save is applied
/// ([`Scripts::after_load`]). Its entity references are remapped with the save's.
#[derive(Debug, Clone, Default, PartialEq, Reflect)]
#[reflect(hidden)]
pub struct ScriptTimers {
    /// Each timer, in the order set.
    pub timers: Vec<SavedTimer>,
}
engine_ecs::reflect_component!(ScriptTimers);

/// A timer as a save keeps it.
#[derive(Debug, Clone, Default, PartialEq, Reflect)]
pub struct SavedTimer {
    /// The script component it calls, by name.
    pub component: String,
    /// Its hook.
    pub hook: String,
    /// The ticks left until it is due.
    pub ticks: u64,
    /// The values it carries.
    pub values: Vec<SavedValue>,
}

/// A message's or a timer's value as a save keeps it.
#[derive(Debug, Clone, Default, PartialEq, Reflect)]
pub struct SavedValue {
    /// What it is: nil 0, a boolean 1, a number 2, a string 3, a vector 4, an entity 5.
    pub kind: u8,
    /// A boolean (0 or 1) or a number.
    pub number: f64,
    /// A string.
    pub text: String,
    /// A vector.
    pub vector: Vec3,
    /// An entity.
    pub entity: Entity,
}

impl SavedValue {
    fn of(v: &Value) -> Self {
        let mut out = Self::default();
        match v {
            Value::Boolean(b) => {
                out.kind = 1;
                out.number = f64::from(u8::from(*b));
            }
            Value::Number(n) => {
                out.kind = 2;
                out.number = *n;
            }
            Value::Integer(i) => {
                out.kind = 2;
                out.number = *i as f64;
            }
            Value::String(s) => {
                out.kind = 3;
                out.text = s.to_string_lossy();
            }
            Value::Vector(v) => {
                out.kind = 4;
                out.vector = Vec3::new(v.x(), v.y(), v.z());
            }
            v => {
                if let Some(e) = script_views::value_entity(v) {
                    out.kind = 5;
                    out.entity = e;
                }
            }
        }
        out
    }

    fn value(&self, lua: &Lua) -> Value {
        match self.kind {
            1 => Value::Boolean(self.number != 0.0),
            2 => Value::Number(self.number),
            3 => lua
                .create_string(&self.text)
                .map_or(Value::Nil, Value::String),
            4 => Value::Vector(Vector::new(self.vector.x, self.vector.y, self.vector.z)),
            5 => script_views::entity_value(self.entity),
            _ => Value::Nil,
        }
    }
}

/// What a module's run declared, gathered while it loads.
#[derive(Default)]
struct Loading {
    declared: Vec<(String, Table, Vec<Prop>)>,
    /// `system(name, hooks)`, in the order declared.
    systems: Vec<(String, Table)>,
}

/// A system (ADR 0114 §1): hooks called once a tick (`on_tick(dt)`) or a frame
/// (`on_frame(dt, alpha)`), after the components' instances, in the order declared.
struct System {
    name: String,
    module: Rc<str>,
    tick: Option<Function>,
    frame: Option<Function>,
    /// An error stopped it: it is called no more, until its script runs again.
    stopped: bool,
    /// What the profiler keeps of its hooks.
    profiled: Profiled,
}

thread_local! {
    /// Whether a message waits: the host looks in the mailbox only then.
    static MAIL: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    /// The world, open to the scripts for the length of a stage (null outside): a
    /// thread-local, not the VM's app data, as every field's access reads it.
    static OPEN_WORLD: core::cell::Cell<*mut World> = const { core::cell::Cell::new(core::ptr::null_mut()) };
    /// The physics world scripts query in this stage (null: none).
    static OPEN_PHYSICS: core::cell::Cell<*const crate::ScenePhysics> = const { core::cell::Cell::new(core::ptr::null()) };
    /// The editor's profiler watches the scripts ([`Scripts::set_profiling`]): their hooks
    /// are timed, and what they allocate counted.
    static PROFILING: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    /// The memory category of the hook running now (0: none), put back when a hook a hook
    /// called returns.
    static CATEGORY: core::cell::Cell<u8> = const { core::cell::Cell::new(0) };
}

/// The hooks the profiler names (ADR 0114 §11: `script Door.on_tick`).
#[derive(Clone, Copy)]
enum Hook<'a> {
    Start,
    Tick,
    Frame,
    Stop,
    Touch,
    Untouch,
    Load,
    /// A message's or a timer's hook, by its name.
    Message(&'a mlua::LuaString),
}

/// The hooks a place is kept for, by [`Hook::slot`].
const HOOKS: usize = 7;

impl Hook<'_> {
    /// Its place among a script's kept places; none for a message's.
    fn slot(self) -> Option<usize> {
        Some(match self {
            Hook::Start => 0,
            Hook::Tick => 1,
            Hook::Frame => 2,
            Hook::Stop => 3,
            Hook::Touch => 4,
            Hook::Untouch => 5,
            Hook::Load => 6,
            Hook::Message(_) => return None,
        })
    }

    /// Its name, as the script names it.
    fn name(self) -> String {
        match self {
            Hook::Start => String::from("on_start"),
            Hook::Tick => String::from("on_tick"),
            Hook::Frame => String::from("on_frame"),
            Hook::Stop => String::from("on_stop"),
            Hook::Touch => String::from("on_touch"),
            Hook::Untouch => String::from("on_untouch"),
            Hook::Load => String::from("on_load"),
            Hook::Message(name) => name.to_string_lossy(),
        }
    }
}

/// What the profiler keeps of a component's or a system's hooks (ADR 0114 §11): each hook's
/// place in Tracy, made the first time it runs while Tracy does; its memory category; and,
/// while the editor's profiler watches, its calls and time.
#[derive(Default)]
struct Profiled {
    places: core::cell::Cell<[Option<engine_profiler::Place>; HOOKS]>,
    messages: RefCell<Vec<(Box<[u8]>, engine_profiler::Place)>>,
    /// Its memory category: 1 to 255, the last shared by those past it.
    category: u8,
    /// Its hooks' calls and nanoseconds since the profile was last taken.
    spent: core::cell::Cell<(u64, u64)>,
}

impl Profiled {
    /// A profile with memory category `category`.
    fn new(category: u8) -> Self {
        Self {
            category,
            ..Self::default()
        }
    }

    /// Hook `hook`'s place in Tracy (`script Door.on_tick`, in its module's file), made the
    /// first time.
    fn place(&self, owner: &str, module: &str, hook: Hook<'_>) -> engine_profiler::Place {
        let made = || {
            let name = format!("script {owner}.{}", hook.name());
            engine_profiler::Place::new(&name, &module_file(module), 0)
        };
        match (hook.slot(), hook) {
            (Some(k), _) => {
                let mut places = self.places.get();
                let place = *places[k].get_or_insert_with(made);
                self.places.set(places);
                place
            }
            (None, Hook::Message(name)) => {
                let bytes = name.as_bytes();
                let mut messages = self.messages.borrow_mut();
                if let Some((_, place)) = messages.iter().find(|(n, _)| **n == *bytes) {
                    return *place;
                }
                let place = made();
                messages.push((Box::from(&*bytes), place));
                place
            }
            (None, _) => made(),
        }
    }
}

/// A script's hooks as the editor's profiler shows them (ADR 0114 §11).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScriptProfile {
    /// The component or the system.
    pub name: String,
    /// Its script's file, relative to the project (`scripts/door.luau`).
    pub file: String,
    /// Its hooks' calls since the profile was last taken, while the profiler watched.
    pub calls: u64,
    /// Their time, a stop's wait aside, in nanoseconds.
    pub nanos: u64,
    /// What they allocated while the profiler watched and is not freed yet, in bytes
    /// (shared by the scripts past the 254th).
    pub bytes: usize,
}

/// The components scripts may name, by their stable names: the program's reflected ones
/// and the scripts' own.
#[derive(Default)]
struct Names {
    by_name: HashMap<String, (ComponentId, &'static TypeInfo)>,
}

/// Each module's file, as `print` and `warn` name it.
#[derive(Default)]
struct Files(HashMap<String, String>);

/// The game's clock, as `time` gives it.
#[derive(Default, Clone, Copy)]
struct Clock {
    ticks: u64,
    dt: f32,
}

/// The game's scripts: one VM, the components they declare and the instances of those in
/// the world.
pub struct Scripts {
    vm: ScriptVm,
    /// What `self` and views read on the fast path (ADR 0114 E7-1c), boxed: the C
    /// functions find it through a pointer for the length of a stage.
    tables: Box<Tables>,
    /// Each module's file, relative to the project.
    files: HashMap<String, PathBuf>,
    components: Vec<Declared>,
    by_name: HashMap<String, usize>,
    /// The instances in their entities' order: (entity index, component), the entity.
    instances: BTreeMap<(u32, u32), (Entity, Instance)>,
    /// The entities seen in this stage, for the instances gone (scratch).
    seen: HashSet<(u32, u32)>,
    /// The instances called in a stage, in their order (scratch: a stage allocates
    /// nothing).
    order: Vec<(u32, u32)>,
    /// The components that hear contacts (scratch).
    listening: Vec<usize>,
    /// The entities a stage's despawns take, with their children (scratch).
    gone: Vec<Entity>,
    /// The systems, in the order declared (ADR 0114 §1).
    systems: Vec<System>,
    /// Messages being delivered, the timers due, the receivers of one (scratch).
    letters: Vec<Message>,
    due: Vec<Timer>,
    receivers: Vec<(u32, u32)>,
    /// A save was applied: the next tick's stage calls every instance's `on_load`.
    loaded: bool,
    /// A joined client's tick (ADR 0114 §8): only its predicted entities' instances run.
    predicted_only: bool,
    /// The registry references of queries collected, let go of at a stage's start
    /// (scratch), and the VM's main state, which names their VM.
    dead: Vec<std::ffi::c_int>,
    main: usize,
    clock: Clock,
    /// Errors already in the log.
    reported: HashSet<String>,
    /// Each module's text as it last ran, hashed: a reload of the same text does nothing.
    sources: HashMap<String, u64>,
    /// Each module's text as it last ran, which runs again when a module it requires is
    /// reloaded.
    texts: HashMap<String, Rc<str>>,
    /// The project's modules as `require` finds them (ADR 0114 E7-5b), and what each
    /// requires by string, by module.
    modules: Rc<RefCell<script_require::Modules>>,
    needs: BTreeMap<String, Vec<String>>,
    /// Each component's registry name's version: the next layout of its props is
    /// `script::Door#2`, and so on.
    versions: HashMap<String, u32>,
    /// The physics world whose contacts were heard last (its address), and its step then:
    /// a step's contacts are heard once.
    heard: (usize, u64),
    /// A step's contacts, copied out of the physics world before any is heard (scratch):
    /// no hook runs while the stage holds the physics world.
    contacts: Vec<Contact>,
    /// The memory categories given to components and systems so far (ADR 0114 §11).
    categories: u16,
}

/// A contact between two bodies, by their entities, as the instances hear it.
#[derive(Clone, Copy)]
struct Contact {
    a: Option<Entity>,
    b: Option<Entity>,
    begin: bool,
    point: Vec3,
    normal: Vec3,
}

/// What reloading a script did ([`Scripts::reload_file`]).
#[derive(Debug, Clone, Default)]
pub struct Reloaded {
    /// The script's file, relative to the project.
    pub file: String,
    /// Its components whose props kept their places: they take the new hooks and
    /// attributes.
    pub kept: Vec<String>,
    /// Those whose props changed: their values moved by name to a component of the new
    /// layout, and how. What holds values of the old (an editor's undo) follows it.
    pub relaid: Vec<(String, Relayout)>,
    /// Those declared for the first time.
    pub added: Vec<String>,
    /// Those no longer declared: their data stays, without hooks.
    pub gone: Vec<String>,
    /// The instances that go on, their state kept.
    pub instances: usize,
    /// The scripts that require it, run again after it (their files).
    pub dependents: Vec<String>,
    /// How long it took.
    pub took: Duration,
}

impl Reloaded {
    /// Nothing changed: the script's text was the one that ran.
    pub fn is_empty(&self) -> bool {
        self.kept.is_empty()
            && self.relaid.is_empty()
            && self.added.is_empty()
            && self.gone.is_empty()
    }
}

impl Scripts {
    /// The scripts under `folder` (the project's `scripts/`; `root` the project, which file
    /// names are relative to), each `.luau` a module named by its path from `folder`, loaded
    /// in that order into a new VM; their components registered with `world`. A script that
    /// does not load is in the log, and the rest load.
    ///
    /// # Errors
    ///
    /// When the VM cannot be made.
    pub fn load(world: &mut World, root: &Path, folder: &Path) -> Result<Self, ScriptError> {
        Self::load_with(world, root, folder, ScriptSettings::default())
    }

    /// [`Self::load`] into a VM made with `settings` (a debuggable compilation, ADR 0119).
    ///
    /// # Errors
    ///
    /// When the VM cannot be made.
    pub fn load_with(
        world: &mut World,
        root: &Path,
        folder: &Path,
        settings: ScriptSettings,
    ) -> Result<Self, ScriptError> {
        engine_profiler::zone!("scripts::load");
        // `require` between the project's scripts, over the modules found below.
        let modules = Rc::new(RefCell::new(script_require::Modules::default()));
        let vm = ScriptVm::new_with(settings, |lua| {
            install(lua)?;
            let require = script_require::require_function(lua, modules.clone())?;
            lua.globals().set("require", require)
        })?;
        // A stop describes `self`, entities and views through reflection (ADR 0119).
        vm.debugger().set_describer(Some(script_views::describe));
        let mut tables = Box::new(Tables::default());
        tables.slow = vm
            .lua()
            .remove_app_data::<script_views::Slow>()
            .unwrap_or_default();
        let mut names = Names::default();
        for t in engine_ecs::reflected_types() {
            if let engine_ecs::ReflectedKind::Component { id, .. } = t.kind {
                let id = id(world);
                names.by_name.insert(t.info.name.to_owned(), (id, t.info));
                tables.name(t.info.name, id, t.info);
            }
        }
        tables.transform = names.by_name.get("Transform").map(|&(id, _)| id);
        vm.lua().set_app_data(names);
        vm.lua().set_app_data(Clock::default());
        vm.lua().set_app_data(Mailbox::default());
        vm.lua().set_app_data(ProjectAssets {
            root: root.to_path_buf(),
            paths: HashMap::new(),
        });
        vm.lua()
            .set_app_data(engine_scene::SceneRegistry::from_program());
        let mut scripts = Self {
            vm,
            tables,
            files: HashMap::new(),
            components: Vec::new(),
            by_name: HashMap::new(),
            instances: BTreeMap::new(),
            seen: HashSet::new(),
            order: Vec::new(),
            listening: Vec::new(),
            gone: Vec::new(),
            systems: Vec::new(),
            letters: Vec::new(),
            due: Vec::new(),
            receivers: Vec::new(),
            loaded: false,
            predicted_only: false,
            dead: Vec::new(),
            main: 0,
            clock: Clock::default(),
            reported: HashSet::new(),
            sources: HashMap::new(),
            texts: HashMap::new(),
            modules,
            needs: BTreeMap::new(),
            heard: (0, 0),
            contacts: Vec::new(),
            versions: HashMap::new(),
            categories: 0,
        };
        let mut main = 0;
        // SAFETY: the VM's state; only its main state's address is read.
        let _ = unsafe {
            scripts
                .vm
                .lua()
                .exec_raw::<()>((), |state| main = script_views::main_state(state))
        };
        scripts.main = main;
        script_views::vm_made(main);
        let mut files = Vec::new();
        collect_scripts(folder, &mut files);
        files.sort();
        let mut shown_files = Files::default();
        for file in &files {
            if let Ok(relative) = file.strip_prefix(folder) {
                let module = relative
                    .with_extension("")
                    .to_string_lossy()
                    .replace('\\', "/");
                let shown = file.strip_prefix(root).unwrap_or(file);
                shown_files
                    .0
                    .insert(module, shown.to_string_lossy().replace('\\', "/"));
            }
        }
        scripts.vm.lua().set_app_data(shown_files);
        let mut texts = Vec::new();
        for file in files {
            let Ok(relative) = file.strip_prefix(folder) else {
                continue;
            };
            let module = relative
                .with_extension("")
                .to_string_lossy()
                .replace('\\', "/");
            let shown = file.strip_prefix(root).unwrap_or(&file).to_path_buf();
            scripts.files.insert(module.clone(), shown.clone());
            match std::fs::read_to_string(&file) {
                Ok(s) => texts.push((module, s)),
                Err(e) => log::error!(target: "script", "{}: {e}", shown.display()),
            }
        }
        let folder_path: Vec<String> = folder
            .strip_prefix(root)
            .unwrap_or(folder)
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        *scripts.modules.borrow_mut() = script_require::Modules::new(
            root.to_path_buf(),
            folder_path,
            texts.iter().map(|(m, _)| m.clone()),
        );
        // What each requires runs before it (ADR 0114 E7-5b).
        for (module, source) in &texts {
            scripts.note_needs(module, source);
        }
        let (order, cycles) = script_require::order(&scripts.needs);
        for cycle in cycles {
            let files: Vec<String> = cycle.iter().map(|m| scripts.file(m)).collect();
            log::error!(
                target: "script",
                "{}: these scripts require each other, so one runs before what it requires",
                files.join(" → ")
            );
        }
        let mut texts: HashMap<String, String> = texts.into_iter().collect();
        for module in order {
            if let Some(source) = texts.remove(&module) {
                scripts.load_module(world, &module, &source);
            }
        }
        Ok(scripts)
    }

    /// What module `module` (its text `source`) requires by string, as modules.
    fn note_needs(&mut self, module: &str, source: &str) {
        let modules = self.modules.borrow();
        let needs = script_require::requires(source)
            .iter()
            .filter_map(|path| modules.resolve(module, path))
            .collect();
        drop(modules);
        self.needs.insert(module.to_owned(), needs);
    }

    /// The modules that require `module`, and those that require them, in the order they
    /// run again after it.
    fn dependents_of(&self, module: &str) -> Vec<String> {
        let mut found: BTreeSet<String> = BTreeSet::new();
        let mut next = vec![module.to_owned()];
        while let Some(m) = next.pop() {
            for (d, needs) in &self.needs {
                if needs.contains(&m) && d != module && found.insert(d.clone()) {
                    next.push(d.clone());
                }
            }
        }
        let among: BTreeMap<String, Vec<String>> = found
            .iter()
            .map(|d| {
                let needs = self.needs[d]
                    .iter()
                    .filter(|n| found.contains(*n))
                    .cloned()
                    .collect();
                (d.clone(), needs)
            })
            .collect();
        script_require::order(&among).0
    }

    /// A script made after the start (the editor's New Script): file `file` under `folder`
    /// (the project's `scripts/`, `root` the project) loaded as a module of its own. The
    /// names of the components it declared, or why it did not load.
    ///
    /// # Errors
    ///
    /// When it cannot be read, is not under `folder`, is loaded already (reloading is its
    /// own step), or does not run.
    pub fn load_file(
        &mut self,
        world: &mut World,
        root: &Path,
        folder: &Path,
        file: &Path,
    ) -> Result<Vec<String>, String> {
        let module = module_of(folder, file)?;
        if self.vm.has_module(&module) {
            return Err(format!("{module} is loaded already"));
        }
        let shown = file
            .strip_prefix(root)
            .unwrap_or(file)
            .to_string_lossy()
            .replace('\\', "/");
        let source = std::fs::read_to_string(file).map_err(|e| format!("{shown}: {e}"))?;
        self.files.insert(module.clone(), PathBuf::from(&shown));
        if let Some(mut files) = self.vm.lua().app_data_mut::<Files>() {
            files.0.insert(module.clone(), shown);
        }
        self.modules.borrow_mut().add(&module);
        self.note_needs(&module, &source);
        let before = self.components.len();
        self.load_module(world, &module, &source);
        if !self.vm.has_module(&module) {
            return Err(format!(
                "{} did not load (the log says why)",
                self.file(&module)
            ));
        }
        Ok(self.components[before..]
            .iter()
            .map(|c| c.name.clone())
            .collect())
    }

    /// Runs module `module`'s `source` and registers what it declares.
    fn load_module(&mut self, world: &mut World, module: &str, source: &str) {
        let lua = self.vm.lua();
        lua.set_app_data(Loading::default());
        let loaded = self.vm.load_module(module, source);
        let Loading { declared, systems } = self
            .vm
            .lua()
            .remove_app_data::<Loading>()
            .unwrap_or_default();
        if let Err(e) = loaded {
            log::error!(target: "script", "{}", self.place(&e));
            return;
        }
        self.ran(module, source);
        self.take_systems(module, systems);
        for (name, table, props) in declared {
            if let Err(why) = self.register(world, module, &name, &table, props) {
                let file = self.file(module);
                log::error!(target: "script", "{file}: component `{name}`: {why}");
            }
        }
    }

    /// The systems module `module` declared (`system(name, hooks)`) as it ran: one of a
    /// name it declared before takes the new hooks where it was (started again), a new one
    /// goes last, and one it no longer declares goes.
    fn take_systems(&mut self, module: &str, declared: Vec<(String, Table)>) {
        let hook = |t: &Table, name: &str| match t.get::<Value>(name) {
            Ok(Value::Function(f)) => Some(f),
            _ => None,
        };
        let mut kept = Vec::with_capacity(declared.len());
        for (name, hooks) in declared {
            let (tick, frame) = (hook(&hooks, "on_tick"), hook(&hooks, "on_frame"));
            match self
                .systems
                .iter_mut()
                .find(|s| *s.module == *module && s.name == name)
            {
                Some(s) => {
                    s.tick = tick;
                    s.frame = frame;
                    s.stopped = false;
                }
                None => {
                    let category = self.next_category();
                    self.systems.push(System {
                        name: name.clone(),
                        module: module.into(),
                        tick,
                        frame,
                        stopped: false,
                        profiled: Profiled::new(category),
                    });
                }
            }
            kept.push(name);
        }
        self.systems
            .retain(|s| *s.module != *module || kept.contains(&s.name));
    }

    /// Module `module` ran its text `source`: what it returned is what `require` gives
    /// from now on, the text kept for its next run.
    fn ran(&mut self, module: &str, source: &str) {
        self.sources.insert(
            module.to_owned(),
            engine_core::hash::state_hash64(source.as_bytes()),
        );
        self.texts.insert(module.to_owned(), source.into());
        if let Some(exports) = self.vm.exports(module)
            && let Err(e) = script_require::ran(self.vm.lua(), module, exports.clone())
        {
            log::error!(target: "script", "{}: {e}", self.file(module));
        }
    }

    /// Script `file` (under `folder`, `root` the project) run again after it changed, its
    /// instances going on with their state (ADR 0114 E7-4b):
    /// - a component whose props kept their places takes the new hooks and attributes;
    /// - one whose props changed moves to a component of the new layout (the next registry
    ///   name, `script::Door#2`): each entity's values move by prop name ([`Relayout`]);
    /// - a component it no longer declares keeps its data and loses its hooks;
    /// - its stopped instances start again (the error may be fixed), and its errors are
    ///   reported again when they come back.
    ///
    /// A file the host has not loaded loads as [`Self::load_file`] loads it; a file whose
    /// text is the one that ran is left alone.
    ///
    /// # Errors
    ///
    /// When it cannot be read, is not under `folder`, or does not run. The old version then
    /// stays, and the error names the file and line.
    pub fn reload_file(
        &mut self,
        world: &mut World,
        root: &Path,
        folder: &Path,
        file: &Path,
    ) -> Result<Reloaded, String> {
        engine_profiler::zone!("scripts::reload");
        let started = Instant::now();
        let module = module_of(folder, file)?;
        if !self.vm.has_module(&module) {
            let added = self.load_file(world, root, folder, file)?;
            return Ok(Reloaded {
                file: self.file(&module),
                added,
                took: started.elapsed(),
                ..Reloaded::default()
            });
        }
        let shown = self.file(&module);
        let source = std::fs::read_to_string(file).map_err(|e| format!("{shown}: {e}"))?;
        let hash = engine_core::hash::state_hash64(source.as_bytes());
        if self.sources.get(&module) == Some(&hash) {
            return Ok(Reloaded {
                file: shown,
                took: started.elapsed(),
                ..Reloaded::default()
            });
        }
        let mut out = self.run_again(world, &module, &source)?;
        // Those that require it run again after it, in their order (ADR 0114 §10), so
        // none keeps what the old version returned.
        for d in self.dependents_of(&module) {
            let Some(text) = self.texts.get(&d).cloned() else {
                continue;
            };
            match self.run_again(world, &d, &text) {
                Ok(again) => {
                    out.instances += again.instances;
                    out.relaid.extend(again.relaid);
                    out.dependents.push(again.file);
                }
                Err(why) => log::error!(target: "script", "{why}"),
            }
        }
        out.took = started.elapsed();
        Ok(out)
    }

    /// Module `module` run again with text `source`, as [`Self::reload_file`] describes.
    fn run_again(
        &mut self,
        world: &mut World,
        module: &str,
        source: &str,
    ) -> Result<Reloaded, String> {
        let started = Instant::now();
        let module = module.to_owned();
        let shown = self.file(&module);
        let mut out = Reloaded {
            file: shown.clone(),
            ..Reloaded::default()
        };
        self.note_needs(&module, source);
        self.vm.lua().set_app_data(Loading::default());
        let loaded = self.vm.load_module(&module, source);
        let Loading { declared, systems } = self
            .vm
            .lua()
            .remove_app_data::<Loading>()
            .unwrap_or_default();
        loaded.map_err(|e| self.place(&e))?;
        self.ran(&module, source);
        self.take_systems(&module, systems);
        let before = self.components.len();
        let mut again = Vec::new();
        for (name, table, props) in declared {
            let result = match self.by_name.get(&name).copied() {
                Some(k) if *self.components[k].module == *module => {
                    again.push(k);
                    self.redeclare(world, k, &table, props)
                        .map(|relaid| match relaid {
                            Some(r) => out.relaid.push((name.clone(), r)),
                            None => out.kept.push(name.clone()),
                        })
                }
                _ => self
                    .register(world, &module, &name, &table, props)
                    .map(|()| out.added.push(name.clone())),
            };
            if let Err(why) = result {
                log::error!(target: "script", "{shown}: component `{name}`: {why}");
            }
        }
        // What it no longer declares: the data stays, the hooks go.
        for (k, c) in self.components[..before].iter_mut().enumerate() {
            if *c.module == *module && !again.contains(&k) {
                c.hooks = Hooks::default();
                if let Some(gone) = self.tables.forget_own(c.id) {
                    unref(self.vm.lua(), gone);
                }
                out.gone.push(c.name.clone());
            }
        }
        // Its instances go on; the stopped ones start again.
        for (&(_, k), (_, inst)) in &mut self.instances {
            if self
                .components
                .get(k as usize)
                .is_some_and(|c| *c.module == *module && c.hooks.any())
            {
                inst.stopped = false;
                out.instances += 1;
            }
        }
        let prefix = format!("{shown}:");
        self.reported.retain(|line| !line.starts_with(&prefix));
        out.took = started.elapsed();
        Ok(out)
    }

    /// Component `k` declared again by its reloaded module, with `props` and the hooks on
    /// `table`: in place when its props kept their places; else moved to a component of the
    /// new layout, how it moved returned.
    fn redeclare(
        &mut self,
        world: &mut World,
        k: usize,
        table: &Table,
        mut props: Vec<Prop>,
    ) -> Result<Option<Relayout>, String> {
        let name = self.components[k].name.clone();
        let source = self.file(&self.components[k].module);
        let (info, defaults, texts, layout) = describe(&name, &mut props)?;
        let (old, old_info) = (self.components[k].id, self.components[k].info);
        let in_place =
            world.components().layout(old) == Some(layout) && same_places(old_info, info);
        let registry: Box<str> = if in_place {
            world.components().name(old).unwrap_or_default().into()
        } else {
            // The next free registry name (or one this layout holds already).
            let version = self.versions.entry(name.clone()).or_insert(1);
            loop {
                *version += 1;
                let candidate = format!("script::{name}#{version}");
                match world.components().id_by_name(&candidate) {
                    Some(id) if world.components().layout(id) != Some(layout) => continue,
                    _ => break candidate.into(),
                }
            }
        };
        let runtime = engine_ecs::RuntimeType {
            info,
            component: registry,
            layout,
            defaults: defaults.clone().into_boxed_slice(),
            texts,
            source: source.into(),
        };
        let relayout = if in_place {
            world.set_type_info(old, info);
            None
        } else {
            let new = runtime
                .id(world)
                .ok_or_else(|| String::from("the world holds another component of its name"))?;
            let r = Relayout::between(old, old_info, new, info, runtime.defaults.clone(), texts);
            move_values(world, &r);
            Some(r)
        };
        let id = relayout.as_ref().map_or(old, |r| r.new);
        // SAFETY: the values are plain data of `layout` but for the texts, as `register`'s.
        unsafe { engine_ecs::add_runtime_type(runtime) };
        if let Some(mut names) = self.vm.lua().app_data_mut::<Names>() {
            names.by_name.insert(name.clone(), (id, info));
        }
        self.tables.name(&name, id, info);
        if id != old
            && let Some(gone) = self.tables.forget_own(old)
        {
            unref(self.vm.lua(), gone);
        }
        self.own(id, table);
        let c = &mut self.components[k];
        c.hooks = hooks_of(table);
        c.info = info;
        c.id = id;
        c.defaults = defaults;
        c.texts = texts;
        c.table = table.clone();
        // Its instances' timers follow it to its new layout.
        if id != old
            && let Some(mut mail) = self.vm.lua().app_data_mut::<Mailbox>()
        {
            for t in &mut mail.timers {
                if t.id == old {
                    t.id = id;
                }
            }
        }
        // Its instances' `self`: the new place and description, their locals kept; every
        // `self`'s kept views go, as they may name the old component.
        let tables = &mut *self.tables;
        for (&(_, at), (_, inst)) in &self.instances {
            let this = inst.this.to_pointer();
            let ours = at as usize == k;
            // SAFETY: instances' `self`s are the host's own, of `self`'s tag; the VM's state
            // for the references.
            let _ = unsafe {
                self.vm.lua().exec_raw::<()>((), |state| {
                    let this = script_views::this_at(this);
                    if ours {
                        script_views::retarget(state, tables, this, id, info);
                    } else {
                        script_views::forget_views(state, this);
                    }
                })
            };
        }
        Ok(relayout)
    }

    /// Registers component `name` of `module` with its props, its hooks from `table`.
    fn register(
        &mut self,
        world: &mut World,
        module: &str,
        name: &str,
        table: &Table,
        mut props: Vec<Prop>,
    ) -> Result<(), String> {
        if self.by_name.contains_key(name)
            || self
                .vm
                .lua()
                .app_data_ref::<Names>()
                .is_some_and(|n| n.by_name.contains_key(name))
        {
            return Err(String::from("another component has this name"));
        }
        let (info, defaults, texts, layout) = describe(name, &mut props)?;
        // Beside the program's types, so scenes and the editor find it by name.
        let runtime = engine_ecs::RuntimeType {
            info,
            component: format!("script::{name}").into(),
            layout,
            defaults: defaults.clone().into_boxed_slice(),
            texts,
            source: self.file(module).into(),
        };
        let id = runtime
            .id(world)
            .ok_or_else(|| String::from("the world holds another component of its name"))?;
        // SAFETY: the values are plain data of `layout` (numbers, booleans, vectors,
        // entities and assets by value) but for the texts, `String`s at `texts`; `Send`
        // and `Sync`; the defaults are one, their texts empty.
        unsafe { engine_ecs::add_runtime_type(runtime) };
        let hooks = hooks_of(table);
        if let Some(mut names) = self.vm.lua().app_data_mut::<Names>() {
            names.by_name.insert(name.to_owned(), (id, info));
        }
        self.tables.name(name, id, info);
        self.own(id, table);
        self.by_name.insert(name.to_owned(), self.components.len());
        let category = self.next_category();
        self.components.push(Declared {
            name: name.to_owned(),
            module: module.into(),
            hooks,
            info,
            id,
            defaults,
            texts,
            table: table.clone(),
            profiled: Profiled::new(category),
        });
        Ok(())
    }

    /// The memory category of the next component or system declared: 1 to 255, the last
    /// shared by those past the 254th (Luau keeps 256; 0, the VM's own, is none).
    fn next_category(&mut self) -> u8 {
        self.categories = (self.categories + 1).min(255);
        u8::try_from(self.categories).unwrap_or(u8::MAX)
    }

    /// Whether the editor's profiler watches the scripts run on this thread (ADR 0114 §11):
    /// while it does, each component's and system's hooks are counted with their time, a
    /// stop's wait aside, and what they allocate is counted against its memory category
    /// ([`Self::take_profile`]). Off, a hook pays one thread-local read for it; its Tracy
    /// zone needs only Tracy running.
    pub fn set_profiling(&mut self, on: bool) {
        PROFILING.set(on);
    }

    /// Each component's and system's profile since the last taken, components first, in
    /// `into` (its rows' texts reused: nothing allocated in steady state). Their counts
    /// start again.
    pub fn take_profile(&mut self, into: &mut Vec<ScriptProfile>) {
        let rows = self
            .components
            .iter()
            .map(|c| (&c.name, &c.module, &c.profiled));
        let rows = rows.chain(
            self.systems
                .iter()
                .map(|s| (&s.name, &s.module, &s.profiled)),
        );
        let mut n = 0;
        for (name, module, profiled) in rows {
            if n == into.len() {
                into.push(ScriptProfile::default());
            }
            let row = &mut into[n];
            row.name.clear();
            row.name.push_str(name);
            row.file.clear();
            row.file.push_str("scripts/");
            row.file.push_str(module);
            row.file.push_str(".luau");
            (row.calls, row.nanos) = profiled.spent.replace((0, 0));
            row.bytes = self.vm.memory_of(profiled.category);
            n += 1;
        }
        into.truncate(n);
    }

    /// The script components declared, by name.
    pub fn component_names(&self) -> impl Iterator<Item = &str> {
        self.components.iter().map(|c| c.name.as_str())
    }

    /// Script component `name`'s id in the world.
    pub fn component(&self, name: &str) -> Option<ComponentId> {
        self.by_name.get(name).map(|&k| self.components[k].id)
    }

    /// The file script component `name` was declared in, relative to the project.
    pub fn file_of(&self, name: &str) -> Option<&Path> {
        let k = *self.by_name.get(name)?;
        self.files
            .get(&*self.components[k].module)
            .map(PathBuf::as_path)
    }

    /// Gives `entity` script component `name`, its props at their defaults; false for an
    /// unknown component or a dead entity.
    pub fn add(&self, world: &mut World, entity: Entity, name: &str) -> bool {
        let Some(c) = self.by_name.get(name).map(|&k| &self.components[k]) else {
            return false;
        };
        if c.texts.is_empty() {
            // SAFETY: the defaults are a value of the component's layout, plain data, copied.
            return unsafe { world.insert_raw(entity, c.id, c.defaults.as_ptr()) };
        }
        let mut value = c.defaults.clone();
        // SAFETY: a value of the component's layout, its texts empty, each replaced by its
        // default (unaligned, as the room is bytes) and moved into the world by
        // `insert_raw`: the copy left is bytes only.
        unsafe {
            for t in c.texts {
                value
                    .as_mut_ptr()
                    .add(t.offset)
                    .cast::<String>()
                    .write_unaligned(String::from(t.default));
            }
            world.insert_raw(entity, c.id, value.as_ptr())
        }
    }

    /// A fixed tick's stage (before the game's own): instances that came are started, then
    /// every instance's `on_tick`, in their entities' order.
    pub fn tick(&mut self, world: &mut World, dt: f32) {
        self.tick_with(world, dt, None);
    }

    /// [`Self::tick`], scripts querying `physics` (`physics.raycast`; ADR 0114 E7-3).
    pub fn tick_with(&mut self, world: &mut World, dt: f32, physics: Option<&crate::ScenePhysics>) {
        // SAFETY: both are this call's borrows, alive until it returns.
        unsafe {
            self.tick_open(
                world,
                dt,
                physics.map_or(core::ptr::null(), core::ptr::from_ref),
            )
        }
    }

    /// [`Self::tick_with`] through pointers, for a stage that may stop halfway (ADR 0119):
    /// it reaches the world and the physics world only through the stage's open pointers,
    /// read again after every hook, so none of its frames holds them while it waits; the
    /// player opens them anew ([`reopen`]) before it goes on.
    ///
    /// # Safety
    ///
    /// `world`, and `physics` when not null, are valid until the stage ends, but for what
    /// [`reopen`] puts in their place; nothing else uses this host meanwhile.
    pub unsafe fn tick_open(
        &mut self,
        world: *mut World,
        dt: f32,
        physics: *const crate::ScenePhysics,
    ) {
        engine_profiler::zone!("scripts::tick");
        self.clock.ticks += 1;
        self.clock.dt = dt;
        // In place: setting app data boxes it anew.
        if let Some(mut clock) = self.vm.lua().app_data_mut::<Clock>() {
            *clock = self.clock;
        }
        self.open(world, physics, false);
        self.follow();
        if core::mem::take(&mut self.loaded) {
            self.call_loaded();
        }
        self.deliver_later();
        self.timers_due();
        self.touches();
        self.call_all(Stage::Tick, dt, 0.0);
        self.despawn_asked();
        self.close();
    }

    /// A joined client's tick (ADR 0114 §8, ADR 0074): the tick hooks of the instances of
    /// its predicted entities ([`engine_net::Predicted`]) only, in every predicted tick and
    /// again in every re-simulated one; the rest of the world comes from the host's
    /// snapshots. No systems, timers or contacts run here: the host's are the world's.
    pub fn tick_predicted(&mut self, world: &mut World, dt: f32) {
        engine_profiler::zone!("scripts::tick_predicted");
        self.clock.ticks += 1;
        self.clock.dt = dt;
        if let Some(mut clock) = self.vm.lua().app_data_mut::<Clock>() {
            *clock = self.clock;
        }
        self.open(world, core::ptr::null(), false);
        self.follow();
        self.predicted_only = true;
        self.call_all(Stage::Tick, dt, 0.0);
        self.predicted_only = false;
        self.despawn_asked();
        self.close();
    }

    /// A frame's stage, before its presentation: every instance's `on_frame`.
    pub fn frame(&mut self, world: &mut World, dt: f32, alpha: f32) {
        self.frame_with(world, dt, alpha, None);
    }

    /// [`Self::frame`], scripts querying `physics`.
    pub fn frame_with(
        &mut self,
        world: &mut World,
        dt: f32,
        alpha: f32,
        physics: Option<&crate::ScenePhysics>,
    ) {
        let physics = physics.map_or(core::ptr::null(), core::ptr::from_ref);
        // SAFETY: both are this call's borrows, alive until it returns.
        unsafe { self.frame_open(world, dt, alpha, physics) }
    }

    /// Whether a frame's stage has anything to call: a frame hook of a component or a
    /// system.
    pub fn has_frame_hooks(&self) -> bool {
        self.components.iter().any(|c| c.hooks.frame.is_some())
            || self.systems.iter().any(|s| s.frame.is_some())
    }

    /// [`Self::frame_with`] through pointers, as [`Self::tick_open`].
    ///
    /// # Safety
    ///
    /// As [`Self::tick_open`]'s.
    pub unsafe fn frame_open(
        &mut self,
        world: *mut World,
        dt: f32,
        alpha: f32,
        physics: *const crate::ScenePhysics,
    ) {
        engine_profiler::zone!("scripts::frame");
        if !self.has_frame_hooks() {
            return;
        }
        self.open(world, physics, true);
        self.follow();
        self.deliver_later();
        self.call_all(Stage::Frame, dt, alpha);
        self.despawn_asked();
        self.close();
    }

    /// Script component `id`'s table, `table`: where `self:open()` finds `Door.open`. The
    /// registry `e:add` makes defaults with is made again, as it may be new.
    fn own(&mut self, id: ComponentId, table: &Table) {
        let lua = self.vm.lua();
        if let Ok(own) = registry_ref(lua, table.clone())
            && let Some(old) = self.tables.set_own(id, own)
        {
            unref(lua, old);
        }
        if let Some(mut registry) = lua.app_data_mut::<engine_scene::SceneRegistry>() {
            *registry = engine_scene::SceneRegistry::from_program();
        }
    }

    /// The entities scripts despawned in this stage, with their children (ADR 0114 §7):
    /// their instances' `on_stop` first, their props still there, then the entities go.
    /// Despawns asked by those `on_stop`s go too, up to 16 passes; the rest wait for the
    /// next stage.
    fn despawn_asked(&mut self) {
        for _ in 0..16 {
            if self.tables.despawns.is_empty() {
                return;
            }
            let mut gone = core::mem::take(&mut self.gone);
            gone.clear();
            gone.append(&mut self.tables.despawns);
            let asked = gone.len();
            {
                // SAFETY: the stage's world, borrowed for this block only (no hook runs in
                // it).
                let world = unsafe { &*open_world() };
                let mut at = 0;
                while at < gone.len() {
                    let e = gone[at];
                    at += 1;
                    gone.extend(world.children(e));
                }
            }
            for &e in &gone {
                self.stop_instances_of(e);
            }
            // SAFETY: as above, read again after the hooks (a stop opens it anew).
            let world = unsafe { &mut *open_world() };
            for &e in &gone[..asked] {
                world.despawn_recursive(e);
            }
            self.gone = gone;
        }
    }

    /// `on_stop` of every instance on `e` still running, which runs no more.
    fn stop_instances_of(&mut self, e: Entity) {
        let mut keys = core::mem::take(&mut self.order);
        keys.clear();
        keys.extend(
            self.instances
                .range((e.index(), 0)..=(e.index(), u32::MAX))
                .map(|(&key, _)| key),
        );
        for &key in &keys {
            let Some((at, inst)) = self.instances.get_mut(&key) else {
                continue;
            };
            if *at != e || inst.stopped {
                continue;
            }
            inst.stopped = true;
            let this = inst.this.clone();
            let c = &self.components[key.1 as usize];
            if let Some(stop) = c.hooks.stop.clone()
                && let Err(err) = call_for(&self.vm, e, c, Hook::Stop, &stop, this)
            {
                self.report(key.1 as usize, "on_stop", e, &err);
            }
        }
        self.order = keys;
    }

    /// Every instance's hook of `stage`, in the instances' order, then the systems', in
    /// the order declared.
    fn call_all(&mut self, stage: Stage, dt: f32, alpha: f32) {
        let mut order = core::mem::take(&mut self.order);
        order.clear();
        order.extend(self.instances.keys().copied());
        for &key in &order {
            // A client runs its predicted entities only.
            if self.predicted_only {
                let e = self.instances.get(&key).map(|(e, _)| *e);
                let world = open_world();
                let predicted = match e {
                    Some(e) if !world.is_null() => {
                        // SAFETY: the stage's world (`open_world`), read for this check only.
                        unsafe { &*world }.get::<engine_net::Predicted>(e).is_some()
                    }
                    _ => false,
                };
                if !predicted {
                    continue;
                }
            }
            self.call(key, stage, dt, alpha);
            self.deliver();
        }
        self.order = order;
        if self.predicted_only {
            return;
        }
        for k in 0..self.systems.len() {
            let s = &self.systems[k];
            let hook = match stage {
                Stage::Tick => &s.tick,
                Stage::Frame => &s.frame,
            };
            let (Some(hook), false) = (hook.clone(), s.stopped) else {
                continue;
            };
            let (vm, owner) = (&self.vm, (s.name.as_str(), &*s.module, &s.profiled));
            let result = match stage {
                Stage::Tick => call_hook(vm, None, owner, Hook::Tick, &hook, dt),
                Stage::Frame => call_hook(vm, None, owner, Hook::Frame, &hook, (dt, alpha)),
            };
            self.deliver();
            if let Err(err) = result
                && !matches!(err, ScriptError::Aborted { .. })
            {
                let name = match stage {
                    Stage::Tick => "on_tick",
                    Stage::Frame => "on_frame",
                };
                self.systems[k].stopped = true;
                let line = format!("{} ({}.{name})", self.place(&err), self.systems[k].name);
                if self.reported.insert(line.clone()) {
                    log::error!(target: "script", "{line}, the system stopped");
                }
            }
        }
    }

    /// The world (and `physics`, when not null) open to the scripts until [`Self::close`];
    /// `frame`: a frame's stage, whose `input` is the frame's view.
    fn open(&mut self, world: *mut World, physics: *const crate::ScenePhysics, frame: bool) {
        script_views::take_dead_refs(self.main, &mut self.dead);
        if !self.dead.is_empty() {
            let dead = core::mem::take(&mut self.dead);
            // SAFETY: the references the queries took with `lua_ref`, each let go once.
            let _ = unsafe {
                self.vm.lua().exec_raw::<()>((), |state| {
                    for &r in &dead {
                        mlua::ffi::lua_unref(state, r);
                    }
                })
            };
            self.dead = dead;
            self.dead.clear();
        }
        OPEN_WORLD.set(world);
        OPEN_PHYSICS.set(physics);
        self.tables.frame = frame;
        script_views::open(&mut self.tables);
    }

    fn close(&self) {
        OPEN_WORLD.set(core::ptr::null_mut());
        OPEN_PHYSICS.set(core::ptr::null());
        script_views::close();
        self.log_breakpoints();
    }

    /// The logpoints' messages since the last stage, to the log at their lines, as `print`
    /// writes (ADR 0119): nothing to do, and nothing made, without them.
    fn log_breakpoints(&self) {
        for l in self.vm.take_logged() {
            log::info!(target: "script", "{}:{}: {}", self.file(&l.module), l.line, l.text);
        }
    }

    /// Script module `module`'s breakpoints from now on (the editor's; ADR 0119): placed in
    /// the VM at once, or when it loads, and kept through its reloads. Their messages go to
    /// the log at the end of each stage.
    pub fn set_breakpoints(&mut self, module: &str, breakpoints: Vec<engine_script::Breakpoint>) {
        self.vm.set_breakpoints(module, breakpoints);
    }

    /// How module `module`'s breakpoints are doing, in the order they were set.
    pub fn breakpoint_states(&self, module: &str) -> Vec<engine_script::BreakState> {
        self.vm.breakpoint_states(module)
    }

    /// The pause point reached since the last call, if one was: its file (relative to the
    /// project, `scripts/door.luau`) and line. The game pauses after its tick.
    pub fn take_debug_pause(&mut self) -> Option<(String, u32)> {
        self.vm
            .take_pause()
            .map(|(module, line)| (self.file(&module), line))
    }

    /// Whether a pause point was reached and its pause not taken yet: the frame runs no
    /// more ticks.
    pub fn debug_pause_asked(&self) -> bool {
        self.vm.pause_asked()
    }

    /// Every breakpoint's hits and latest message back to none: a new Play (ADR 0119).
    pub fn reset_breakpoint_states(&mut self) {
        self.vm.reset_breakpoint_states();
    }

    /// Raised whenever a breakpoint's state changes: the editor reads them again only
    /// when it moved ([`Self::breakpoint_states`]).
    pub fn breakpoints_version(&self) -> u64 {
        self.vm.breakpoints_version()
    }

    /// The VM's debugger apart from the host (ADR 0119): what reads and sets breakpoints
    /// while a stage is stopped halfway and holds the host.
    pub fn debugger(&self) -> engine_script::ScriptDebugger {
        self.vm.debugger()
    }

    /// Debugging (ADR 0119 E9-1b-2): the hooks run on a thread of their own from now on,
    /// so a stop in one can be stepped from; off, they are called the plain way again.
    pub fn set_debugging(&self, on: bool) {
        self.vm.set_debugging(on);
    }

    /// The module of script file `file` (relative to the project, as the editor names it),
    /// if it is loaded.
    pub fn module_of(&self, file: &Path) -> Option<&str> {
        self.files
            .iter()
            .find(|(_, f)| f.as_path() == file)
            .map(|(m, _)| m.as_str())
    }

    /// The instances made to match the world: those that came get a `self` and are started
    /// (in their entities' order), those whose component went are stopped.
    fn follow(&mut self) {
        self.seen.clear();
        let mut came: Vec<((u32, u32), Entity)> = Vec::new();
        // SAFETY: the stage's world, borrowed until the instances that came are found (no
        // hook runs meanwhile).
        let world = unsafe { &*open_world() };
        for (k, c) in self.components.iter().enumerate() {
            if !c.hooks.any() {
                continue;
            }
            for arch in world.archetypes() {
                if !arch.components().contains(&c.id) {
                    continue;
                }
                for chunk in 0..arch.chunk_count() {
                    for &e in arch.chunk_entities(chunk) {
                        let key = (e.index(), k as u32);
                        self.seen.insert(key);
                        let same = self.instances.get(&key).is_some_and(|(had, _)| *had == e);
                        if !same {
                            came.push((key, e));
                        }
                    }
                }
            }
        }
        // Gone: stopped while their entity lives.
        let gone: Vec<(u32, u32)> = self
            .instances
            .keys()
            .filter(|k| !self.seen.contains(k))
            .copied()
            .collect();
        for key in gone {
            let Some((e, inst)) = self.instances.remove(&key) else {
                continue;
            };
            // SAFETY: as above, read again for each (an `on_stop` may stop and the stage
            // open it anew).
            if unsafe { &*open_world() }.contains(e)
                && !inst.stopped
                && let Some(stop) = self.components[key.1 as usize].hooks.stop.clone()
            {
                let c = &self.components[key.1 as usize];
                if let Err(err) = call_for(&self.vm, e, c, Hook::Stop, &stop, inst.this.clone()) {
                    self.report(key.1 as usize, "on_stop", e, &err);
                }
            }
            let this = inst.this.to_pointer();
            // SAFETY: the instance's `self`, of `self`'s tag; the VM's state for the
            // references.
            let _ = unsafe {
                self.vm.lua().exec_raw::<()>((), |state| {
                    script_views::release(state, script_views::this_at(this));
                })
            };
        }
        came.sort_unstable_by_key(|(k, _)| *k);
        for (key, e) in came {
            let c = &self.components[key.1 as usize];
            let (id, info) = (c.id, c.info);
            let tables = &mut *self.tables;
            // SAFETY: room for the one value pushed, returned as the userdata.
            let made = unsafe {
                self.vm.lua().exec_raw::<AnyUserData>((), |state| {
                    script_views::push_this(state, tables, e, id, info);
                })
            };
            let Ok(this) = made else {
                continue;
            };
            self.instances.insert(
                key,
                (
                    e,
                    Instance {
                        this,
                        started: false,
                        stopped: false,
                    },
                ),
            );
        }
    }

    /// Calls instance `key`'s hook of `stage` (its `on_start` first, the first time).
    fn call(&mut self, key: (u32, u32), stage: Stage, dt: f32, alpha: f32) {
        let k = key.1 as usize;
        let Some((e, inst)) = self.instances.get_mut(&key) else {
            return;
        };
        if inst.stopped {
            return;
        }
        let (e, this) = (*e, inst.this.clone());
        let c = &self.components[k];
        if !inst.started && stage == Stage::Tick {
            inst.started = true;
            if let Some(start) = c.hooks.start.clone()
                && let Err(err) = call_for(&self.vm, e, c, Hook::Start, &start, this.clone())
            {
                self.stop(key, "on_start", e, &err);
                return;
            }
        }
        let c = &self.components[k];
        let (hook, name) = match stage {
            Stage::Tick => (c.hooks.tick.clone(), "on_tick"),
            Stage::Frame => (c.hooks.frame.clone(), "on_frame"),
        };
        let Some(hook) = hook else {
            return;
        };
        let result = match stage {
            Stage::Tick => call_for(&self.vm, e, c, Hook::Tick, &hook, (this, dt)),
            Stage::Frame => call_for(&self.vm, e, c, Hook::Frame, &hook, (this, dt, alpha)),
        };
        if let Err(err) = result {
            self.stop(key, name, e, &err);
        }
    }

    /// The last physics step's contacts heard by the instances of the entities in them
    /// (ADR 0114 E7-3), once each step: `on_touch(self, other, point, normal)` when a body
    /// began touching one of the entity's bodies or its trigger, `on_untouch(self, other)`
    /// when it stopped; the normal points from the instance's entity toward the other.
    fn touches(&mut self) {
        let physics = OPEN_PHYSICS.get();
        if physics.is_null() {
            return;
        }
        // SAFETY: the stage's physics world, borrowed until its contacts are copied out.
        let physics = unsafe { &*physics };
        let at = (core::ptr::from_ref(physics) as usize, physics.steps());
        if at == self.heard {
            return;
        }
        self.heard = at;
        let mut listened = core::mem::take(&mut self.listening);
        listened.clear();
        listened.extend((0..self.components.len()).filter(|&k| {
            let h = &self.components[k].hooks;
            h.touch.is_some() || h.untouch.is_some()
        }));
        if listened.is_empty() {
            self.listening = listened;
            return;
        }
        let mut contacts = core::mem::take(&mut self.contacts);
        contacts.clear();
        contacts.extend(
            engine_physics::PhysicsBackend::contact_events(physics.backend())
                .iter()
                .map(|event| Contact {
                    a: physics.entity_of(event.a),
                    b: physics.entity_of(event.b),
                    begin: event.kind == engine_physics::ContactKind::Begin,
                    point: event.point,
                    normal: event.normal,
                }),
        );
        for c in &contacts {
            for (me, other, normal) in [(c.a, c.b, c.normal), (c.b, c.a, -c.normal)] {
                let Some(me) = me else {
                    continue;
                };
                for &k in &listened {
                    self.touch(me, k, other, c.begin, c.point, normal);
                    self.deliver();
                }
            }
        }
        self.contacts = contacts;
        self.listening = listened;
    }

    /// Instance (`me`, component `k`) hears a contact with `other` (`begin`: it began).
    fn touch(
        &mut self,
        me: Entity,
        k: usize,
        other: Option<Entity>,
        begin: bool,
        point: Vec3,
        normal: Vec3,
    ) {
        let key = (me.index(), k as u32);
        let Some((e, inst)) = self.instances.get_mut(&key) else {
            return;
        };
        if *e != me || inst.stopped {
            return;
        }
        let this = inst.this.clone();
        let started = core::mem::replace(&mut inst.started, true);
        let c = &self.components[k];
        // Its `on_start` first, the first time.
        if !started
            && let Some(start) = c.hooks.start.clone()
            && let Err(err) = call_for(&self.vm, me, c, Hook::Start, &start, this.clone())
        {
            self.stop(key, "on_start", me, &err);
            return;
        }
        let c = &self.components[k];
        let (hook, name, which) = if begin {
            (c.hooks.touch.clone(), "on_touch", Hook::Touch)
        } else {
            (c.hooks.untouch.clone(), "on_untouch", Hook::Untouch)
        };
        let Some(hook) = hook else {
            return;
        };
        let other = other.map_or(Value::Nil, script_views::entity_value);
        let vector = |v: Vec3| Value::Vector(Vector::new(v.x, v.y, v.z));
        let result = if begin {
            let args = (this, other, vector(point), vector(normal));
            call_for(&self.vm, me, c, which, &hook, args)
        } else {
            call_for(&self.vm, me, c, which, &hook, (this, other))
        };
        if let Err(err) = result {
            self.stop(key, name, me, &err);
        }
    }

    /// Before a save is taken (ADR 0114 §8): each entity's `after` timers written into a
    /// [`ScriptTimers`] on it, so the save keeps them with the entity. [`Self::after_save`]
    /// takes them away again once the save is taken.
    pub fn before_save(&mut self, world: &mut World) {
        let now = self.clock.ticks;
        let mut by_entity: BTreeMap<Entity, Vec<SavedTimer>> = BTreeMap::new();
        if let Some(mail) = self.vm.lua().app_data_ref::<Mailbox>() {
            for t in &mail.timers {
                let Some(c) = self.components.iter().find(|c| c.id == t.id) else {
                    continue;
                };
                by_entity.entry(t.entity).or_default().push(SavedTimer {
                    component: c.name.clone(),
                    hook: t.call.hook.to_string_lossy(),
                    ticks: t.due.saturating_sub(now),
                    values: t.call.values[..t.call.count]
                        .iter()
                        .map(SavedValue::of)
                        .collect(),
                });
            }
        }
        for (e, timers) in by_entity {
            if world.contains(e) {
                world.insert(e, ScriptTimers { timers });
            }
        }
    }

    /// After a save was taken: the [`ScriptTimers`] [`Self::before_save`] put on entities
    /// taken away (a snapshot takes plain data only).
    pub fn after_save(&mut self, world: &mut World) {
        let mut q = engine_ecs::QueryState::<(Entity, &ScriptTimers)>::new(world);
        let held: Vec<Entity> = q.iter_readonly(world).map(|(e, _)| e).collect();
        for e in held {
            world.remove::<ScriptTimers>(e);
        }
    }

    /// After a save was applied (ADR 0114 §8): what waited from the world before goes, the
    /// save's timers are set again from the [`ScriptTimers`] on its entities (then taken
    /// away), and the next tick's stage calls every instance's `on_load`.
    pub fn after_load(&mut self, world: &mut World) {
        let now = self.clock.ticks;
        let mut q = engine_ecs::QueryState::<(Entity, &ScriptTimers)>::new(world);
        let saved: Vec<(Entity, ScriptTimers)> = q
            .iter_readonly(world)
            .map(|(e, t)| (e, t.clone()))
            .collect();
        let lua = self.vm.lua();
        if let Some(mut mail) = lua.app_data_mut::<Mailbox>() {
            mail.messages.clear();
            mail.later.clear();
            mail.timers.clear();
            for (e, timers) in &saved {
                for t in &timers.timers {
                    let Some(c) = self.components.iter().find(|c| c.name == t.component) else {
                        continue;
                    };
                    let Ok(hook) = lua.create_string(&t.hook) else {
                        continue;
                    };
                    let mut values = [Value::Nil, Value::Nil, Value::Nil, Value::Nil];
                    let count = t.values.len().min(values.len());
                    for (slot, v) in values.iter_mut().zip(&t.values) {
                        *slot = v.value(lua);
                    }
                    mail.seq += 1;
                    let (due, seq) = (now + t.ticks.max(1), mail.seq);
                    let at = mail
                        .timers
                        .partition_point(|x| (x.due, x.seq) <= (due, seq));
                    mail.timers.insert(
                        at,
                        Timer {
                            due,
                            seq,
                            entity: *e,
                            id: c.id,
                            call: Call {
                                hook,
                                count,
                                values,
                            },
                        },
                    );
                }
            }
        }
        MAIL.set(false);
        for (e, _) in saved {
            world.remove::<ScriptTimers>(e);
        }
        self.loaded = true;
    }

    /// Every instance's `on_load` (its `on_start` first if it has not started), in the
    /// instances' order, after a save was applied.
    fn call_loaded(&mut self) {
        let mut order = core::mem::take(&mut self.order);
        order.clear();
        order.extend(self.instances.keys().copied());
        for &key in &order {
            let k = key.1 as usize;
            let Some((e, inst)) = self.instances.get_mut(&key) else {
                continue;
            };
            if inst.stopped {
                continue;
            }
            let (e, this) = (*e, inst.this.clone());
            let started = core::mem::replace(&mut inst.started, true);
            let c = &self.components[k];
            let (start, load) = (c.hooks.start.clone(), c.hooks.load.clone());
            if !started
                && let Some(start) = start
                && let Err(err) = call_for(&self.vm, e, c, Hook::Start, &start, this.clone())
            {
                self.stop(key, "on_start", e, &err);
                continue;
            }
            let c = &self.components[k];
            if let Some(load) = load
                && let Err(err) = call_for(&self.vm, e, c, Hook::Load, &load, this)
            {
                self.stop(key, "on_load", e, &err);
            }
            self.deliver();
        }
        self.order = order;
    }

    /// The messages scripts sent, delivered (ADR 0114 §7): each, in the order sent, to every
    /// script of its entity whose component has a function of its hook's name. Those sent
    /// while delivering go in the same pass, up to 16 passes; the rest wait for the next
    /// stage, and the log says so once.
    fn deliver(&mut self) {
        if !MAIL.get() {
            return;
        }
        for _ in 0..16 {
            let mut batch = core::mem::take(&mut self.letters);
            if let Some(mut mail) = self.vm.lua().app_data_mut::<Mailbox>() {
                core::mem::swap(&mut batch, &mut mail.messages);
            }
            MAIL.set(false);
            if batch.is_empty() {
                self.letters = batch;
                return;
            }
            for m in batch.drain(..) {
                self.receive(m.target, None, &m.call);
            }
            self.letters = batch;
        }
        if MAIL.get() {
            if let Some(mut mail) = self.vm.lua().app_data_mut::<Mailbox>() {
                let Mailbox {
                    messages, later, ..
                } = &mut *mail;
                later.append(messages);
            }
            MAIL.set(false);
            let line = String::from(
                "scripts: messages still sent after 16 passes (do scripts send to each other in a loop?); the rest wait for the next stage",
            );
            if self.reported.insert(line.clone()) {
                log::warn!(target: "script", "{line}");
            }
        }
    }

    /// The messages left by the last stage's passes, delivered first in this one.
    fn deliver_later(&mut self) {
        if let Some(mut mail) = self.vm.lua().app_data_mut::<Mailbox>()
            && !mail.later.is_empty()
        {
            let Mailbox {
                messages, later, ..
            } = &mut *mail;
            core::mem::swap(messages, later);
            messages.append(later);
            MAIL.set(true);
        }
        self.deliver();
    }

    /// The timers due at this tick, called in the order due and set, each one's messages
    /// delivered when it returns.
    fn timers_due(&mut self) {
        let now = self.clock.ticks;
        let mut due = core::mem::take(&mut self.due);
        if let Some(mut mail) = self.vm.lua().app_data_mut::<Mailbox>() {
            let n = mail.timers.partition_point(|t| t.due <= now);
            due.extend(mail.timers.drain(..n));
        }
        for t in due.drain(..) {
            if let Some(k) = self.components.iter().position(|c| c.id == t.id) {
                self.receive(t.entity, Some(k), &t.call);
                self.deliver();
            }
        }
        self.due = due;
    }

    /// `call` on the scripts of `target` (only component `only`'s, for a timer) whose
    /// component has its hook: started first if new. A message no script of a living
    /// entity hears is said once.
    fn receive(&mut self, target: Entity, only: Option<usize>, call: &Call) {
        let world = open_world();
        // SAFETY: the stage's world (`open_world`), read only here.
        if world.is_null() || !unsafe { &*world }.contains(target) {
            return;
        }
        let mut keys = core::mem::take(&mut self.receivers);
        keys.clear();
        keys.extend(
            self.instances
                .range((target.index(), 0)..=(target.index(), u32::MAX))
                .map(|(&key, _)| key)
                .filter(|key| only.is_none_or(|k| key.1 as usize == k)),
        );
        let mut heard = false;
        for &key in &keys {
            let k = key.1 as usize;
            let Some((e, inst)) = self.instances.get_mut(&key) else {
                continue;
            };
            if *e != target || inst.stopped {
                continue;
            }
            let Ok(Value::Function(hook)) = self.components[k].table.raw_get::<Value>(&call.hook)
            else {
                continue;
            };
            heard = true;
            let this = inst.this.clone();
            let started = core::mem::replace(&mut inst.started, true);
            let owner = &self.components[k];
            if !started
                && let Some(start) = owner.hooks.start.clone()
                && let Err(err) =
                    call_for(&self.vm, target, owner, Hook::Start, &start, this.clone())
            {
                self.stop(key, "on_start", target, &err);
                continue;
            }
            let owner = &self.components[k];
            let [a, b, c, d] = &call.values;
            let (vm, which) = (&self.vm, Hook::Message(&call.hook));
            let result = match call.count {
                0 => call_for(vm, target, owner, which, &hook, this),
                1 => call_for(vm, target, owner, which, &hook, (this, a)),
                2 => call_for(vm, target, owner, which, &hook, (this, a, b)),
                3 => call_for(vm, target, owner, which, &hook, (this, a, b, c)),
                _ => call_for(vm, target, owner, which, &hook, (this, a, b, c, d)),
            };
            if let Err(err) = result {
                let name = call.hook.to_string_lossy();
                self.stop(key, &name, target, &err);
            }
        }
        self.receivers = keys;
        if !heard && only.is_none() {
            // SAFETY: as above, read again after the hooks (a stop opens it anew).
            let who = unsafe { &*open_world() }
                .get::<engine_scene::Name>(target)
                .map_or_else(|| format!("{target:?}"), |n| n.as_str().to_owned());
            let line = format!(
                "scripts: a message `{}` reached {who}, and none of its scripts has it",
                call.hook.to_string_lossy()
            );
            if self.reported.insert(line.clone()) {
                log::warn!(target: "script", "{line}");
            }
        }
    }

    /// Instance `key` stopped by `err` in hook `hook`. A call the debugger ended (Play
    /// stopped at a stop) is no error of the script's.
    fn stop(&mut self, key: (u32, u32), hook: &str, e: Entity, err: &ScriptError) {
        if matches!(err, ScriptError::Aborted { .. }) {
            return;
        }
        if let Some((_, inst)) = self.instances.get_mut(&key) {
            inst.stopped = true;
        }
        self.report(key.1 as usize, hook, e, err);
    }

    /// `err` in component `k`'s `hook` on `e`, in the log once.
    fn report(&mut self, k: usize, hook: &str, e: Entity, err: &ScriptError) {
        if matches!(err, ScriptError::Aborted { .. }) {
            return;
        }
        let c = &self.components[k];
        let place = self.place(err);
        let line = format!("{place} ({}.{hook})", c.name);
        if self.reported.insert(line.clone()) {
            log::error!(target: "script", "{line}, stopped on entity {e:?}");
        }
    }

    /// `err` with its module's file in place of the module's name.
    fn place(&self, err: &ScriptError) -> String {
        match err {
            ScriptError::Compile {
                module,
                line,
                message,
            }
            | ScriptError::Runtime {
                module,
                line,
                message,
            } => {
                let at = line.map_or(String::new(), |l| format!(":{l}"));
                format!("{}{at}: {message}", self.file(module))
            }
            ScriptError::Timeout { module } => {
                format!("{}: ran past its time budget", self.file(module))
            }
            other => other.to_string(),
        }
    }

    fn file(&self, module: &str) -> String {
        self.files.get(module).map_or_else(
            || module.to_owned(),
            |f| f.to_string_lossy().replace('\\', "/"),
        )
    }
}

impl Drop for Scripts {
    fn drop(&mut self) {
        // Before the VM closes (the fields drop after this): its queries' references go
        // with it, and its closing collects queries without leaving any.
        script_views::vm_gone(self.main);
    }
}

/// Which hooks a stage calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Tick,
    Frame,
}

/// Calls hook `f` of `module` for the instance of entity `e` with `args`, the debugger told
/// whose hook runs (ADR 0119 E9-4a): a stop in it names the entity, and a breakpoint
/// limited to other entities passes. A thread-local written twice: nothing measurable.
fn call_for(
    vm: &ScriptVm,
    e: Entity,
    c: &Declared,
    hook: Hook<'_>,
    f: &Function,
    args: impl IntoLuaMulti,
) -> Result<(), ScriptError> {
    call_hook(
        vm,
        Some(e),
        (&c.name, &c.module, &c.profiled),
        hook,
        f,
        args,
    )
}

/// Calls `f` with `args`, the hook `hook` of `owner` (its name, its module, its profile),
/// for entity `e`'s instance or a system (`None`): the debugger told whose hook runs (ADR
/// 0119 E9-4a), and the profiler (ADR 0114 §11) given a Tracy zone while Tracy runs, and
/// the hook's time and memory while the editor's profiler watches. Nothing more is paid
/// while neither does.
fn call_hook(
    vm: &ScriptVm,
    e: Option<Entity>,
    (name, module, profiled): (&str, &str, &Profiled),
    hook: Hook<'_>,
    f: &Function,
    args: impl IntoLuaMulti,
) -> Result<(), ScriptError> {
    let _zone = engine_profiler::running().then(|| profiled.place(name, module, hook).zone());
    let watched = PROFILING.get().then(|| {
        (
            CATEGORY.replace(profiled.category),
            Instant::now(),
            engine_script::waited(),
        )
    });
    if watched.is_some() {
        vm.set_memory_category(profiled.category);
    }
    let result = match e {
        Some(e) => {
            engine_script::with_instance(e.to_bits(), || vm.call_function::<_, ()>(module, f, args))
        }
        None => vm.call_function::<_, ()>(module, f, args),
    };
    if let Some((before, since, waited)) = watched {
        CATEGORY.set(before);
        vm.set_memory_category(before);
        let spent = u64::try_from(since.elapsed().as_nanos())
            .unwrap_or(u64::MAX)
            .saturating_sub(engine_script::waited() - waited);
        let (calls, nanos) = profiled.spent.get();
        profiled.spent.set((calls + 1, nanos.saturating_add(spent)));
    }
    result
}

/// The module a project's script file is loaded as, from its path relative to the
/// project (`scripts/npc/guard.luau` is `npc/guard`): what the editor sets its breakpoints
/// in before the host loads it. None for a file outside `scripts/` or not a `.luau`.
pub fn module_name(file: &Path) -> Option<String> {
    if file.extension().is_none_or(|e| e != "luau") {
        return None;
    }
    module_of(Path::new("scripts"), file).ok()
}

/// The file of script module `module`, relative to the project (`scripts/npc/guard.luau`):
/// what [`module_name`] reads back.
pub fn module_file(module: &str) -> String {
    format!("scripts/{module}.luau")
}

/// The module of script `file` under `folder`: its path from there, without `.luau`
/// (`npc/guard`).
fn module_of(folder: &Path, file: &Path) -> Result<String, String> {
    let relative = file
        .strip_prefix(folder)
        .map_err(|_| format!("{} is not in {}", file.display(), folder.display()))?;
    Ok(relative
        .with_extension("")
        .to_string_lossy()
        .replace('\\', "/"))
}

/// Component `name`'s description from its props, laid out by name (a table's keys have
/// no order): its type, its default value (its texts empty), its texts and its layout.
#[allow(clippy::type_complexity)]
fn describe(
    name: &str,
    props: &mut [Prop],
) -> Result<
    (
        &'static TypeInfo,
        Vec<u8>,
        &'static [engine_ecs::TextField],
        std::alloc::Layout,
    ),
    String,
> {
    props.sort_by(|a, b| a.name.cmp(&b.name));
    let mut offset = 0usize;
    let mut align = 1usize;
    for p in props.iter_mut() {
        let info = p.kind.info();
        offset = offset.next_multiple_of(info.align);
        p.offset = offset;
        offset += info.size;
        align = align.max(info.align);
    }
    let size = offset.next_multiple_of(align);
    let fields: Vec<Field> = props
        .iter()
        .map(|p| Field {
            name: leak(&p.name),
            offset: p.offset,
            ty: p.kind.info(),
            attrs: p.attrs,
        })
        .collect();
    let info: &'static TypeInfo = Box::leak(Box::new(TypeInfo {
        name: leak(name),
        size,
        align,
        kind: TypeKind::Struct(Box::leak(fields.into_boxed_slice())),
        attrs: Attrs::NONE,
    }));
    let mut defaults = vec![0u8; size];
    for p in props.iter() {
        write_default(&mut defaults[p.offset..], p.kind, &p.default);
    }
    let texts: Vec<engine_ecs::TextField> = props
        .iter()
        .filter(|p| p.kind == Kind::Text)
        .map(|p| engine_ecs::TextField {
            offset: p.offset,
            default: match &p.default {
                Value::String(s) => leak(&s.to_string_lossy()),
                _ => "",
            },
        })
        .collect();
    let texts: &'static [engine_ecs::TextField] = Box::leak(texts.into_boxed_slice());
    let layout = std::alloc::Layout::from_size_align(size, align).map_err(|e| e.to_string())?;
    Ok((info, defaults, texts, layout))
}

/// Whether two descriptions of a component keep its props in the same places: the same
/// names at the same offsets, of types of one name and size.
fn same_places(a: &TypeInfo, b: &TypeInfo) -> bool {
    let (TypeKind::Struct(x), TypeKind::Struct(y)) = (&a.kind, &b.kind) else {
        return false;
    };
    a.size == b.size
        && a.align == b.align
        && x.len() == y.len()
        && x.iter().zip(y.iter()).all(|(f, g)| {
            f.name == g.name
                && f.offset == g.offset
                && f.ty.name == g.ty.name
                && f.ty.size == g.ty.size
        })
}

/// The hooks on a component's table.
fn hooks_of(table: &Table) -> Hooks {
    let hook = |key: &str| table.get::<Option<Function>>(key).ok().flatten();
    // A function of its own (a message's, a timer's hook) makes instances too.
    let handlers = table
        .pairs::<Value, Value>()
        .flatten()
        .any(|(_, v)| matches!(v, Value::Function(_)));
    Hooks {
        start: hook("on_start"),
        tick: hook("on_tick"),
        frame: hook("on_frame"),
        stop: hook("on_stop"),
        touch: hook("on_touch"),
        untouch: hook("on_untouch"),
        load: hook("on_load"),
        handlers,
    }
}

/// Every entity's value of `r.old` moved into `r.new` ([`Relayout::take`]): its texts
/// move with it, and the old value drops what did not.
fn move_values(world: &mut World, r: &Relayout) {
    let entities: Vec<Entity> = world
        .archetypes()
        .iter()
        .filter(|a| a.components().contains(&r.old))
        .flat_map(|a| (0..a.chunk_count()).flat_map(move |c| a.chunk_entities(c).iter().copied()))
        .collect();
    let mut moved = vec![0u8; r.defaults.len()];
    for e in entities {
        let Some(ptr) = world.get_raw_mut(e, r.old) else {
            continue;
        };
        // SAFETY: the entity's live value of the old layout, and room for one of the new,
        // which `insert_raw` moves into the world (the room's bytes are written over next,
        // never dropped); removing the old value drops what it kept.
        unsafe {
            r.take(ptr.as_ptr(), moved.as_mut_ptr());
            world.insert_raw(e, r.new, moved.as_ptr());
        }
        world.remove_raw(e, r.old);
    }
}

/// The `.luau` files under `dir`, hidden ones and definition files (`.d.luau`, types to
/// read, not code to run) left out.
fn collect_scripts(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            collect_scripts(&path, out);
        } else if path.extension().is_some_and(|x| x == "luau")
            && !entry.file_name().to_string_lossy().ends_with(".d.luau")
        {
            out.push(path);
        }
    }
}

fn leak(s: &str) -> &'static str {
    Box::leak(s.to_owned().into_boxed_str())
}

/// Writes a prop's default into `out` (its bytes from its offset on).
fn write_default(out: &mut [u8], kind: Kind, value: &Value) {
    let number = |v: &Value| match v {
        Value::Number(n) => *n,
        Value::Integer(i) => *i as f64,
        _ => 0.0,
    };
    match kind {
        Kind::Number => out[..4].copy_from_slice(&(number(value) as f32).to_le_bytes()),
        Kind::Integer => out[..4].copy_from_slice(&(number(value) as i32).to_le_bytes()),
        Kind::Boolean => out[0] = u8::from(matches!(value, Value::Boolean(true))),
        Kind::Vector => {
            if let Value::Vector(v) = value {
                for (i, c) in [v.x(), v.y(), v.z()].into_iter().enumerate() {
                    out[i * 4..i * 4 + 4].copy_from_slice(&c.to_le_bytes());
                }
            }
        }
        Kind::Entity => out[..8].copy_from_slice(&Entity::NULL.to_bits().to_le_bytes()),
        // `AssetRef::NONE` is all zeros, as the defaults start.
        Kind::Asset => {}
        // An empty `String`, which owns nothing (any copy of it is one too): the default
        // text is written into each value ([`engine_ecs::RuntimeType::write_default`]).
        Kind::Text => {
            let room = &mut out[..core::mem::size_of::<String>()];
            // SAFETY: room for a `String`, written unaligned, as bytes are.
            unsafe {
                room.as_mut_ptr()
                    .cast::<String>()
                    .write_unaligned(String::new());
            }
        }
    }
}

/// `value` (a function, a table) in the registry, by a reference the fast path pushes it
/// by.
fn registry_ref(lua: &Lua, value: impl mlua::IntoLuaMulti) -> mlua::Result<std::ffi::c_int> {
    let mut made = 0;
    // SAFETY: the value is the one argument pushed; the reference keeps it.
    unsafe {
        lua.exec_raw::<()>(value, |state| {
            made = mlua::ffi::lua_ref(state, -1);
        })?;
    }
    Ok(made)
}

/// C function `f`, named `name` in tracebacks, in the registry: made once, pushed by its
/// reference.
fn c_function_ref(
    lua: &Lua,
    f: mlua::ffi::lua_CFunction,
    name: &'static std::ffi::CStr,
) -> mlua::Result<std::ffi::c_int> {
    let mut made = 0;
    // SAFETY: the function is pushed and kept by the reference.
    unsafe {
        lua.exec_raw::<()>((), |state| {
            mlua::ffi::lua_pushcfunctiond(state, f, name.as_ptr());
            made = mlua::ffi::lua_ref(state, -1);
        })?;
    }
    Ok(made)
}

/// Lets go of registry reference `r`.
fn unref(lua: &Lua, r: std::ffi::c_int) {
    // SAFETY: a reference the host took with `lua_ref`.
    let _ = unsafe {
        lua.exec_raw::<()>((), |state| {
            mlua::ffi::lua_unref(state, r);
        })
    };
}

/// `e:add(Component, values?)` (ADR 0114 §3): the component's default given to the entity
/// if it has none, then `values` written into it, field by field. At once: the next line
/// reads it. A script component's instance starts at the next stage.
fn entity_add(
    lua: &Lua,
    (target, component, values): (Value, Value, Option<Table>),
) -> mlua::Result<()> {
    let Some(e) = script_views::value_entity(&target) else {
        return Err(mlua::Error::runtime("add: not an entity"));
    };
    let takes =
        || mlua::Error::runtime("add takes a component (`Transform`, a script's) or its name");
    let name = match &component {
        Value::Table(t) => t.raw_get::<mlua::LuaString>("name").map_err(|_| takes())?,
        Value::String(s) => s.clone(),
        _ => return Err(takes()),
    };
    let name = name.to_str()?;
    let Some((id, info)) = named(lua, &name) else {
        return Err(mlua::Error::runtime(format!(
            "add: no component is called `{name}`"
        )));
    };
    if matches!(info.name, "Parent" | "Children") {
        return Err(mlua::Error::runtime(
            "`Parent` and `Children` keep the hierarchy: they are not added",
        ));
    }
    // SAFETY: the host's world for this stage (`world_ptr`), used within this callback only.
    let w = unsafe { &mut *world_ptr(lua)? };
    if !w.contains(e) {
        return Err(mlua::Error::runtime("add: the entity is gone"));
    }
    if w.get_raw(e, id).is_none() {
        // Scene files' names, which the registry goes by.
        let made = lua
            .app_data_ref::<engine_scene::SceneRegistry>()
            .is_some_and(|r| r.insert_default(w, e, info.name));
        if !made {
            return Err(mlua::Error::runtime(format!(
                "`{name}` cannot be added from a script"
            )));
        }
    }
    if let Some(values) = values {
        for pair in values.pairs::<mlua::LuaString, Value>() {
            let (key, value) = pair?;
            let f = struct_field(info, &key.to_str()?)?;
            set_field(w, e, id, info, 0, f, value)?;
        }
    }
    Ok(())
}

/// Whether `name` can be a global naming a component: a capital, then letters, digits and
/// `_`.
fn global_name(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_uppercase())
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// The host's libraries, before the sandbox seals the globals.
fn install(lua: &Lua) -> mlua::Result<()> {
    // The fast path's atoms and metatables (ADR 0114 E7-1c), and the functions it hands
    // the rest to.
    // SAFETY: the VM's state; installed once, before any script runs.
    unsafe { lua.exec_raw::<()>((), |state| script_views::install(state))? };
    let slow = script_views::Slow {
        self_index: registry_ref(lua, lua.create_function(self_index_slow)?)?,
        self_newindex: registry_ref(lua, lua.create_function(self_newindex_slow)?)?,
        view_index: registry_ref(lua, lua.create_function(view_index_slow)?)?,
        view_newindex: registry_ref(lua, lua.create_function(view_newindex_slow)?)?,
        add: registry_ref(lua, lua.create_function(entity_add)?)?,
        send: registry_ref(lua, lua.create_function(entity_send)?)?,
        after: registry_ref(lua, lua.create_function(self_after)?)?,
        children: c_function_ref(lua, script_views::children_next, c"children")?,
        query_next: c_function_ref(lua, script_views::query_next, c"query")?,
    };
    lua.set_app_data(slow);
    let globals = lua.globals();
    // Components as values (ADR 0114 §3): `Transform` and every other component of the
    // program is a table of its name, as `has`, `add` and `remove` take them; a script's
    // `component` table is one too. The sandbox makes them read-only.
    for t in engine_ecs::reflected_types() {
        let name = t.info.name;
        if matches!(t.kind, engine_ecs::ReflectedKind::Component { .. })
            && !t.info.attrs.hidden
            && !t.info.attrs.skip
            && global_name(name)
        {
            let c = lua.create_table()?;
            c.set("name", name)?;
            globals.set(name, c)?;
        }
    }
    globals.set("component", lua.create_function(declare_component)?)?;
    let prop = lua.create_table()?;
    for (name, kind) in [
        ("number", "number"),
        ("integer", "integer"),
        ("boolean", "boolean"),
        ("vector", "vector"),
        ("entity", "entity"),
        ("asset", "asset"),
        ("string", "string"),
    ] {
        prop.set(
            name,
            lua.create_function(move |lua, (default, opts): (Value, Option<Table>)| {
                let t = lua.create_table()?;
                t.set("__prop", kind)?;
                t.set("default", default)?;
                t.set("opts", opts)?;
                Ok(t)
            })?,
        )?;
    }
    globals.set("prop", prop)?;
    let world = lua.create_table()?;
    // SAFETY: a C function of the fast path's, which checks its argument.
    world.set("find", unsafe {
        lua.create_c_function(script_views::world_find)?
    })?;
    // SAFETY: as `find`.
    world.set("query", unsafe {
        lua.create_c_function(script_views::world_query)?
    })?;
    world.set("spawn", lua.create_function(world_spawn)?)?;
    world.set("instantiate", lua.create_function(world_instantiate)?)?;
    globals.set("world", world)?;
    globals.set("system", lua.create_function(declare_system)?)?;
    // The game's input (ADR 0114 §4), C functions: a name read through mlua allocates.
    let input = lua.create_table()?;
    let reads: [(&str, mlua::ffi::lua_CFunction); 6] = [
        ("pressed", script_views::input_pressed),
        ("just_pressed", script_views::input_just_pressed),
        ("just_released", script_views::input_just_released),
        ("strength", script_views::input_strength),
        ("axis", script_views::input_axis),
        ("vector", script_views::input_vector),
    ];
    for (name, f) in reads {
        // SAFETY: C functions of the fast path's, which check their arguments.
        input.set(name, unsafe { lua.create_c_function(f)? })?;
    }
    globals.set("input", input)?;
    // The world's generator (ADR 0114 §4), C functions too.
    let random = lua.create_table()?;
    let draws: [(&str, mlua::ffi::lua_CFunction); 3] = [
        ("number", script_views::random_number),
        ("integer", script_views::random_integer),
        ("vector_in_sphere", script_views::random_in_sphere),
    ];
    for (name, f) in draws {
        // SAFETY: C functions of the fast path's, which check their arguments.
        random.set(name, unsafe { lua.create_c_function(f)? })?;
    }
    globals.set("random", random)?;
    // Sounds (ADR 0114 §4, ADR 0105): entities that play once and go.
    let audio = lua.create_table()?;
    audio.set("play", lua.create_function(audio_play)?)?;
    audio.set("play_at", lua.create_function(audio_play_at)?)?;
    globals.set("audio", audio)?;
    // The session (ADR 0114 §4, §8).
    let net = lua.create_table()?;
    let asks: [(&str, mlua::ffi::lua_CFunction); 3] = [
        ("is_server", script_views::net_is_server),
        ("is_client", script_views::net_is_client),
        ("resimulating", script_views::net_resimulating),
    ];
    for (name, f) in asks {
        // SAFETY: C functions of the fast path's, which take no arguments.
        net.set(name, unsafe { lua.create_c_function(f)? })?;
    }
    globals.set("net", net)?;
    let physics = lua.create_table()?;
    physics.set("raycast", lua.create_function(raycast)?)?;
    physics.set("overlap_sphere", lua.create_function(overlap_sphere)?)?;
    physics.set(
        "filter",
        lua.create_function(|_, layers: Table| {
            let names = layers
                .sequence_values::<String>()
                .collect::<mlua::Result<Vec<_>>>()?;
            Ok(PhysicsFilter {
                names,
                resolved: core::cell::Cell::new(None),
            })
        })?,
    )?;
    globals.set("physics", physics)?;
    let time = lua.create_table()?;
    time.set(
        "now",
        lua.create_function(|lua, ()| {
            let c = lua
                .app_data_ref::<Clock>()
                .map_or_else(Clock::default, |c| *c);
            Ok(f64::from(c.dt) * c.ticks as f64)
        })?,
    )?;
    time.set(
        "tick",
        lua.create_function(|lua, ()| Ok(lua.app_data_ref::<Clock>().map_or(0, |c| c.ticks)))?,
    )?;
    time.set(
        "dt",
        lua.create_function(|lua, ()| Ok(lua.app_data_ref::<Clock>().map_or(0.0, |c| c.dt)))?,
    )?;
    globals.set("time", time)?;
    let quat = lua.create_table()?;
    quat.set(
        "identity",
        lua.create_function(|_, ()| Ok(ScriptQuat(Quat::IDENTITY)))?,
    )?;
    quat.set(
        "axis_angle",
        lua.create_function(|_, (axis, angle): (Vector, f32)| {
            let axis = Vec3::new(axis.x(), axis.y(), axis.z()).normalize_or_zero();
            Ok(ScriptQuat(Quat::from_axis_angle(axis, angle)))
        })?,
    )?;
    quat.set(
        "euler",
        lua.create_function(|_, (x, y, z): (f32, f32, f32)| {
            Ok(ScriptQuat(Quat::from_euler(
                engine_core::math::EulerRot::YXZ,
                y,
                x,
                z,
            )))
        })?,
    )?;
    globals.set("quat", quat)?;
    globals.set(
        "print",
        lua.create_function(|lua, args: mlua::Variadic<Value>| {
            log::info!(target: "script", "{}: {}", here(lua), joined(&args));
            Ok(())
        })?,
    )?;
    globals.set(
        "warn",
        lua.create_function(|lua, args: mlua::Variadic<Value>| {
            log::warn!(target: "script", "{}: {}", here(lua), joined(&args));
            Ok(())
        })?,
    )?;
    Ok(())
}

/// The calling script's file and line (`scripts/door.luau:14`).
fn here(lua: &Lua) -> String {
    let (module, line) = lua
        .inspect_stack(1, |d| {
            let source = d.source();
            let module = source
                .source
                .as_deref()
                .map_or_else(String::new, |s| s.trim_start_matches(['=', '@']).to_owned());
            (module, d.current_line())
        })
        .unwrap_or_default();
    let file = lua
        .app_data_ref::<Files>()
        .and_then(|f| f.0.get(&module).cloned())
        .unwrap_or_else(|| format!("{module}.luau"));
    match line {
        Some(line) => format!("{file}:{line}"),
        None => file,
    }
}

fn joined(args: &[Value]) -> String {
    let mut out = String::new();
    for (i, a) in args.iter().enumerate() {
        if i > 0 {
            out.push('\t');
        }
        match a {
            Value::String(s) => out.push_str(&s.to_string_lossy()),
            other => out.push_str(&other.to_string().unwrap_or_else(|_| format!("{other:?}"))),
        }
    }
    out
}

/// `component(name, props)`: the component's table, its props remembered for the host.
fn declare_component(
    lua: &Lua,
    (name, props): (mlua::LuaString, Option<Table>),
) -> mlua::Result<Table> {
    let name = name.to_str()?.to_owned();
    if !name.starts_with(|c: char| c.is_ascii_uppercase()) {
        return Err(mlua::Error::runtime(format!(
            "component `{name}`: its name starts with a capital, as components' do"
        )));
    }
    let mut list = Vec::new();
    if let Some(props) = &props {
        for pair in props.pairs::<String, Value>() {
            let (key, value) = pair?;
            list.push(prop_of(&name, key, value)?);
        }
    }
    let table = lua.create_table()?;
    table.set("props", props.unwrap_or(lua.create_table()?))?;
    table.set("name", name.as_str())?;
    let Some(mut loading) = lua.app_data_mut::<Loading>() else {
        return Err(mlua::Error::runtime(
            "component() declares at a script's top, while it loads",
        ));
    };
    loading.declared.push((name, table.clone(), list));
    Ok(table)
}

/// `audio.play(sound, bus?)` (ADR 0114 §4): an entity playing `sound` once as it is (no
/// place: the UI, a stinger) on `bus` (`ui` by default), despawned at its end; the entity.
fn audio_play(
    lua: &Lua,
    (sound, bus): (mlua::LuaString, Option<mlua::LuaString>),
) -> mlua::Result<Value> {
    // SAFETY: the host's world for this stage (`world_ptr`), used within this callback only.
    let w = unsafe { &mut *world_ptr(lua)? };
    // A tick run again on a client played it the first time (ADR 0114 §8).
    if engine_net::Resimulating::now(w) {
        return Ok(Value::Nil);
    }
    let bus = bus.as_ref().map(mlua::LuaString::to_str).transpose()?;
    let source = crate::AudioSource::new(&sound.to_str()?)
        .on_bus(bus.as_deref().unwrap_or("ui"))
        .despawned_at_end();
    Ok(script_views::entity_value(w.spawn((source,))))
}

/// `audio.play_at(sound, position, bus?)` (ADR 0114 §4): an entity at `position` playing
/// `sound` once in the world (fainter with distance, behind walls muffled) on `bus`
/// (`effects` by default), despawned at its end; the entity.
fn audio_play_at(
    lua: &Lua,
    (sound, at, bus): (mlua::LuaString, Vector, Option<mlua::LuaString>),
) -> mlua::Result<Value> {
    // SAFETY: the host's world for this stage (`world_ptr`), used within this callback only.
    let w = unsafe { &mut *world_ptr(lua)? };
    if engine_net::Resimulating::now(w) {
        return Ok(Value::Nil);
    }
    let bus = bus.as_ref().map(mlua::LuaString::to_str).transpose()?;
    let mut source = crate::AudioSource3D::new(&sound.to_str()?).despawned_at_end();
    if let Some(bus) = bus.as_deref() {
        source = source.on_bus(bus);
    }
    let at = Vec3::new(at.x(), at.y(), at.z());
    let e = w.spawn((
        engine_ecs::Transform::from_translation(at),
        engine_ecs::GlobalTransform(engine_core::math::Affine3A::from_translation(at)),
        source,
    ));
    Ok(script_views::entity_value(e))
}

/// `system(name, hooks)`: a system of module that runs it (ADR 0114 §1), its hooks
/// (`on_tick`, `on_frame`) on `hooks`.
fn declare_system(lua: &Lua, (name, hooks): (mlua::LuaString, Table)) -> mlua::Result<()> {
    let name = name.to_str()?.to_owned();
    let Some(mut loading) = lua.app_data_mut::<Loading>() else {
        return Err(mlua::Error::runtime(
            "system() declares at a script's top, while it loads",
        ));
    };
    if loading.systems.iter().any(|(n, _)| *n == name) {
        return Err(mlua::Error::runtime(format!(
            "system `{name}` is declared twice"
        )));
    }
    loading.systems.push((name, hooks));
    Ok(())
}

/// The project's assets as scripts reach them (ADR 0114 E7-5e3): its folder, and the paths
/// of the assets found so far by their ids.
struct ProjectAssets {
    root: PathBuf,
    paths: HashMap<engine_scene::AssetId, String>,
}

impl ProjectAssets {
    /// The path of asset file `id`, relative to the project: the editor's index of the
    /// project's assets, read only; else the headers of its scene files, read once (the
    /// prefabs scripts place are scenes).
    fn path_of(&mut self, id: engine_scene::AssetId) -> Option<String> {
        if let Some(path) = self.paths.get(&id) {
            return Some(path.clone());
        }
        let db = engine_scene::AssetDatabase::open(&self.root);
        for e in db.entries() {
            self.paths.entry(e.id).or_insert_with(|| e.path.clone());
        }
        if !self.paths.contains_key(&id) {
            let mut files = Vec::new();
            collect_scenes(&self.root, &self.root, &mut files);
            for path in files {
                if let Ok(text) = std::fs::read_to_string(self.root.join(&path))
                    && let Ok((_, Some(found))) = engine_scene::scene_header(&text)
                {
                    self.paths.entry(found).or_insert(path);
                }
            }
        }
        self.paths.get(&id).cloned()
    }
}

/// The scene files under `dir` (inside the project `root`), by their paths from it, with
/// `/`; the editor's own folders left out.
fn collect_scenes(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let hidden = entry.file_name().to_string_lossy().starts_with('.');
        if path.is_dir() {
            if !hidden && entry.file_name() != "target" {
                collect_scenes(root, &path, out);
            }
        } else if path.extension().is_some_and(|e| e == "scene")
            && let Ok(relative) = path.strip_prefix(root)
        {
            out.push(relative.to_string_lossy().replace('\\', "/"));
        }
    }
}

/// `world.instantiate(prefab, translation?)` (ADR 0114 §3, ADR 0108): the prefab scene's
/// entities placed in the world, at once, its roots moved by `translation`; the entity of
/// its root (a prefab of several roots gets one of its own, the roots its children).
/// `prefab` is an asset (`prop.asset("scene")`) or the scene's path in the project.
/// Instances made while the game runs are anonymous, as `world.spawn`'s entities are: no
/// `SceneId`, so two of one prefab never share one; saves keep them all the same.
fn world_instantiate(lua: &Lua, (prefab, at): (Value, Option<Vector>)) -> mlua::Result<Value> {
    let path = match &prefab {
        Value::String(s) => s.to_str()?.to_owned(),
        Value::UserData(_) => {
            let mut found = None;
            // SAFETY: the value is the one argument pushed, read where it lies.
            unsafe {
                lua.exec_raw::<()>(prefab.clone(), |state| {
                    found = script_views::asset_at(state, -1);
                })?;
            }
            let Some(asset) = found else {
                return Err(mlua::Error::runtime(
                    "world.instantiate takes a prefab: an asset (prop.asset) or a scene's path",
                ));
            };
            let path = lua
                .app_data_mut::<ProjectAssets>()
                .and_then(|mut a| a.path_of(asset.file))
                .ok_or_else(|| {
                    mlua::Error::runtime(format!(
                        "world.instantiate: no scene file of the project is {}",
                        asset.file
                    ))
                })?;
            script_views::name_asset(asset, &path);
            path
        }
        _ => {
            return Err(mlua::Error::runtime(
                "world.instantiate takes a prefab: an asset (prop.asset) or a scene's path",
            ));
        }
    };
    let root = lua
        .app_data_ref::<ProjectAssets>()
        .map(|a| a.root.clone())
        .ok_or_else(|| mlua::Error::runtime("world.instantiate: no project"))?;
    let text = std::fs::read_to_string(root.join(&path))
        .map_err(|e| mlua::Error::runtime(format!("world.instantiate: {path}: {e}")))?;
    // SAFETY: the host's world for this stage (`world_ptr`), used within this callback only.
    let w = unsafe { &mut *world_ptr(lua)? };
    let report = {
        let Some(registry) = lua.app_data_ref::<engine_scene::SceneRegistry>() else {
            return Err(mlua::Error::runtime("world.instantiate: no scene registry"));
        };
        let files = engine_scene::PrefabFiles(&root);
        let options = engine_scene::LoadOptions {
            prefabs: Some(&files),
        };
        engine_scene::load_with(w, &registry, &text, &options)
            .map_err(|e| mlua::Error::runtime(format!("world.instantiate: {path}:{e}")))?
    };
    // Anonymous: what scene files name their entities by goes.
    for &e in &report.entities {
        w.remove::<engine_scene::SceneId>(e);
        w.remove::<engine_scene::PrefabInstance>(e);
        w.remove::<engine_scene::KeptBlocks>(e);
    }
    let roots: Vec<Entity> = report
        .entities
        .iter()
        .copied()
        .filter(|&e| w.parent(e).is_none())
        .collect();
    let offset = at.map_or(Vec3::ZERO, |v| Vec3::new(v.x(), v.y(), v.z()));
    let root = match roots.as_slice() {
        [] => {
            return Err(mlua::Error::runtime(format!(
                "world.instantiate: {path} is empty"
            )));
        }
        [one] => *one,
        many => {
            let parent = w.spawn((
                engine_ecs::Transform::IDENTITY,
                engine_ecs::GlobalTransform::IDENTITY,
            ));
            for &r in many {
                let _ = w.set_parent(r, Some(parent));
            }
            parent
        }
    };
    if let Some(t) = w.get_mut::<engine_ecs::Transform>(root) {
        t.translation += offset;
    }
    Ok(script_views::entity_value(root))
}

/// `world.spawn({ Component = { field = value } })` (ADR 0114 §3): an entity with those
/// components, each its default with the fields given written, at once (the next line
/// reads them). A component of one field takes its value as it is (`Name = "Bullet"`). A
/// script component's hooks start at the next stage.
fn world_spawn(lua: &Lua, components: Option<Table>) -> mlua::Result<Value> {
    // SAFETY: the host's world for this stage (`world_ptr`), used within this callback only.
    let w = unsafe { &mut *world_ptr(lua)? };
    let e = w.spawn(());
    let Some(components) = components else {
        return Ok(script_views::entity_value(e));
    };
    let made = (|| {
        for pair in components.pairs::<mlua::LuaString, Value>() {
            let (name, value) = pair?;
            let name = name.to_str()?;
            let Some((id, info)) = named(lua, &name) else {
                return Err(mlua::Error::runtime(format!(
                    "world.spawn: no component is called `{name}`"
                )));
            };
            if matches!(info.name, "Parent" | "Children") {
                return Err(mlua::Error::runtime(
                    "`Parent` and `Children` keep the hierarchy: they are not spawned with",
                ));
            }
            if w.get_raw(e, id).is_none() {
                let made = lua
                    .app_data_ref::<engine_scene::SceneRegistry>()
                    .is_some_and(|r| r.insert_default(w, e, info.name));
                if !made {
                    return Err(mlua::Error::runtime(format!(
                        "`{name}` cannot be made from a script"
                    )));
                }
            }
            match value {
                Value::Table(fields) => {
                    for pair in fields.pairs::<mlua::LuaString, Value>() {
                        let (key, value) = pair?;
                        let f = struct_field(info, &key.to_str()?)?;
                        set_field(w, e, id, info, 0, f, value)?;
                    }
                }
                Value::Boolean(true) => {}
                value => {
                    let TypeKind::Struct([f]) = &info.kind else {
                        return Err(mlua::Error::runtime(format!(
                            "world.spawn: `{name}` takes a table of its fields"
                        )));
                    };
                    set_field(w, e, id, info, 0, f, value)?;
                }
            }
        }
        Ok(())
    })();
    match made {
        Ok(()) => Ok(script_views::entity_value(e)),
        Err(err) => {
            // Nothing half made stays.
            w.despawn(e);
            Err(err)
        }
    }
}

/// A prop from its declaration: a plain default, or a `prop.*` helper's table.
fn prop_of(component: &str, name: String, value: Value) -> mlua::Result<Prop> {
    let bad =
        |why: &str| mlua::Error::runtime(format!("component `{component}`, prop `{name}`: {why}"));
    if !name.starts_with(|c: char| c.is_ascii_lowercase() || c == '_') {
        return Err(bad(
            "a prop's name starts with a small letter (capitals name components)",
        ));
    }
    let plain = |value: &Value| match value {
        Value::Number(_) | Value::Integer(_) => Some(Kind::Number),
        Value::Boolean(_) => Some(Kind::Boolean),
        Value::Vector(_) => Some(Kind::Vector),
        Value::String(_) => Some(Kind::Text),
        _ => None,
    };
    if let Some(kind) = plain(&value) {
        return Ok(Prop {
            name,
            kind,
            default: value,
            attrs: Attrs::NONE,
            offset: 0,
        });
    }
    let Value::Table(t) = &value else {
        return Err(bad(
            "a prop is a number, a boolean, a vector, a string or a prop.* value",
        ));
    };
    let helper: Option<String> = t.get("__prop")?;
    let kind = match helper.as_deref() {
        Some("number") => Kind::Number,
        Some("integer") => Kind::Integer,
        Some("boolean") => Kind::Boolean,
        Some("vector") => Kind::Vector,
        Some("entity") => Kind::Entity,
        Some("asset") => Kind::Asset,
        Some("string") => Kind::Text,
        _ => {
            return Err(bad(
                "a table is not a prop; use prop.number(…) and the others",
            ));
        }
    };
    let default: Value = t.get("default")?;
    if kind == Kind::Text && !matches!(default, Value::String(_) | Value::Nil) {
        return Err(bad("prop.string's default is a string"));
    }
    let mut attrs = Attrs::NONE;
    if let Some(opts) = t.get::<Option<Table>>("opts")? {
        attrs.min = opts.get("min")?;
        attrs.max = opts.get("max")?;
        attrs.step = opts.get("step")?;
        attrs.hidden = opts.get::<Option<bool>>("hidden")?.unwrap_or(false);
        attrs.skip = opts.get::<Option<bool>>("skip")?.unwrap_or(false);
        // How the network sends it, once the game's schema names its component.
        attrs.net_skip = opts.get::<Option<bool>>("net_skip")?.unwrap_or(false);
        attrs.net_quantum = opts.get("net_quantum")?;
        if attrs
            .net_quantum
            .is_some_and(|q| q <= 0.0 || !q.is_finite())
        {
            return Err(bad("net_quantum is a positive number"));
        }
        if let Some(tip) = opts.get::<Option<String>>("tooltip")? {
            attrs.tooltip = Some(leak(&tip));
        }
    }
    Ok(Prop {
        name,
        kind,
        default,
        attrs,
        offset: 0,
    })
}

/// The physics world scripts query in this stage, or why there is none.
fn open_physics() -> mlua::Result<*const crate::ScenePhysics> {
    let p = OPEN_PHYSICS.get();
    if p.is_null() {
        return Err(mlua::Error::runtime(
            "no physics world: the game gives none (`Game::physics`), and \
             `PlayerSettings::physics` asks for none",
        ));
    }
    Ok(p)
}

/// `physics.filter(layers)`: the collision layers a query sees, by name, made once at a
/// module's top and resolved against a physics world's layer names the first time it is
/// used there.
struct PhysicsFilter {
    names: Vec<String>,
    /// The world it was resolved for (its address), and the layers' bits.
    resolved: core::cell::Cell<Option<(usize, u64)>>,
}

impl UserData for PhysicsFilter {}

/// What query `filter` (a [`PhysicsFilter`], or none: every layer) sees in `physics`.
fn query_filter(
    physics: &crate::ScenePhysics,
    filter: Option<&AnyUserData>,
) -> mlua::Result<engine_physics::QueryFilter> {
    let mut out = engine_physics::QueryFilter::default();
    let Some(filter) = filter else {
        return Ok(out);
    };
    let filter = filter.borrow::<PhysicsFilter>()?;
    let world = core::ptr::from_ref(physics) as usize;
    let bits = match filter.resolved.get() {
        Some((at, bits)) if at == world => bits,
        _ => {
            let mut bits = 0u64;
            for name in &filter.names {
                let layer = physics.settings().layer(name).ok_or_else(|| {
                    mlua::Error::runtime(format!(
                        "no collision layer `{name}` (PhysicsSettings::layer_names)"
                    ))
                })?;
                bits |= 1 << layer.0;
            }
            filter.resolved.set(Some((world, bits)));
            bits
        }
    };
    out.layers = bits;
    Ok(out)
}

/// `physics.raycast(origin, direction, max_distance, filter?)`: the first body along the
/// ray, as its entity (nil for a body without one), the hit's point and normal, and its
/// distance; nils when it hits nothing. Four values, not a table: nothing is allocated.
fn raycast(
    _: &Lua,
    (origin, direction, max, filter): (Vector, Vector, f32, Option<AnyUserData>),
) -> mlua::Result<(Value, Value, Value, Value)> {
    // SAFETY: the stage's physics world (`open_physics`), read within this callback only.
    let physics = unsafe { &*open_physics()? };
    let Some(direction) = Vec3::new(direction.x(), direction.y(), direction.z()).try_normalize()
    else {
        return Err(mlua::Error::runtime(
            "physics.raycast: a direction of length 0",
        ));
    };
    // No length (or not a number): nothing to hit.
    if max.is_nan() || max <= 0.0 {
        return Ok((Value::Nil, Value::Nil, Value::Nil, Value::Nil));
    }
    let ray = engine_physics::Ray {
        origin: Vec3::new(origin.x(), origin.y(), origin.z()),
        direction: direction * max,
    };
    let filter = query_filter(physics, filter.as_ref())?;
    let Some(hit) = engine_physics::PhysicsBackend::raycast(physics.backend(), &ray, filter) else {
        return Ok((Value::Nil, Value::Nil, Value::Nil, Value::Nil));
    };
    let entity = engine_physics::ecs::user_data_entity(hit.user_data)
        .map_or(Value::Nil, script_views::entity_value);
    let vector = |v: Vec3| Value::Vector(Vector::new(v.x, v.y, v.z));
    Ok((
        entity,
        vector(hit.point),
        vector(hit.normal),
        Value::Number(f64::from(hit.fraction * max)),
    ))
}

/// `physics.overlap_sphere(center, radius, filter?)`: the entities whose bodies overlap the
/// sphere, for a `for` loop (each once, in the bodies' order).
fn overlap_sphere(
    lua: &Lua,
    (center, radius, filter): (Vector, f32, Option<AnyUserData>),
) -> mlua::Result<Function> {
    // SAFETY: the stage's physics world (`open_physics`), read within this callback only.
    let physics = unsafe { &*open_physics()? };
    let filter = query_filter(physics, filter.as_ref())?;
    let mut bodies = Vec::new();
    engine_physics::PhysicsBackend::overlap_sphere(
        physics.backend(),
        Vec3::new(center.x(), center.y(), center.z()),
        radius.max(0.0),
        filter,
        &mut bodies,
    );
    let mut entities: Vec<Entity> = Vec::with_capacity(bodies.len());
    for body in bodies {
        if let Some(e) = physics.entity_of(body)
            && !entities.contains(&e)
        {
            entities.push(e);
        }
    }
    let next = core::cell::RefCell::new(entities.into_iter());
    lua.create_function(move |_, ()| {
        Ok(next
            .borrow_mut()
            .next()
            .map_or(Value::Nil, script_views::entity_value))
    })
}

/// The world the host opened for this stage (null outside), for the fast path.
pub(crate) fn open_world() -> *mut World {
    OPEN_WORLD.get()
}

/// `f` with `world` open to scripts as a hook has it, the world and physics world open
/// before put back after: an evaluation at a stop (ADR 0119 E9-1b-5c), with the world the
/// frame has now.
///
/// # Safety
///
/// `world` is valid and borrowed by nothing else while `f` runs.
pub(crate) unsafe fn with_world<R>(world: *mut World, f: impl FnOnce() -> R) -> R {
    let (before, physics) = (OPEN_WORLD.get(), OPEN_PHYSICS.get());
    OPEN_WORLD.set(world);
    let r = f();
    OPEN_WORLD.set(before);
    OPEN_PHYSICS.set(physics);
    r
}

/// A stage stopped halfway (ADR 0119) goes on with the world and the physics world of the
/// frame that resumes it: the borrows it was opened with belong to an earlier frame.
///
/// # Safety
///
/// As [`Scripts::tick_open`]'s, for the rest of the stage.
pub unsafe fn reopen(world: *mut World, physics: *const crate::ScenePhysics) {
    OPEN_WORLD.set(world);
    OPEN_PHYSICS.set(physics);
}

/// The world the host opened for this stage. A caller dereferences it within one callback
/// and not across a call back into Lua, so no two borrows of the world are alive at once.
fn world_ptr(_lua: &Lua) -> mlua::Result<*mut World> {
    let w = OPEN_WORLD.get();
    if w.is_null() {
        return Err(mlua::Error::runtime(
            "the world is open to scripts only in their hooks",
        ));
    }
    Ok(w)
}

/// The component named `name` (a reflected Rust one or a script's), by its stable name.
fn named(lua: &Lua, name: &str) -> Option<(ComponentId, &'static TypeInfo)> {
    lua.app_data_ref::<Names>()?.by_name.get(name).copied()
}

/// The text of key `key` (made only where the fast path found no field: reading a string
/// through mlua allocates).
fn key_text(key: &Value) -> mlua::Result<mlua::BorrowedStr> {
    match key {
        Value::String(s) => s.to_str(),
        _ => Err(mlua::Error::runtime("fields are named by strings")),
    }
}

/// The field of `info` a slow path is asked for: at place `slot` when the fast path found
/// it, else named by `key`.
fn asked_field(
    info: &'static TypeInfo,
    key: &Value,
    slot: Option<u32>,
) -> mlua::Result<&'static Field> {
    if let Some(i) = slot
        && let TypeKind::Struct(fields) = &info.kind
        && let Some(f) = fields.get(i as usize)
    {
        return Ok(f);
    }
    struct_field(info, &key_text(key)?)
}

/// `self.key` where the fast path hands over (ADR 0114 E7-1c): props of the kinds it does
/// not read (`slot`: the field's place, as the fast path found it), and what is missing.
fn self_index_slow(
    lua: &Lua,
    (ud, key, slot): (AnyUserData, Value, Option<u32>),
) -> mlua::Result<Value> {
    // SAFETY: the fast path hands over `self`s of its tag only.
    let this = unsafe { *script_views::this_at(ud.to_pointer()) };
    // `self.entity` and `self.locals` are the fast path's own.
    if slot.is_none() {
        let key = key_text(&key)?;
        if key.starts_with(|c: char| c.is_ascii_uppercase()) {
            return component_view(lua, this.entity, &key);
        }
    }
    let f = asked_field(this.info, &key, slot)?;
    // SAFETY: the host's world for this stage (`world_ptr`), used within this callback only.
    let w = unsafe { &mut *world_ptr(lua)? };
    field(lua, w, this.entity, this.id, 0, f)
}

/// `self.key = value` where the fast path hands over: props of the kinds it does not write,
/// and what cannot be written.
fn self_newindex_slow(
    lua: &Lua,
    (ud, key, value, slot): (AnyUserData, Value, Value, Option<u32>),
) -> mlua::Result<()> {
    // SAFETY: as `self_index_slow`.
    let this = unsafe { *script_views::this_at(ud.to_pointer()) };
    if slot.is_none() {
        let key = key_text(&key)?;
        if key.starts_with(|c: char| c.is_ascii_uppercase()) {
            return Err(mlua::Error::runtime(format!(
                "`self.{key}` is a component: write its fields (`self.{key}.field = …`)"
            )));
        }
    }
    let f = asked_field(this.info, &key, slot)?;
    // SAFETY: the host's world for this stage (`world_ptr`), used within this callback only.
    let w = unsafe { &mut *world_ptr(lua)? };
    set_field(w, this.entity, this.id, this.info, 0, f, value)
}

/// `view.key` where the fast path hands over.
fn view_index_slow(
    lua: &Lua,
    (ud, key, slot): (AnyUserData, Value, Option<u32>),
) -> mlua::Result<Value> {
    // SAFETY: the fast path hands over views of their tag only.
    let view = unsafe { *script_views::view_at(ud.to_pointer()) };
    let f = asked_field(view.info, &key, slot)?;
    // SAFETY: the host's world for this stage (`world_ptr`), used within this callback only.
    let w = unsafe { &mut *world_ptr(lua)? };
    field(lua, w, view.entity, view.id, view.offset, f)
}

/// `view.key = value` where the fast path hands over.
fn view_newindex_slow(
    lua: &Lua,
    (ud, key, value, slot): (AnyUserData, Value, Value, Option<u32>),
) -> mlua::Result<()> {
    // SAFETY: as `view_index_slow`.
    let view = unsafe { *script_views::view_at(ud.to_pointer()) };
    let f = asked_field(view.info, &key, slot)?;
    // SAFETY: the host's world for this stage (`world_ptr`), used within this callback only.
    let w = unsafe { &mut *world_ptr(lua)? };
    set_field(w, view.entity, view.id, view.info, view.offset, f, value)
}

/// A view of `entity`'s component `id` (described by `info`), or of a struct of type
/// `info` at `offset` in it, made on the fast path's tag.
fn new_view(
    lua: &Lua,
    entity: Entity,
    id: ComponentId,
    info: &'static TypeInfo,
    offset: usize,
) -> mlua::Result<Value> {
    let tables = script_views::open_tables();
    if tables.is_null() {
        return Err(mlua::Error::runtime(
            "the world is open to scripts only in their hooks",
        ));
    }
    // SAFETY: the stage's tables (`open_tables`), used within this call only; room for the
    // one value pushed.
    unsafe {
        lua.exec_raw::<Value>((), |state| {
            script_views::push_view(state, &mut *tables, entity, id, info, offset);
        })
    }
}

/// A view of `entity`'s component `name`, or an error naming what is missing.
fn component_view(lua: &Lua, entity: Entity, name: &str) -> mlua::Result<Value> {
    let Some((id, info)) = named(lua, name) else {
        return Err(mlua::Error::runtime(format!(
            "no component is called `{name}`"
        )));
    };
    // SAFETY: the host's world for this stage (`world_ptr`), used within this callback only.
    let w = unsafe { &mut *world_ptr(lua)? };
    if w.get_raw(entity, id).is_none() {
        return Err(mlua::Error::runtime(format!(
            "the entity has no {name} (check with `e:has({name})` once it exists)"
        )));
    }
    new_view(lua, entity, id, info, 0)
}

/// A rotation in a script.
#[derive(Clone, Copy)]
struct ScriptQuat(Quat);

impl UserData for ScriptQuat {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::Index, |_, this, key: mlua::LuaString| {
            let q = this.0;
            Ok(match &*key.to_str()? {
                "x" => q.x,
                "y" => q.y,
                "z" => q.z,
                "w" => q.w,
                other => {
                    return Err(mlua::Error::runtime(format!("a quat has no `{other}`")));
                }
            })
        });
        methods.add_meta_function(MetaMethod::Mul, |lua, (a, b): (AnyUserData, Value)| {
            let a = a.borrow::<ScriptQuat>()?.0;
            match b {
                Value::Vector(v) => {
                    let r = a * Vec3::new(v.x(), v.y(), v.z());
                    Ok(Value::Vector(Vector::new(r.x, r.y, r.z)))
                }
                Value::UserData(u) => {
                    let b = u.borrow::<ScriptQuat>()?.0;
                    lua.create_userdata(ScriptQuat(a * b)).map(Value::UserData)
                }
                _ => Err(mlua::Error::runtime("a quat multiplies a quat or a vector")),
            }
        });
        methods.add_meta_method(MetaMethod::ToString, |_, this, ()| {
            let q = this.0;
            Ok(format!("quat({}, {}, {}, {})", q.x, q.y, q.z, q.w))
        });
    }
}

/// The struct field `key` of `info` at `offset` in `entity`'s component `id`, as a Luau
/// value.
fn field(
    lua: &Lua,
    w: &World,
    entity: Entity,
    id: ComponentId,
    offset: usize,
    f: &'static Field,
) -> mlua::Result<Value> {
    let base = w
        .get_raw(entity, id)
        .ok_or_else(|| mlua::Error::runtime("the entity is gone, or lost the component"))?;
    // SAFETY: the component's bytes, `f` inside them by its offset from reflection.
    let at = unsafe { base.as_ptr().add(offset + f.offset) };
    read(lua, at, f.ty, entity, id, offset + f.offset)
}

/// Reads a value of `ty` at `at` (offset `offset` in `entity`'s component `id`).
fn read(
    lua: &Lua,
    at: *const u8,
    ty: &'static TypeInfo,
    entity: Entity,
    id: ComponentId,
    offset: usize,
) -> mlua::Result<Value> {
    // SAFETY: (each read) `at` points to a value of `ty` inside a live component.
    unsafe {
        match ty.name {
            "Vec3" => {
                let v = at.cast::<Vec3>().read_unaligned();
                return Ok(Value::Vector(Vector::new(v.x, v.y, v.z)));
            }
            "Vec3A" => {
                let v = at.cast::<Vec3A>().read_unaligned();
                return Ok(Value::Vector(Vector::new(v.x, v.y, v.z)));
            }
            "Vec2" => {
                let v = at.cast::<Vec2>().read_unaligned();
                return Ok(Value::Vector(Vector::new(v.x, v.y, 0.0)));
            }
            "Quat" => {
                let q = at.cast::<Quat>().read_unaligned();
                return lua.create_userdata(ScriptQuat(q)).map(Value::UserData);
            }
            _ => {}
        }
        if ty.name == "Entity" {
            return Ok(script_views::entity_value(
                at.cast::<Entity>().read_unaligned(),
            ));
        }
        if ty.name == "AssetRef" {
            let a = at.cast::<engine_scene::AssetRef>().read_unaligned();
            // SAFETY: room for the one value pushed, returned.
            return lua.exec_raw::<Value>((), |state| script_views::push_new_asset(state, a));
        }
        match &ty.kind {
            TypeKind::Primitive(p) => Ok(match p {
                Primitive::Bool => Value::Boolean(at.read() != 0),
                Primitive::I8 => Value::Number(f64::from(at.cast::<i8>().read())),
                Primitive::I16 => Value::Number(f64::from(at.cast::<i16>().read_unaligned())),
                Primitive::I32 => Value::Number(f64::from(at.cast::<i32>().read_unaligned())),
                Primitive::U8 => Value::Number(f64::from(at.read())),
                Primitive::U16 => Value::Number(f64::from(at.cast::<u16>().read_unaligned())),
                Primitive::U32 => Value::Number(f64::from(at.cast::<u32>().read_unaligned())),
                Primitive::I64 => Value::Number(at.cast::<i64>().read_unaligned() as f64),
                Primitive::U64 => Value::Number(at.cast::<u64>().read_unaligned() as f64),
                Primitive::F32 => Value::Number(f64::from(at.cast::<f32>().read_unaligned())),
                Primitive::F64 => Value::Number(at.cast::<f64>().read_unaligned()),
            }),
            TypeKind::Enum { repr, variants } => {
                let v = read_int(at, *repr);
                let name = variants
                    .iter()
                    .find(|x| x.value == v)
                    .map_or("", |x| x.name);
                lua.create_string(name).map(Value::String)
            }
            TypeKind::String => {
                let s = engine_reflect::read_str(at);
                lua.create_string(s).map(Value::String)
            }
            TypeKind::Struct(_) => new_view(lua, entity, id, ty, offset),
            _ => Ok(Value::Nil),
        }
    }
}

/// An integer of `repr` at `at`.
///
/// # Safety
///
/// `at` points to one.
unsafe fn read_int(at: *const u8, repr: Primitive) -> i64 {
    // SAFETY: as the caller says.
    unsafe {
        match repr {
            Primitive::I8 => i64::from(at.cast::<i8>().read()),
            Primitive::U8 => i64::from(at.read()),
            Primitive::I16 => i64::from(at.cast::<i16>().read_unaligned()),
            Primitive::U16 => i64::from(at.cast::<u16>().read_unaligned()),
            Primitive::I32 => i64::from(at.cast::<i32>().read_unaligned()),
            Primitive::U32 => i64::from(at.cast::<u32>().read_unaligned()),
            Primitive::I64 | Primitive::U64 => at.cast::<i64>().read_unaligned(),
            _ => 0,
        }
    }
}

/// The field `key` of struct `info`.
fn struct_field(info: &'static TypeInfo, key: &str) -> mlua::Result<&'static Field> {
    let TypeKind::Struct(fields) = &info.kind else {
        return Err(mlua::Error::runtime(format!("{} has no fields", info.name)));
    };
    fields
        .iter()
        .find(|f| f.name == key)
        .ok_or_else(|| mlua::Error::runtime(format!("{} has no field `{key}`", info.name)))
}

/// Writes `value` into field `f` of `info` at `offset` in `entity`'s component `id`,
/// clamped to the field's range as the inspector does.
fn set_field(
    w: &mut World,
    entity: Entity,
    id: ComponentId,
    info: &'static TypeInfo,
    offset: usize,
    f: &'static Field,
    value: Value,
) -> mlua::Result<()> {
    let key = f.name;
    let base = w
        .get_raw_mut(entity, id)
        .ok_or_else(|| mlua::Error::runtime("the entity is gone, or lost the component"))?;
    // SAFETY: the component's bytes, `f` inside them by its offset from reflection.
    let at = unsafe { base.as_ptr().add(offset + f.offset) };
    let given = if script_views::value_entity(&value).is_some() {
        "Entity"
    } else {
        value.type_name()
    };
    let wrong =
        |what: &str| mlua::Error::runtime(format!("{}.{key} takes {what}, not {given}", info.name));
    let number = match &value {
        Value::Number(n) => Some(*n),
        Value::Integer(i) => Some(*i as f64),
        _ => None,
    };
    let clamped = |n: f64| {
        let n = f.attrs.min.map_or(n, |m| n.max(m));
        f.attrs.max.map_or(n, |m| n.min(m))
    };
    let ty = f.ty;
    // SAFETY: (each write) `at` points to a value of `ty` inside a live component, plain
    // data for every kind written here.
    unsafe {
        if matches!(ty.name, "Vec3" | "Vec3A" | "Vec2") {
            let Value::Vector(v) = value else {
                return Err(wrong("a vector"));
            };
            match ty.name {
                "Vec3" => at
                    .cast::<Vec3>()
                    .write_unaligned(Vec3::new(v.x(), v.y(), v.z())),
                "Vec3A" => at
                    .cast::<Vec3A>()
                    .write_unaligned(Vec3A::new(v.x(), v.y(), v.z())),
                _ => at.cast::<Vec2>().write_unaligned(Vec2::new(v.x(), v.y())),
            }
            return Ok(());
        }
        if ty.name == "Quat" {
            let Value::UserData(u) = &value else {
                return Err(wrong("a quat"));
            };
            let q = u.borrow::<ScriptQuat>().map_err(|_| wrong("a quat"))?.0;
            at.cast::<Quat>().write_unaligned(q.normalize());
            return Ok(());
        }
        if ty.name == "Entity" {
            let e = match &value {
                Value::Nil => Entity::NULL,
                _ => script_views::value_entity(&value).ok_or_else(|| wrong("an entity"))?,
            };
            at.cast::<Entity>().write_unaligned(e);
            return Ok(());
        }
        match &ty.kind {
            TypeKind::Primitive(Primitive::Bool) => {
                let Value::Boolean(b) = value else {
                    return Err(wrong("a boolean"));
                };
                at.write(u8::from(b));
            }
            TypeKind::Primitive(p) => {
                let n = clamped(number.ok_or_else(|| wrong("a number"))?);
                let whole = || {
                    if n.fract() == 0.0 {
                        Ok(n)
                    } else {
                        Err(wrong("a whole number"))
                    }
                };
                match p {
                    Primitive::F32 => at.cast::<f32>().write_unaligned(n as f32),
                    Primitive::F64 => at.cast::<f64>().write_unaligned(n),
                    Primitive::I8 => at.cast::<i8>().write(whole()? as i8),
                    Primitive::U8 => at.write(whole()? as u8),
                    Primitive::I16 => at.cast::<i16>().write_unaligned(whole()? as i16),
                    Primitive::U16 => at.cast::<u16>().write_unaligned(whole()? as u16),
                    Primitive::I32 => at.cast::<i32>().write_unaligned(whole()? as i32),
                    Primitive::U32 => at.cast::<u32>().write_unaligned(whole()? as u32),
                    Primitive::I64 => at.cast::<i64>().write_unaligned(whole()? as i64),
                    Primitive::U64 => at.cast::<u64>().write_unaligned(whole()? as u64),
                    Primitive::Bool => {}
                }
            }
            TypeKind::Enum { repr, variants } => {
                let Value::String(s) = &value else {
                    return Err(wrong("a variant's name"));
                };
                let s = s.to_str()?;
                let v = variants
                    .iter()
                    .find(|x| x.name == &*s)
                    .ok_or_else(|| wrong("one of its variants"))?
                    .value;
                match repr {
                    Primitive::I8 | Primitive::U8 => at.write(v as u8),
                    Primitive::I16 | Primitive::U16 => at.cast::<u16>().write_unaligned(v as u16),
                    Primitive::I64 | Primitive::U64 => at.cast::<i64>().write_unaligned(v),
                    _ => at.cast::<u32>().write_unaligned(v as u32),
                }
            }
            TypeKind::String => {
                let Value::String(s) = &value else {
                    return Err(wrong("a string"));
                };
                engine_reflect::write_str(at, &s.to_str()?);
            }
            _ => {
                return Err(mlua::Error::runtime(format!(
                    "{}.{key} is written as a whole value, not from a script yet",
                    info.name
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use engine_ecs::Transform;
    use engine_scene::Name;

    /// A project folder with `scripts/<name>` for each of `files`.
    fn project(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("ofelia-scripts-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (file, text) in files {
            let path = dir.join("scripts").join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        dir
    }

    fn load(dir: &Path, world: &mut World) -> Scripts {
        Scripts::load(world, dir, &dir.join("scripts")).unwrap()
    }

    const DOOR: &str = r#"--!strict
local Door = component("Door", {
    lift = 2.5,
    speed = prop.number(2, { min = 0.1, tooltip = "Metres a second" }),
    closed = vector.zero,
    opened = false,
})

function Door.on_start(self)
    self.closed = self.Transform.translation
end

function Door.on_tick(self, dt: number)
    local player = world.find("Player")
    local near = player ~= nil
        and vector.magnitude(player.Transform.translation - self.closed) < 3
    local goal = if near then self.closed + vector.create(0, self.lift, 0) else self.closed
    local t = self.Transform
    local to_goal = goal - t.translation
    local step = math.min(vector.magnitude(to_goal), self.speed * dt)
    if step > 0 then
        t.translation += vector.normalize(to_goal) * step
    end
    self.opened = near
    self.locals.ticks = (self.locals.ticks or 0) + 1
end

return Door
"#;

    #[test]
    fn a_door_rises_while_the_player_is_near_and_its_props_live_in_the_world() {
        let dir = project("door", &[("door.luau", DOOR)]);
        let mut world = World::new();
        let mut scripts = load(&dir, &mut world);
        assert_eq!(scripts.component_names().collect::<Vec<_>>(), ["Door"]);
        assert_eq!(
            scripts.file_of("Door"),
            Some(Path::new("scripts/door.luau"))
        );
        let door = world.spawn((Name::new("Front door"), Transform::from_xyz(4.0, 0.0, 0.0)));
        assert!(scripts.add(&mut world, door, "Door"));
        let player = world.spawn((Name::new("Player"), Transform::from_xyz(10.0, 0.0, 0.0)));
        // Far away: the door stays.
        for _ in 0..8 {
            scripts.tick(&mut world, 0.125);
        }
        assert!(scripts.reported.is_empty(), "{:?}", scripts.reported);
        assert_eq!(scripts.instances.len(), 1);
        assert_eq!(
            world.get::<Transform>(door).unwrap().translation,
            Vec3::new(4.0, 0.0, 0.0)
        );
        // Near: it rises at its speed, 2 m/s, up to its lift.
        world.get_mut::<Transform>(player).unwrap().translation = Vec3::new(5.0, 0.0, 0.0);
        for _ in 0..4 {
            scripts.tick(&mut world, 0.125);
        }
        let y = world.get::<Transform>(door).unwrap().translation.y;
        assert!((y - 1.0).abs() < 1e-5, "{y}");
        for _ in 0..20 {
            scripts.tick(&mut world, 0.125);
        }
        let y = world.get::<Transform>(door).unwrap().translation.y;
        assert!((y - 2.5).abs() < 1e-5, "{y}");
        // Its props are the world's: read by reflection as the inspector reads them.
        let id = scripts.component("Door").unwrap();
        let info = world.components().type_info(id).unwrap();
        let names: Vec<&str> = match &info.kind {
            TypeKind::Struct(fields) => fields.iter().map(|f| f.name).collect(),
            _ => Vec::new(),
        };
        assert_eq!(
            names,
            ["closed", "lift", "opened", "speed"],
            "laid out by name"
        );
        let speed = match &info.kind {
            TypeKind::Struct(fields) => fields.iter().find(|f| f.name == "speed").unwrap(),
            _ => unreachable!(),
        };
        assert_eq!(speed.attrs.min, Some(0.1));
        assert_eq!(speed.attrs.tooltip, Some("Metres a second"));
        let base = world.get_raw(door, id).unwrap().as_ptr();
        // SAFETY: the door's component, its `speed` an f32 at its offset.
        let read = unsafe { base.add(speed.offset).cast::<f32>().read_unaligned() };
        assert_eq!(read, 2.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn each_scripts_hooks_are_timed_and_its_memory_counted_while_the_profiler_watches() {
        const KEEPER: &str = r#"--!strict
local Keeper = component("Keeper", { turns = 0 })
local kept = {}
function Keeper.on_tick(self, dt: number)
    self.turns += 1
    table.insert(kept, { self.turns })
end
system("Counter", {
    on_tick = function(dt: number) end,
})
return Keeper
"#;
        let dir = project("profile", &[("door.luau", DOOR), ("keeper.luau", KEEPER)]);
        let mut world = World::new();
        let mut scripts = load(&dir, &mut world);
        let door = world.spawn((Name::new("Front door"), Transform::from_xyz(4.0, 0.0, 0.0)));
        assert!(scripts.add(&mut world, door, "Door"));
        let keeper = world.spawn((Name::new("Keeper"), Transform::IDENTITY));
        assert!(scripts.add(&mut world, keeper, "Keeper"));
        // Not watched: nothing counted.
        for _ in 0..4 {
            scripts.tick(&mut world, 0.125);
        }
        let mut profile = Vec::new();
        scripts.take_profile(&mut profile);
        let names: Vec<&str> = profile.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            ["Door", "Keeper", "Counter"],
            "components, then systems"
        );
        assert!(
            profile
                .iter()
                .all(|p| p.calls == 0 && p.nanos == 0 && p.bytes == 0),
            "{profile:?}"
        );
        // Watched: each hook with its time, and what each keeps against its own.
        scripts.set_profiling(true);
        for _ in 0..8 {
            scripts.tick(&mut world, 0.125);
        }
        scripts.set_profiling(false);
        assert!(scripts.reported.is_empty(), "{:?}", scripts.reported);
        scripts.take_profile(&mut profile);
        let [door, keeper, counter] = [0, 1, 2].map(|k| profile[k].clone());
        assert_eq!((door.calls, keeper.calls, counter.calls), (8, 8, 8));
        assert!(door.nanos > 0 && keeper.nanos > 0 && counter.nanos > 0);
        assert_eq!(keeper.file, "scripts/keeper.luau");
        assert!(keeper.bytes >= 8 * 16, "{keeper:?}");
        assert_eq!(counter.bytes, 0, "{counter:?}");
        // Taken: the counts start again, what is kept stays counted; and nothing is
        // counted while the profiler does not watch.
        scripts.tick(&mut world, 0.125);
        scripts.take_profile(&mut profile);
        assert_eq!((profile[1].calls, profile[1].nanos), (0, 0));
        assert_eq!(profile[1].bytes, keeper.bytes);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_hooks_place_in_tracy_is_made_once() {
        let profiled = Profiled::new(3);
        for _ in 0..3 {
            let _ = profiled.place("Door", "door", Hook::Tick);
        }
        let places = profiled.places.get();
        assert!(places[1].is_some() && places.iter().filter(|p| p.is_some()).count() == 1);
        let lua = Lua::new();
        let open = lua.create_string("open").unwrap();
        let shut = lua.create_string("shut").unwrap();
        for name in [&open, &shut, &open] {
            let _ = profiled.place("Door", "door", Hook::Message(name));
        }
        assert_eq!(profiled.messages.borrow().len(), 2);
    }

    /// ADR 0114 §13's measurements (`--release -- --ignored --nocapture`): an empty
    /// `on_tick` per instance, a prop's and a component field's read and write, and 1,000
    /// doors in one tick; medians of 51 ticks.
    #[test]
    #[ignore = "a measurement"]
    fn measure_hooks_and_views() {
        let dir = project(
            "measure",
            &[
                (
                    "empty.luau",
                    "local Empty = component(\"Empty\", {})
function Empty.on_tick(self, dt) end
return Empty
",
                ),
                (
                    "props.luau",
                    "local Props = component(\"Props\", { a = 1 })
function Props.on_tick(self, dt)
    for i = 1, 100 do self.a = self.a + 1 end
end
return Props
",
                ),
                (
                    "fields.luau",
                    "local Fields = component(\"Fields\", {})
function Fields.on_tick(self, dt)
    local t = self.Transform
    for i = 1, 100 do t.translation = t.translation + vector.create(0, dt, 0) end
end
return Fields
",
                ),
                ("door.luau", DOOR),
                (
                    "spin_system.luau",
                    "local SpinSystem = component(\"SpinSystem\", { speed = 1.5 })
local spinning = world.query(SpinSystem, Transform)
local UP = vector.create(0, 1, 0)
system(\"SpinSystem\", {
    on_tick = function(dt)
        for _, spin, t in spinning do
            t:rotate(UP, spin.speed * dt)
        end
    end,
})
return SpinSystem
",
                ),
                (
                    "spin_hook.luau",
                    "local SpinHook = component(\"SpinHook\", { speed = 1.5 })
local UP = vector.create(0, 1, 0)
function SpinHook.on_tick(self, dt)
    self.Transform:rotate(UP, self.speed * dt)
end
return SpinHook
",
                ),
            ],
        );
        let median = |mut v: Vec<f64>| {
            v.sort_by(f64::total_cmp);
            v[v.len() / 2]
        };
        // ADR 0119's debuggable compilation: `ENGINE_SCRIPTS_MEASURE=o1` measures level 1,
        // `o1-debug` level 1 while debugging, `o1-stop` with a breakpoint that stops set,
        // `o1-errors` with errors not caught stopping; `-interp` after any of them without
        // native code. `ENGINE_SCRIPTS_PROFILE=1`: while the editor's profiler watches.
        let how = std::env::var("ENGINE_SCRIPTS_MEASURE").unwrap_or_default();
        let profiled = std::env::var("ENGINE_SCRIPTS_PROFILE").is_ok_and(|v| v == "1");
        let settings = ScriptSettings {
            optimization_level: if how.starts_with("o1") { 1 } else { 2 },
            native: !how.ends_with("-interp"),
            ..ScriptSettings::default()
        };
        let run = |name: &str, count: usize| {
            let mut world = World::new();
            let mut scripts =
                Scripts::load_with(&mut world, &dir, &dir.join("scripts"), settings).unwrap();
            scripts.set_debugging(how.starts_with("o1-"));
            scripts.set_profiling(profiled);
            if how.starts_with("o1-errors") {
                scripts
                    .debugger()
                    .set_error_stops(engine_script::ErrorStops {
                        uncaught: true,
                        caught: false,
                    });
            }
            // `o1-stop`: a breakpoint that stops is set (on a line no tick runs), so the hooks
            // go through the debugger's thread.
            if how == "o1-stop" {
                scripts.set_breakpoints(
                    "door",
                    vec![engine_script::Breakpoint {
                        line: 1,
                        action: engine_script::BreakAction::Stop,
                        condition: None,
                        hits: engine_script::Hits::Every,
                        enabled: true,
                        only: Vec::new(),
                    }],
                );
            }
            world.spawn((Name::new("Player"), Transform::from_xyz(1.0, 0.0, 0.0)));
            for i in 0..count {
                let e = world.spawn((Transform::from_xyz(i as f32, 0.0, 0.0),));
                scripts.add(&mut world, e, name);
            }
            scripts.tick(&mut world, 1.0 / 64.0);
            let times: Vec<f64> = (0..51)
                .map(|_| {
                    let t = std::time::Instant::now();
                    scripts.tick(&mut world, 1.0 / 64.0);
                    t.elapsed().as_secs_f64() * 1e6
                })
                .collect();
            assert!(scripts.reported.is_empty(), "{:?}", scripts.reported);
            median(times)
        };
        let empty = run("Empty", 1000);
        let props = run("Props", 100);
        let fields = run("Fields", 100);
        let doors = run("Door", 1000);
        let system = run("SpinSystem", 10_000);
        let hooks = run("SpinHook", 10_000);
        println!("| what | per | median |");
        println!("|---|---|---:|");
        println!("| an empty on_tick | instance | {:.3} µs |", empty / 1000.0);
        println!(
            "| a prop read and written (`self.a = self.a + 1`) | pair | {:.1} ns |",
            (props / 100.0 - empty / 1000.0) * 1e3 / 100.0
        );
        println!(
            "| a field read and written (`t.translation = t.translation + v`) | pair | {:.1} ns |",
            (fields / 100.0 - empty / 1000.0) * 1e3 / 100.0
        );
        println!(
            "| 1,000 doors (the door example) | tick | {:.3} ms |",
            doors / 1e3
        );
        println!(
            "| 10,000 spinners as a system (the spin example) | tick | {:.3} ms |",
            system / 1e3
        );
        println!(
            "| 10,000 spinners, a hook each | tick | {:.3} ms |",
            hooks / 1e3
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn script_components_are_saved_in_scene_files_and_loaded_from_them() {
        use engine_scene::{SaveOptions, SceneId, SceneRegistry};
        let dir = project("scenes", &[("door.luau", DOOR)]);
        let mut world = World::new();
        let scripts = load(&dir, &mut world);
        let door = world.spawn((
            SceneId::new(),
            Name::new("Front door"),
            Transform::from_xyz(4.0, 0.0, 0.0),
        ));
        assert!(scripts.add(&mut world, door, "Door"));
        // Its lift set as the inspector would.
        let id = scripts.component("Door").unwrap();
        let TypeKind::Struct(fields) = &world.components().type_info(id).unwrap().kind else {
            unreachable!()
        };
        let lift = fields.iter().find(|f| f.name == "lift").unwrap().offset;
        let base = world.get_raw_mut(door, id).unwrap().as_ptr();
        // SAFETY: Door's `lift`, an f32 at its offset.
        unsafe { base.add(lift).cast::<f32>().write_unaligned(3.0) };
        let mut text = String::new();
        let options = SaveOptions {
            id: Some(engine_scene::AssetId::new()),
            assets: None,
            prefabs: None,
        };
        engine_scene::save_with(
            &mut world,
            &SceneRegistry::from_program(),
            &options,
            &mut text,
        )
        .unwrap();
        assert!(text.contains("Door.lift = 3"), "{text}");
        assert!(text.contains("Door.speed = 2"), "{text}");
        // Another world, its scripts loaded first: the door comes back with its lift, and
        // rises 3 m.
        let mut world = World::new();
        let mut scripts = load(&dir, &mut world);
        engine_scene::load(&mut world, &SceneRegistry::from_program(), &text).unwrap();
        let mut q = engine_ecs::QueryState::<(Entity, &Name)>::new(&mut world);
        let door = q
            .iter_readonly(&world)
            .find(|(_, n)| n.as_str() == "Front door")
            .map(|(e, _)| e)
            .unwrap();
        let id = scripts.component("Door").unwrap();
        let base = world.get_raw(door, id).unwrap().as_ptr();
        // SAFETY: Door's `lift`, an f32 at its offset.
        let read = unsafe { base.add(lift).cast::<f32>().read_unaligned() };
        assert_eq!(read, 3.0);
        world.spawn((Name::new("Player"), Transform::from_xyz(4.5, 0.0, 0.0)));
        for _ in 0..64 {
            scripts.tick(&mut world, 1.0 / 32.0);
        }
        let y = world.get::<Transform>(door).unwrap().translation.y;
        assert!((y - 3.0).abs() < 1e-4, "{y}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_error_stops_its_instance_only_and_a_broken_script_leaves_the_others() {
        let dir = project(
            "errors",
            &[
                (
                    "spin.luau",
                    r#"local Spin = component("Spin", { rate = 1 })
function Spin.on_tick(self, dt)
    if self.rate > 1 then
        error("too fast")
    end
    local t = self.Transform
    t.rotation = quat.axis_angle(vector.create(0, 1, 0), self.rate * dt) * t.rotation
end
return Spin
"#,
                ),
                ("broken.luau", "local x = = 1\n"),
            ],
        );
        let mut world = World::new();
        let mut scripts = load(&dir, &mut world);
        assert_eq!(scripts.component_names().collect::<Vec<_>>(), ["Spin"]);
        let calm = world.spawn((Transform::IDENTITY,));
        let fast = world.spawn((Transform::IDENTITY,));
        scripts.add(&mut world, calm, "Spin");
        scripts.add(&mut world, fast, "Spin");
        // The fast one's rate set as a script would: its first tick fails.
        let id = scripts.component("Spin").unwrap();
        let base = world.get_raw_mut(fast, id).unwrap().as_ptr();
        // SAFETY: Spin's only prop, an f32 at offset 0.
        unsafe { base.cast::<f32>().write_unaligned(5.0) };
        for _ in 0..4 {
            scripts.tick(&mut world, 0.25);
        }
        let turned = world.get::<Transform>(calm).unwrap().rotation;
        let expected = Quat::from_axis_angle(Vec3::Y, 1.0);
        assert!(turned.angle_between(expected) < 1e-4, "{turned:?}");
        assert_eq!(
            world.get::<Transform>(fast).unwrap().rotation,
            Quat::IDENTITY
        );
        let stopped = scripts
            .instances
            .values()
            .filter(|(_, i)| i.stopped)
            .count();
        assert_eq!(stopped, 1);
        assert!(
            scripts
                .reported
                .iter()
                .any(|r| r.starts_with("scripts/spin.luau:4: too fast (Spin.on_tick)")),
            "{:?}",
            scripts.reported
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn entities_answer_on_the_fast_path_and_its_errors_name_the_line() {
        let dir = project(
            "entities",
            &[
                (
                    "probe.luau",
                    r#"local Probe = component("Probe", { seen = 0, target = prop.entity(), ok = false })
function Probe.on_tick(self, dt)
    local p = world.find("Player")
    self.target = p
    local same = p == world.find("Player") and p ~= self.entity and self.entity == self.entity
    local named = p.name == "Player" and p.alive and string.sub(tostring(p), 1, 7) == "Entity("
    local moved = p.Transform.translation.x == 7 and self.Transform.translation.y == 2
    self.ok = same and named and moved and self.target == p and world.find("Nobody") == nil
    self.seen += 1
end
return Probe
"#,
                ),
                (
                    "wrong.luau",
                    r#"local Wrong = component("Wrong", { speed = 1 })
function Wrong.on_tick(self, dt)
    self.speed = "fast"
end
return Wrong
"#,
                ),
                (
                    "ghost.luau",
                    r#"local Ghost = component("Ghost", {})
function Ghost.on_tick(self, dt)
    local g = world.find("Player").Haunt
end
return Ghost
"#,
                ),
            ],
        );
        let mut world = World::new();
        let mut scripts = load(&dir, &mut world);
        let player = world.spawn((Name::new("Player"), Transform::from_xyz(7.0, 0.0, 0.0)));
        let probe = world.spawn((Transform::from_xyz(0.0, 2.0, 0.0),));
        scripts.add(&mut world, probe, "Probe");
        let wrong = world.spawn((Transform::IDENTITY,));
        scripts.add(&mut world, wrong, "Wrong");
        let ghost = world.spawn((Transform::IDENTITY,));
        scripts.add(&mut world, ghost, "Ghost");
        for _ in 0..3 {
            scripts.tick(&mut world, 0.1);
        }
        assert_eq!(read_f32(&world, &scripts, probe, "Probe", "seen"), 3.0);
        let id = scripts.component("Probe").unwrap();
        let info = world.components().type_info(id).unwrap();
        let TypeKind::Struct(fields) = &info.kind else {
            unreachable!()
        };
        let at = |name: &str| fields.iter().find(|f| f.name == name).unwrap().offset;
        let base = world.get_raw(probe, id).unwrap().as_ptr();
        // SAFETY: Probe's props at their offsets: a bool and an entity.
        unsafe {
            assert_eq!(base.add(at("ok")).read(), 1, "every check held");
            assert_eq!(
                base.add(at("target")).cast::<Entity>().read_unaligned(),
                player
            );
        }
        let reported: Vec<&str> = scripts.reported.iter().map(String::as_str).collect();
        for line in [
            "scripts/wrong.luau:3: Wrong.speed takes a number, not string (Wrong.on_tick)",
            "scripts/ghost.luau:3: no component is called `Haunt` (Ghost.on_tick)",
        ] {
            assert!(reported.contains(&line), "{line} in {reported:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn entities_are_values_and_keys_and_find_follows_the_names() {
        let dir = project(
            "values",
            &[
                (
                    "keys.luau",
                    r#"local Keys = component("Keys", { checks = 0, ok = false, mate = prop.entity() })
function Keys.on_tick(self, dt)
    local p = world.find("Player")
    local seen = self.locals
    seen[p] = (seen[p] or 0) + 1
    local by_value = seen[world.find("Player")] == seen[p]
    self.mate = p
    local typed = typeof(p) == "Entity" and type(p) == "userdata"
    local stored = self.mate == p and rawequal(self.mate, p) and rawequal(p.Transform, p.Transform)
    local set = {}
    set[p] = true
    set[self.entity] = true
    set[world.find("Player")] = true
    local count = 0
    for _ in pairs(set) do
        count += 1
    end
    self.ok = by_value and typed and stored and count == 2 and seen[p] == self.checks + 1
    self.checks += 1
end
return Keys
"#,
                ),
                (
                    "finder.luau",
                    r#"local Finder = component("Finder", { found = prop.entity(), other = prop.entity(), many = 0 })
function Finder.on_tick(self, dt)
    self.found = world.find("Player")
    self.other = world.find("Other")
    -- More entities than the kept views' places: each view still shows its own.
    local right = 0
    for i = 1, 300 do
        local e = world.find("E" .. i)
        if e and e.Transform.translation.x == i then
            right += 1
        end
    end
    self.many = right
end
return Finder
"#,
                ),
                (
                    "bad.luau",
                    r#"local BadMate = component("BadMate", { mate = prop.entity() })
function BadMate.on_tick(self, dt)
    self.mate = 5
end
local BadSpeed = component("BadSpeed", { speed = 1 })
function BadSpeed.on_tick(self, dt)
    self.speed = world.find("Player")
end
return BadMate
"#,
                ),
            ],
        );
        let mut world = World::new();
        let mut scripts = load(&dir, &mut world);
        let first = world.spawn((Name::new("Player"), Transform::IDENTITY));
        for i in 1..=300 {
            world.spawn((
                Name::new(format!("E{i}")),
                Transform::from_xyz(i as f32, 0.0, 0.0),
            ));
        }
        let keys = world.spawn((Transform::IDENTITY,));
        scripts.add(&mut world, keys, "Keys");
        let finder = world.spawn((Transform::IDENTITY,));
        scripts.add(&mut world, finder, "Finder");
        let bad = world.spawn((Transform::IDENTITY,));
        scripts.add(&mut world, bad, "BadMate");
        scripts.add(&mut world, bad, "BadSpeed");
        let found = |world: &World, scripts: &Scripts| {
            (
                read_entity(world, scripts, finder, "Finder", "found"),
                read_entity(world, scripts, finder, "Finder", "other"),
            )
        };
        for _ in 0..3 {
            scripts.tick(&mut world, 0.1);
        }
        assert_eq!(read_f32(&world, &scripts, keys, "Keys", "checks"), 3.0);
        let id = scripts.component("Keys").unwrap();
        let TypeKind::Struct(fields) = &world.components().type_info(id).unwrap().kind else {
            unreachable!()
        };
        let ok = fields.iter().find(|f| f.name == "ok").unwrap().offset;
        // SAFETY: Keys' `ok`, a bool at its offset.
        let ok = unsafe { world.get_raw(keys, id).unwrap().as_ptr().add(ok).read() };
        assert_eq!(ok, 1, "entities compare, hash and type as values");
        assert_eq!(read_entity(&world, &scripts, keys, "Keys", "mate"), first);
        assert_eq!(read_f32(&world, &scripts, finder, "Finder", "many"), 300.0);
        assert_eq!(found(&world, &scripts), (first, Entity::NULL));

        // A second of the name comes after the first: the first stays found.
        let second = world.spawn((Name::new("Player"), Transform::IDENTITY));
        scripts.tick(&mut world, 0.1);
        assert_eq!(found(&world, &scripts), (first, Entity::NULL));
        // The first gone: the second.
        world.despawn(first);
        scripts.tick(&mut world, 0.1);
        assert_eq!(found(&world, &scripts), (second, Entity::NULL));
        // Renamed: found by its new name only.
        *world.get_mut::<Name>(second).unwrap() = Name::new("Other");
        scripts.tick(&mut world, 0.1);
        assert_eq!(found(&world, &scripts), (Entity::NULL, second));
        // One that comes with the name is found from the next stage on.
        let third = world.spawn((Name::new("Player"), Transform::IDENTITY));
        scripts.tick(&mut world, 0.1);
        assert_eq!(found(&world, &scripts), (third, second));

        let reported: Vec<&str> = scripts.reported.iter().map(String::as_str).collect();
        for line in [
            "scripts/bad.luau:3: BadMate.mate takes an entity, not number (BadMate.on_tick)",
            "scripts/bad.luau:7: BadSpeed.speed takes a number, not Entity (BadSpeed.on_tick)",
        ] {
            assert!(reported.contains(&line), "{line} in {reported:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn entities_have_methods_and_components_are_values() {
        let dir = project(
            "methods",
            &[
                (
                    "methods.luau",
                    r#"local Methods = component("Methods", {
    has = false, kind = false, kids = 0, first = prop.entity(), nearest = prop.entity(),
    family = false, added = false, own = 0, removed = false,
})
function Methods.bump(self, by)
    self.own += by
    return self.own
end
function Methods.on_tick(self, dt)
    local e = self.entity
    self.has = e:has(Transform) and self:has(Transform) and e:has(Methods)
        and e:has("Methods") and not e:has("Trigger") == (self.own == 0)
    self.kind = type(Transform) == "table" and Transform.name == "Transform"
        and Methods.name == "Methods"
    local n = 0
    for child in e:children() do
        n += 1
        if n == 1 then
            self.first = child
        end
    end
    self.kids = n
    self.nearest = e:find_child("Deep")
    local second = e:find_child("Second")
    self.family = second ~= nil and second.parent == e and e.parent == nil
        and e:find_child("Nobody") == nil
    if not e:has(Trigger) then
        e:add(Trigger, { size = vector.create(2, 3, 4) })
        self.added = e.Trigger.size.y == 3 and e.Trigger.shape == "Box"
    end
    self.removed = self:bump(2) == self.own
    if self.own >= 4 then
        e:remove(Trigger)
    end
end
return Methods
"#,
                ),
                (
                    "turn.luau",
                    r#"local Turn = component("Turn", { forward = vector.zero, right = vector.zero, up = vector.zero, looked = vector.zero })
function Turn.on_tick(self, dt)
    local t = self.Transform
    t:rotate(vector.create(0, 2, 0), math.pi / 2)
    self.forward = t:forward()
    self.right = t:right()
    self.up = t:up()
    local eye = world.find("Eye").Transform
    eye:look_at(vector.create(10, 1, 0))
    self.looked = eye:forward()
end
return Turn
"#,
                ),
                (
                    "doomed.luau",
                    r#"local Doomed = component("Doomed", { ticks = 0 })
function Doomed.on_tick(self, dt)
    self.ticks += 1
    if self.ticks == 2 then
        self:despawn()
        self.Transform.translation = vector.create(5, 0, 0)
    end
end
function Doomed.on_stop(self)
    world.find("Witness").Transform.translation = vector.create(self.ticks, self.Transform.translation.x, 0)
end
return Doomed
"#,
                ),
                (
                    "wrong.luau",
                    r#"local NoColon = component("NoColon", {})
function NoColon.on_tick(self, dt)
    local _ = self.entity.has
end
local Unknown = component("Unknown", {})
function Unknown.on_tick(self, dt)
    self:fly()
end
local NoSuch = component("NoSuch", {})
function NoSuch.on_tick(self, dt)
    local _ = self.entity:has(Nothing)
end
local Spin = component("Spin", {})
function Spin.on_tick(self, dt)
    self.Transform:spin()
end
return NoColon
"#,
                ),
            ],
        );
        let mut world = World::new();
        let mut scripts = load(&dir, &mut world);
        // A parent with two children, the second with a grandchild named "Deep", and a
        // child named "Deep" further down the first.
        let parent = world.spawn((Name::new("Parent"), Transform::IDENTITY));
        let first = world.spawn((Name::new("First"), Transform::IDENTITY));
        let second = world.spawn((Name::new("Second"), Transform::IDENTITY));
        let deeper = world.spawn((Name::new("Under"), Transform::IDENTITY));
        let deepest = world.spawn((Name::new("Deep"), Transform::IDENTITY));
        let near = world.spawn((Name::new("Deep"), Transform::IDENTITY));
        world.set_parent(first, Some(parent)).unwrap();
        world.set_parent(second, Some(parent)).unwrap();
        world.set_parent(deeper, Some(first)).unwrap();
        world.set_parent(deepest, Some(deeper)).unwrap();
        world.set_parent(near, Some(second)).unwrap();
        scripts.add(&mut world, parent, "Methods");
        let turn = world.spawn((Transform::IDENTITY,));
        scripts.add(&mut world, turn, "Turn");
        world.spawn((Name::new("Eye"), Transform::from_xyz(0.0, 1.0, 0.0)));
        let witness = world.spawn((Name::new("Witness"), Transform::IDENTITY));
        let doomed = world.spawn((Transform::IDENTITY,));
        let doomed_child = world.spawn((Transform::IDENTITY,));
        world.set_parent(doomed_child, Some(doomed)).unwrap();
        scripts.add(&mut world, doomed, "Doomed");
        let wrong = world.spawn((Transform::IDENTITY,));
        for name in ["NoColon", "Unknown", "NoSuch", "Spin"] {
            scripts.add(&mut world, wrong, name);
        }

        scripts.tick(&mut world, 0.1);
        let flag = |world: &World, scripts: &Scripts, e: Entity, name: &str| {
            let id = scripts.component("Methods").unwrap();
            let TypeKind::Struct(fields) = &world.components().type_info(id).unwrap().kind else {
                unreachable!()
            };
            let at = fields.iter().find(|f| f.name == name).unwrap().offset;
            // SAFETY: a bool prop of Methods at its offset.
            unsafe { world.get_raw(e, id).unwrap().as_ptr().add(at).read() == 1 }
        };
        for name in ["has", "kind", "family", "added", "removed"] {
            assert!(
                flag(&world, &scripts, parent, name),
                "{name}: {:?}",
                scripts.reported
            );
        }
        assert_eq!(read_f32(&world, &scripts, parent, "Methods", "kids"), 2.0);
        assert_eq!(
            read_entity(&world, &scripts, parent, "Methods", "first"),
            first
        );
        assert_eq!(
            read_entity(&world, &scripts, parent, "Methods", "nearest"),
            near,
            "the nearest of two of a name"
        );
        assert_eq!(read_f32(&world, &scripts, parent, "Methods", "own"), 2.0);
        let size = world.get::<crate::Trigger>(parent).unwrap().size;
        assert_eq!(size, Vec3::new(2.0, 3.0, 4.0), "added with its values");
        // Turned a quarter about y: forward (-z) to -x, right (x) to -z.
        let t = world.get::<Transform>(turn).unwrap().rotation;
        assert!((t * Vec3::NEG_Z - Vec3::NEG_X).length() < 1e-5, "{t:?}");
        let vec = |name: &str| {
            let id = scripts.component("Turn").unwrap();
            let TypeKind::Struct(fields) = &world.components().type_info(id).unwrap().kind else {
                unreachable!()
            };
            let at = fields.iter().find(|f| f.name == name).unwrap().offset;
            // SAFETY: a vector prop of Turn at its offset.
            unsafe {
                world
                    .get_raw(turn, id)
                    .unwrap()
                    .as_ptr()
                    .add(at)
                    .cast::<Vec3>()
                    .read_unaligned()
            }
        };
        assert!((vec("forward") - Vec3::NEG_X).length() < 1e-5);
        assert!((vec("right") - Vec3::NEG_Z).length() < 1e-5);
        assert!((vec("up") - Vec3::Y).length() < 1e-5);
        assert!(
            (vec("looked") - Vec3::X).length() < 1e-5,
            "looks at the point"
        );
        assert!(world.contains(doomed));

        // The second tick: the doomed entity despawns at the stage's end, with its child,
        // after its `on_stop` saw its props and what the tick wrote.
        scripts.tick(&mut world, 0.1);
        assert!(!world.contains(doomed) && !world.contains(doomed_child));
        assert_eq!(
            world.get::<Transform>(witness).unwrap().translation,
            Vec3::new(2.0, 5.0, 0.0)
        );
        assert_eq!(read_f32(&world, &scripts, parent, "Methods", "own"), 4.0);
        assert!(
            world.get::<crate::Trigger>(parent).is_none(),
            "removed at once"
        );

        let reported: Vec<&str> = scripts.reported.iter().map(String::as_str).collect();
        for line in [
            "scripts/wrong.luau:3: `has` is a method: call it with a colon (`e:has(…)`) (NoColon.on_tick)",
            "scripts/wrong.luau:7: no method `fly`: neither an entity's nor a function of the component (Unknown.on_tick)",
            "scripts/wrong.luau:11: has takes a component (`Transform`, a script's) or its name (NoSuch.on_tick)",
            "scripts/wrong.luau:15: Transform has no method `spin` (Spin.on_tick)",
        ] {
            assert!(reported.contains(&line), "{line} in {reported:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scripts_require_each_other_by_path_and_dependents_run_again_after_a_reload() {
        let dir = project(
            "require",
            &[
                // Before the wallet in path order: it runs after it all the same.
                (
                    "coin.luau",
                    r#"local Wallet = require("./wallet")
local math2 = require("@shared/math")
local Coin = component("Coin", { version = 0, paid = false, doubled = 0 })
function Coin.on_tick(self, dt)
    self.version = Wallet.version
    self.paid = self:has(Wallet) and Wallet.name == "Wallet"
    self.doubled = math2.double(21)
end
return Coin
"#,
                ),
                (
                    "wallet.luau",
                    r#"local Wallet = component("Wallet", { coins = 0 })
Wallet.version = 1
return Wallet
"#,
                ),
                (
                    "shared/math.luau",
                    "return { double = function(x) return x * 2 end }\n",
                ),
                (
                    "npc/init.luau",
                    r#"local guard = require("@self/guard")
local Npc = component("Npc", { guarded = false })
function Npc.on_tick(self, dt)
    self.guarded = guard.on_duty
end
return Npc
"#,
                ),
                (
                    "npc/guard.luau",
                    "local math2 = require(\"../shared/math\")\nreturn { on_duty = math2.double(1) == 2 }\n",
                ),
                ("a.luau", "local B = require(\"./b\")\nreturn { b = B }\n"),
                ("b.luau", "local A = require(\"./a\")\nreturn { a = A }\n"),
            ],
        );
        std::fs::write(
            dir.join(".luaurc"),
            r#"{ "aliases": { "shared": "./scripts/shared" } }"#,
        )
        .unwrap();
        let mut world = World::new();
        let mut scripts = load(&dir, &mut world);
        let e = world.spawn((Transform::IDENTITY,));
        for name in ["Coin", "Wallet", "Npc"] {
            assert!(scripts.add(&mut world, e, name), "{name} loaded");
        }
        // The two that require each other: neither runs.
        assert!(!scripts.vm.has_module("a") || !scripts.vm.has_module("b"));
        scripts.tick(&mut world, 0.1);
        assert!(scripts.reported.is_empty(), "{:?}", scripts.reported);
        assert_eq!(read_f32(&world, &scripts, e, "Coin", "version"), 1.0);
        assert_eq!(read_f32(&world, &scripts, e, "Coin", "doubled"), 42.0);
        let flag = |world: &World, scripts: &Scripts, component: &str, name: &str| {
            let id = scripts.component(component).unwrap();
            let TypeKind::Struct(fields) = &world.components().type_info(id).unwrap().kind else {
                unreachable!()
            };
            let at = fields.iter().find(|f| f.name == name).unwrap().offset;
            // SAFETY: a bool prop at its offset.
            unsafe { world.get_raw(e, id).unwrap().as_ptr().add(at).read() == 1 }
        };
        assert!(flag(&world, &scripts, "Coin", "paid"));
        assert!(flag(&world, &scripts, "Npc", "guarded"));

        // The wallet changed: the coin, which requires it, runs again after it and sees
        // what the new one returned.
        let wallet = dir.join("scripts/wallet.luau");
        std::fs::write(
            &wallet,
            "local Wallet = component(\"Wallet\", { coins = 0 })\nWallet.version = 2\nreturn Wallet\n",
        )
        .unwrap();
        let reloaded = scripts
            .reload_file(&mut world, &dir, &dir.join("scripts"), &wallet)
            .unwrap();
        assert_eq!(reloaded.dependents, ["scripts/coin.luau"]);
        scripts.tick(&mut world, 0.1);
        assert_eq!(read_f32(&world, &scripts, e, "Coin", "version"), 2.0);
        assert!(scripts.reported.is_empty(), "{:?}", scripts.reported);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn systems_loop_over_queries_and_spawned_entities_are_there_at_once() {
        let dir = project(
            "systems",
            &[
                (
                    "spin.luau",
                    r#"local Spin = component("Spin", { speed = 1, turns = 0 })
local spinning = world.query(Spin, Transform)
local plain = world.query("Transform"):with(Spin):without(Trigger)
local UP = vector.create(0, 1, 0)
system("Spin", {
    on_tick = function(dt)
        for e, spin, t in spinning do
            t:rotate(UP, spin.speed * dt)
            spin.turns += 1
        end
    end,
    on_frame = function(dt, alpha)
        local n = 0
        for e in plain do
            n += 1
        end
        world.find("Counter").Transform.translation = vector.create(n, alpha, 0)
    end,
})
system("Broken", {
    on_tick = function(dt)
        error("no luck")
    end,
})
return Spin
"#,
                ),
                (
                    "spawner.luau",
                    r#"local Spawner = component("Spawner", { made = prop.entity(), at = vector.zero, named = false })
function Spawner.on_tick(self, dt)
    if self.made == nil then
        local e = world.spawn({
            Name = "Bullet",
            Transform = { translation = vector.create(1, 2, 3) },
            Spin = { speed = 2 },
        })
        self.made = e
        self.at = e.Transform.translation
        self.named = e.name == "Bullet" and e:has("Spin")
    end
    -- Queries made and dropped: collected, their views let go of.
    for i = 1, 20 do
        for e in world.query(Transform) do
            break
        end
    end
end
return Spawner
"#,
                ),
            ],
        );
        let mut world = World::new();
        let mut scripts = load(&dir, &mut world);
        let counter = world.spawn((Name::new("Counter"), Transform::IDENTITY));
        let spinners: Vec<Entity> = (0..3)
            .map(|_| {
                let e = world.spawn((Transform::IDENTITY,));
                scripts.add(&mut world, e, "Spin");
                e
            })
            .collect();
        // One with a trigger: spun, but not in the filtered query.
        world.insert(spinners[2], crate::Trigger::default());
        let spawner = world.spawn((Transform::IDENTITY,));
        scripts.add(&mut world, spawner, "Spawner");
        for _ in 0..4 {
            scripts.tick(&mut world, 0.25);
        }
        scripts.frame(&mut world, 0.016, 0.5);
        // Four ticks a quarter of a turn each (speed 1 rad/s × 0.25 s): 1 rad about y.
        for &e in &spinners {
            assert_eq!(read_f32(&world, &scripts, e, "Spin", "turns"), 4.0);
            let (axis, angle) = world.get::<Transform>(e).unwrap().rotation.to_axis_angle();
            assert!(
                (angle - 1.0).abs() < 1e-4 && axis.y > 0.99,
                "{axis} {angle}"
            );
        }
        // The spawned bullet: its components at once, spun by the system from its first
        // tick (the loop began after it was made).
        let bullet = read_entity(&world, &scripts, spawner, "Spawner", "made");
        assert!(world.contains(bullet));
        assert_eq!(
            world.get::<Transform>(bullet).unwrap().translation,
            Vec3::new(1.0, 2.0, 3.0)
        );
        assert!(world.get::<engine_ecs::GlobalTransform>(bullet).is_some());
        assert_eq!(world.get::<Name>(bullet).unwrap().as_str(), "Bullet");
        assert_eq!(read_f32(&world, &scripts, bullet, "Spin", "speed"), 2.0);
        assert_eq!(read_f32(&world, &scripts, bullet, "Spin", "turns"), 4.0);
        let id = scripts.component("Spawner").unwrap();
        let TypeKind::Struct(fields) = &world.components().type_info(id).unwrap().kind else {
            unreachable!()
        };
        let named = fields.iter().find(|f| f.name == "named").unwrap().offset;
        let base = world.get_raw(spawner, id).unwrap().as_ptr();
        // SAFETY: Spawner's `named`, a bool at its offset.
        let named = unsafe { base.add(named).read() };
        assert_eq!(named, 1);
        // The frame's system: two spinners without a trigger, and the bullet.
        assert_eq!(
            world.get::<Transform>(counter).unwrap().translation,
            Vec3::new(3.0, 0.5, 0.0)
        );
        let reported: Vec<&str> = scripts.reported.iter().map(String::as_str).collect();
        assert_eq!(
            reported,
            ["scripts/spin.luau:22: no luck (Broken.on_tick)"],
            "the broken system stopped once"
        );
        // The queries made in the ticks are collected; their views go at the next stage.
        scripts.vm.lua().gc_collect().unwrap();
        assert!(
            script_views::dead_refs() >= 80,
            "{}",
            script_views::dead_refs()
        );
        scripts.tick(&mut world, 0.25);
        assert!(script_views::dead_refs() < 20, "let go of");
        // The host gone: its VM's queries (the module's own, collected as it closes) leave
        // no references for a VM made later at the same address.
        scripts.tick(&mut world, 0.25);
        scripts.vm.lua().gc_collect().unwrap();
        drop(scripts);
        assert_eq!(script_views::dead_refs(), 0, "nothing left behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scripts_read_the_ticks_view_of_input_in_ticks_and_the_frames_in_frames() {
        use engine_input::{Binding, EventKind, Input, InputEvent, InputMap, Key};
        let dir = project(
            "input",
            &[(
                "pad.luau",
                r#"local Pad = component("Pad", { jumped = 0, held = false, x = 0, y = 0, framed = false, unknown = true })
function Pad.on_tick(self, dt)
    if input.just_pressed("jump") then
        self.jumped += 1
    end
    self.held = input.pressed("jump")
    local move = input.vector("move")
    self.x = move.x
    self.y = move.y
    self.unknown = input.pressed("nothing") or input.axis("nothing") ~= 0
end
function Pad.on_frame(self, dt, alpha)
    self.framed = input.pressed("jump")
end
return Pad
"#,
            )],
        );
        let mut world = World::new();
        let mut scripts = load(&dir, &mut world);
        let mut map = InputMap::new();
        let jump = map.add_button("jump").unwrap();
        map.bind(jump, Binding::button(Key::Space)).unwrap();
        let walk = map.add_axis2d("move").unwrap();
        map.bind(walk, Binding::wasd()).unwrap();
        world.insert_resource(Input::new(map));
        let pad = world.spawn((Transform::IDENTITY,));
        scripts.add(&mut world, pad, "Pad");
        let t0 = std::time::Instant::now();
        let at = |ms: u64| t0 + std::time::Duration::from_millis(ms);
        let key = |world: &mut World, ms: u64, key: Key, pressed: bool| {
            world.resource_mut::<Input>().unwrap().handle(&InputEvent {
                time: at(ms),
                kind: EventKind::Key { key, pressed },
            });
        };
        let flag = |world: &World, scripts: &Scripts, name: &str| {
            let id = scripts.component("Pad").unwrap();
            let TypeKind::Struct(fields) = &world.components().type_info(id).unwrap().kind else {
                unreachable!()
            };
            let at = fields.iter().find(|f| f.name == name).unwrap().offset;
            let base = world.get_raw(pad, id).unwrap().as_ptr();
            // SAFETY: a bool prop of Pad at its offset.
            unsafe { base.add(at).read() == 1 }
        };
        // Space and D down: the tick sees the press once, and the move to the right.
        key(&mut world, 1, Key::Space, true);
        key(&mut world, 1, Key::KeyD, true);
        world.resource_mut::<Input>().unwrap().sample_tick(at(2));
        scripts.tick(&mut world, 0.1);
        assert_eq!(read_f32(&world, &scripts, pad, "Pad", "jumped"), 1.0);
        assert!(flag(&world, &scripts, "held"));
        assert_eq!(read_f32(&world, &scripts, pad, "Pad", "x"), 1.0);
        assert_eq!(read_f32(&world, &scripts, pad, "Pad", "y"), 0.0);
        assert!(
            !flag(&world, &scripts, "unknown"),
            "an unknown action is released"
        );
        // Still held at the next tick: pressed, not just pressed.
        world.resource_mut::<Input>().unwrap().sample_tick(at(20));
        scripts.tick(&mut world, 0.1);
        assert_eq!(read_f32(&world, &scripts, pad, "Pad", "jumped"), 1.0);
        // The frames' own view: held in a frame, then let go (a frame that saw it go still
        // latches the press; the one after does not). The tick's view is not sampled again.
        world.resource_mut::<Input>().unwrap().sample_frame(at(22));
        scripts.frame(&mut world, 0.016, 0.5);
        assert!(flag(&world, &scripts, "framed"));
        key(&mut world, 25, Key::Space, false);
        world.resource_mut::<Input>().unwrap().sample_frame(at(30));
        world.resource_mut::<Input>().unwrap().sample_frame(at(40));
        scripts.frame(&mut world, 0.016, 0.5);
        assert!(!flag(&world, &scripts, "framed"));
        assert!(world.resource::<Input>().unwrap().tick().pressed("jump"));
        assert!(scripts.reported.is_empty(), "{:?}", scripts.reported);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn messages_are_delivered_after_their_hook_and_timers_when_due() {
        let dir = project(
            "messages",
            &[
                (
                    "bell.luau",
                    r#"local Bell = component("Bell", { rung = 0, ok = false, echoed = 0, late = 0, timed = 0, at = 0 })
function Bell.ring(self, word, who, n)
    self.rung += 1
    self.ok = word == "dong" and who == world.find("Ringer") and n == 7
    if self.rung == 1 then
        self:send("echo")
        self:after(0.25, "late", 3)
    end
end
function Bell.echo(self)
    self.echoed += 1
end
function Bell.late(self, n)
    self.late = n
    self.timed += 1
    self.at = time.tick()
end
return Bell
"#,
                ),
                (
                    "ringer.luau",
                    r#"local Ringer = component("Ringer", { sent = 0, heard = 0 })
function Ringer.on_tick(self, dt)
    if self.sent < 3 then
        world.find("Bell"):send("ring", "dong", self.entity, 7)
        self.sent += 1
        -- Delivered after this hook: not heard yet.
        self.heard = world.find("Bell").Bell.rung
    end
    if self.sent == 1 then
        self.entity:send("nobody_has_this")
    end
end
return Ringer
"#,
                ),
                (
                    "pinger.luau",
                    r#"local Pinger = component("Pinger", { pings = 0 })
function Pinger.on_tick(self, dt)
    if self.pings == 0 then
        self:send("ping")
    end
end
function Pinger.ping(self)
    self.pings += 1
    self:send("ping")
end
return Pinger
"#,
                ),
                (
                    "bad.luau",
                    r#"local Table = component("Table", {})
function Table.on_tick(self, dt)
    self:send("x", {})
end
local Many = component("Many", {})
function Many.on_tick(self, dt)
    self:send("x", 1, 2, 3, 4, 5)
end
return Table
"#,
                ),
            ],
        );
        let mut world = World::new();
        let mut scripts = load(&dir, &mut world);
        let ringer = world.spawn((Name::new("Ringer"), Transform::IDENTITY));
        scripts.add(&mut world, ringer, "Ringer");
        let bell = world.spawn((Name::new("Bell"), Transform::IDENTITY));
        scripts.add(&mut world, bell, "Bell");
        let pinger = world.spawn((Transform::IDENTITY,));
        scripts.add(&mut world, pinger, "Pinger");
        let bad = world.spawn((Transform::IDENTITY,));
        scripts.add(&mut world, bad, "Table");
        scripts.add(&mut world, bad, "Many");
        scripts.tick(&mut world, 0.1);
        assert_eq!(read_f32(&world, &scripts, ringer, "Ringer", "heard"), 0.0);
        assert_eq!(read_f32(&world, &scripts, bell, "Bell", "rung"), 1.0);
        assert_eq!(read_f32(&world, &scripts, bell, "Bell", "echoed"), 1.0);
        // A loop of messages: 16 passes, the rest the next stage.
        assert_eq!(read_f32(&world, &scripts, pinger, "Pinger", "pings"), 16.0);
        for _ in 0..4 {
            scripts.tick(&mut world, 0.1);
        }
        assert_eq!(read_f32(&world, &scripts, pinger, "Pinger", "pings"), 80.0);
        assert_eq!(read_f32(&world, &scripts, bell, "Bell", "rung"), 3.0);
        assert_eq!(read_f32(&world, &scripts, bell, "Bell", "echoed"), 1.0);
        // Set at tick 1, a quarter second at 0.1 s a tick: due at the start of tick 4.
        assert_eq!(read_f32(&world, &scripts, bell, "Bell", "late"), 3.0);
        assert_eq!(read_f32(&world, &scripts, bell, "Bell", "timed"), 1.0);
        assert_eq!(read_f32(&world, &scripts, bell, "Bell", "at"), 4.0);
        let id = scripts.component("Bell").unwrap();
        let TypeKind::Struct(fields) = &world.components().type_info(id).unwrap().kind else {
            unreachable!()
        };
        let ok = fields.iter().find(|f| f.name == "ok").unwrap().offset;
        let base = world.get_raw(bell, id).unwrap().as_ptr();
        // SAFETY: Bell's `ok`, a bool at its offset.
        let ok = unsafe { base.add(ok).read() };
        assert_eq!(ok, 1, "the values arrived as sent");
        let reported: Vec<&str> = scripts.reported.iter().map(String::as_str).collect();
        for line in [
            "scripts: a message `nobody_has_this` reached Ringer, and none of its scripts has it",
            "scripts: messages still sent after 16 passes (do scripts send to each other in a loop?); the rest wait for the next stage",
            "scripts/bad.luau:3: send carries plain values (nil, booleans, numbers, strings, vectors, entities), not a table (Table.on_tick)",
            "scripts/bad.luau:7: send carries at most 4 values, not 5 (Many.on_tick)",
        ] {
            assert!(reported.contains(&line), "{line} in {reported:?}");
        }
        assert_eq!(reported.len(), 4, "{reported:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn random_draws_are_the_worlds_and_sounds_play_once() {
        let dir = project(
            "random",
            &[(
                "dice.luau",
                r#"local Dice = component("Dice", { roll = 0, pick = 0, inside = false })
function Dice.on_tick(self, dt)
    self.roll = random.integer(1, 6)
    self.pick = random.number()
    self.inside = vector.magnitude(random.vector_in_sphere()) <= 1
end
local Sounder = component("Sounder", { played = prop.entity(), placed = prop.entity() })
function Sounder.on_tick(self, dt)
    if self.played == nil then
        self.played = audio.play("ui/click.ogg")
        self.placed = audio.play_at("sfx/shot.wav", vector.create(1, 2, 3), "voice")
    end
end
return Dice
"#,
            )],
        );
        let run = |ticks: usize| {
            let mut world = World::new();
            let mut scripts = load(&dir, &mut world);
            let dice = world.spawn((Transform::IDENTITY,));
            scripts.add(&mut world, dice, "Dice");
            let mut rolls = Vec::new();
            for _ in 0..ticks {
                scripts.tick(&mut world, 0.1);
                rolls.push(read_f32(&world, &scripts, dice, "Dice", "pick"));
            }
            assert!(scripts.reported.is_empty(), "{:?}", scripts.reported);
            (world, scripts, dice, rolls)
        };
        // The same seed on two worlds: the same draws.
        let (_, _, _, a) = run(5);
        let (mut world, mut scripts, dice, b) = run(5);
        assert_eq!(a, b);
        assert!(a.windows(2).any(|w| w[0] != w[1]), "{a:?}");
        let roll = read_f32(&world, &scripts, dice, "Dice", "roll");
        assert!((1.0..=6.0).contains(&roll) && roll.fract() == 0.0);
        // The generator is a component: a snapshot taken before a tick gives that tick's
        // draws again (prediction's rollback).
        let mut snapshot = engine_ecs::Snapshot::new();
        world.save(&mut snapshot).unwrap();
        scripts.tick(&mut world, 0.1);
        let first = read_f32(&world, &scripts, dice, "Dice", "pick");
        world.restore(&snapshot).unwrap();
        scripts.tick(&mut world, 0.1);
        assert_eq!(read_f32(&world, &scripts, dice, "Dice", "pick"), first);
        // The sounds: an entity each, playing once and despawned at their ends.
        let sounder = world.spawn((Transform::IDENTITY,));
        scripts.add(&mut world, sounder, "Sounder");
        scripts.tick(&mut world, 0.1);
        assert!(scripts.reported.is_empty(), "{:?}", scripts.reported);
        let played = read_entity(&world, &scripts, sounder, "Sounder", "played");
        let source = world.get::<crate::AudioSource>(played).unwrap();
        assert_eq!(
            (source.sound.as_str(), source.bus.as_str(), source.mode),
            ("ui/click.ogg", "ui", crate::PlayMode::Despawn)
        );
        let placed = read_entity(&world, &scripts, sounder, "Sounder", "placed");
        let source = world.get::<crate::AudioSource3D>(placed).unwrap();
        assert_eq!(
            (
                source.source.sound.as_str(),
                source.source.bus.as_str(),
                source.source.mode
            ),
            ("sfx/shot.wav", "voice", crate::PlayMode::Despawn)
        );
        assert_eq!(
            world.get::<Transform>(placed).unwrap().translation,
            Vec3::new(1.0, 2.0, 3.0)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prefabs_are_placed_by_asset_or_path_and_their_scripts_run() {
        use engine_scene::{AssetId, AssetRef, SaveOptions, SceneId, SceneRegistry};
        let dir = project(
            "instantiate",
            &[
                ("door.luau", DOOR),
                (
                    "spawner.luau",
                    r#"local Spawner = component("Spawner", {
    prefab = prop.asset("scene"), made = prop.entity(), by_path = prop.entity(),
    named = false, same = false, lid = prop.entity(),
})
function Spawner.on_tick(self, dt)
    if self.made == nil then
        self.made = world.instantiate(self.prefab, vector.create(5, 0, 0))
        self.by_path = world.instantiate("props/crate.scene")
        self.named = tostring(self.prefab) == "props/crate.scene"
        self.same = self.prefab == self.prefab and typeof(self.prefab) == "Asset"
        self.lid = self.made:find_child("Lid")
    end
end
return Spawner
"#,
                ),
            ],
        );
        // The prefab: a crate (a door script on it) and its lid, saved as a scene.
        let id = AssetId::new();
        {
            let mut world = World::new();
            let scripts = load(&dir, &mut world);
            let crate_ = world.spawn((
                SceneId::new(),
                Name::new("Crate"),
                Transform::from_xyz(1.0, 0.0, 0.0),
            ));
            assert!(scripts.add(&mut world, crate_, "Door"));
            let door = scripts.component("Door").unwrap();
            let TypeKind::Struct(fields) = &world.components().type_info(door).unwrap().kind else {
                unreachable!()
            };
            let lift = fields.iter().find(|f| f.name == "lift").unwrap().offset;
            let base = world.get_raw_mut(crate_, door).unwrap().as_ptr();
            // SAFETY: Door's `lift`, an f32 at its offset.
            unsafe { base.add(lift).cast::<f32>().write_unaligned(3.0) };
            let lid = world.spawn((SceneId::new(), Name::new("Lid"), Transform::IDENTITY));
            world.set_parent(lid, Some(crate_)).unwrap();
            let mut text = String::new();
            let options = SaveOptions {
                id: Some(id),
                assets: None,
                prefabs: None,
            };
            engine_scene::save_with(
                &mut world,
                &SceneRegistry::from_program(),
                &options,
                &mut text,
            )
            .unwrap();
            std::fs::create_dir_all(dir.join("props")).unwrap();
            std::fs::write(dir.join("props/crate.scene"), text).unwrap();
        }
        let mut world = World::new();
        let mut scripts = load(&dir, &mut world);
        let spawner = world.spawn((Transform::IDENTITY,));
        scripts.add(&mut world, spawner, "Spawner");
        let sp = scripts.component("Spawner").unwrap();
        let TypeKind::Struct(fields) = &world.components().type_info(sp).unwrap().kind else {
            unreachable!()
        };
        let at = |name: &str| fields.iter().find(|f| f.name == name).unwrap().offset;
        let (prefab, named, same) = (at("prefab"), at("named"), at("same"));
        let base = world.get_raw_mut(spawner, sp).unwrap().as_ptr();
        // SAFETY: Spawner's `prefab`, an asset reference at its offset.
        unsafe {
            base.add(prefab)
                .cast::<AssetRef>()
                .write_unaligned(AssetRef { file: id, part: 0 });
        }
        scripts.tick(&mut world, 0.1);
        assert!(scripts.reported.is_empty(), "{:?}", scripts.reported);
        let made = read_entity(&world, &scripts, spawner, "Spawner", "made");
        let by_path = read_entity(&world, &scripts, spawner, "Spawner", "by_path");
        for (e, x) in [(made, 6.0), (by_path, 1.0)] {
            assert_eq!(world.get::<Name>(e).unwrap().as_str(), "Crate");
            assert_eq!(world.get::<Transform>(e).unwrap().translation.x, x);
            assert!(world.get::<SceneId>(e).is_none(), "anonymous");
            assert_eq!(read_f32(&world, &scripts, e, "Door", "lift"), 3.0);
        }
        let lid = read_entity(&world, &scripts, spawner, "Spawner", "lid");
        assert_eq!(world.parent(lid), Some(made));
        assert!(world.get::<SceneId>(lid).is_none());
        let base = world.get_raw(spawner, sp).unwrap().as_ptr();
        // SAFETY: Spawner's `named` and `same`, bools at their offsets.
        let (named, same) = unsafe { (base.add(named).read(), base.add(same).read()) };
        assert_eq!((named, same), (1, 1), "the asset's path and equality");
        // The doors' hooks start at the next stage: closed where they stand.
        scripts.tick(&mut world, 0.1);
        let closed = fields_of(&world, &scripts, "Door", "closed");
        // SAFETY: Door's `closed`, a vector at its offset.
        let at_made = unsafe {
            world
                .get_raw(made, scripts.component("Door").unwrap())
                .unwrap()
                .as_ptr()
                .add(closed)
                .cast::<Vec3>()
                .read_unaligned()
        };
        assert_eq!(at_made, Vec3::new(6.0, 0.0, 0.0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The offset of field `name` of script component `component`.
    fn fields_of(world: &World, scripts: &Scripts, component: &str, name: &str) -> usize {
        let id = scripts.component(component).unwrap();
        let TypeKind::Struct(fields) = &world.components().type_info(id).unwrap().kind else {
            unreachable!()
        };
        fields.iter().find(|f| f.name == name).unwrap().offset
    }

    /// Script component `component`'s text prop `name` on `e`.
    fn read_text(
        world: &World,
        scripts: &Scripts,
        e: Entity,
        component: &str,
        name: &str,
    ) -> String {
        let at = fields_of(world, scripts, component, name);
        let base = world
            .get_raw(e, scripts.component(component).unwrap())
            .unwrap();
        // SAFETY: the component's live value, a `String` at the prop's offset.
        unsafe { engine_reflect::read_str(base.as_ptr().add(at)) }.to_owned()
    }

    #[test]
    fn text_props_are_the_components_own_through_scenes_saves_and_reloads() {
        const BELL: &str = r#"--!strict
local Bell = component("Bell", {
    sound = "ui/bell.ogg",
    label = prop.string("a bell", { tooltip = "What it says" }),
    rings = 0,
})
function Bell.on_tick(self, dt)
    if self.sound == "ui/bell.ogg" then
        self.rings += 1
    end
    if self.rings == 2 then
        self.sound = "ui/gong.ogg"
    end
    self.label = "rung"
end
return Bell
"#;
        let dir = project("texts", &[("bell.luau", BELL)]);
        let mut world = World::new();
        let mut scripts = load(&dir, &mut world);
        let id = scripts.component("Bell").unwrap();
        assert!(!world.components().is_pod(id), "it owns its texts");
        let bell = world.spawn((
            engine_scene::SceneId::new(),
            Name::new("Bell"),
            Transform::IDENTITY,
        ));
        assert!(scripts.add(&mut world, bell, "Bell"));
        assert_eq!(
            read_text(&world, &scripts, bell, "Bell", "sound"),
            "ui/bell.ogg"
        );
        assert_eq!(read_text(&world, &scripts, bell, "Bell", "label"), "a bell");
        // Read and written by its hooks.
        for _ in 0..3 {
            scripts.tick(&mut world, 0.1);
        }
        assert!(scripts.reported.is_empty(), "{:?}", scripts.reported);
        assert_eq!(
            read_text(&world, &scripts, bell, "Bell", "sound"),
            "ui/gong.ogg"
        );
        assert_eq!(read_text(&world, &scripts, bell, "Bell", "label"), "rung");
        assert_eq!(read_f32(&world, &scripts, bell, "Bell", "rings"), 2.0);
        // A scene keeps them as text, and gives them back.
        let mut text = String::new();
        let options = engine_scene::SaveOptions {
            id: Some(engine_scene::AssetId::new()),
            assets: None,
            prefabs: None,
        };
        engine_scene::save_with(
            &mut world,
            &engine_scene::SceneRegistry::from_program(),
            &options,
            &mut text,
        )
        .unwrap();
        assert!(text.contains("Bell.sound = \"ui/gong.ogg\""), "{text}");
        let mut loaded = World::new();
        let loaded_scripts = load(&dir, &mut loaded);
        engine_scene::load(
            &mut loaded,
            &engine_scene::SceneRegistry::from_program(),
            &text,
        )
        .unwrap();
        let mut q = engine_ecs::QueryState::<(Entity, &Name)>::new(&mut loaded);
        let again = q
            .iter_readonly(&loaded)
            .find(|(_, n)| n.as_str() == "Bell")
            .unwrap()
            .0;
        assert_eq!(
            read_text(&loaded, &loaded_scripts, again, "Bell", "sound"),
            "ui/gong.ogg"
        );
        assert_eq!(
            read_text(&loaded, &loaded_scripts, again, "Bell", "label"),
            "rung"
        );
        drop(loaded);
        // A save too.
        let image = engine_save::capture(&world, &engine_save::CaptureOptions::default());
        let bytes =
            engine_save::encode(&image, engine_save::Encoding::Text, &Default::default()).unwrap();
        let save = engine_save::decode(&bytes).unwrap();
        let mut saved = World::new();
        let saved_scripts = load(&dir, &mut saved);
        let report = engine_save::apply(&mut saved, &save).unwrap();
        assert!(
            !report.notes.iter().any(|n| n.contains("Bell")),
            "{:?}",
            report.notes
        );
        let mut q = engine_ecs::QueryState::<(Entity, &Name)>::new(&mut saved);
        let again = q
            .iter_readonly(&saved)
            .find(|(_, n)| n.as_str() == "Bell")
            .unwrap()
            .0;
        assert_eq!(
            read_text(&saved, &saved_scripts, again, "Bell", "sound"),
            "ui/gong.ogg"
        );
        drop(saved);
        // Reloaded with a prop taken away and one added: the texts kept move, the new one
        // has its default.
        let file = dir.join("scripts/bell.luau");
        let changed = BELL
            .replace(
                "    label = prop.string(\"a bell\", { tooltip = \"What it says\" }),\n",
                "    note = \"hello\",\n",
            )
            .replace("    self.label = \"rung\"\n", "");
        std::fs::write(&file, changed).unwrap();
        scripts
            .reload_file(&mut world, &dir, &dir.join("scripts"), &file)
            .unwrap();
        assert_ne!(scripts.component("Bell").unwrap(), id, "a new layout");
        assert_eq!(
            read_text(&world, &scripts, bell, "Bell", "sound"),
            "ui/gong.ogg"
        );
        assert_eq!(read_text(&world, &scripts, bell, "Bell", "note"), "hello");
        assert_eq!(read_f32(&world, &scripts, bell, "Bell", "rings"), 2.0);
        // A text prop takes strings only.
        let lua = Lua::new();
        let declared = lua.create_table().unwrap();
        declared.set("__prop", "string").unwrap();
        declared.set("default", 3).unwrap();
        let why = prop_of("Bad", String::from("sound"), Value::Table(declared)).unwrap_err();
        assert!(
            why.to_string()
                .contains("prop.string's default is a string"),
            "{why}"
        );
        // Dropped with their entity.
        assert!(world.despawn(bell));
        assert!(scripts.reported.is_empty(), "{:?}", scripts.reported);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn props_say_how_the_network_sends_them() {
        let dir = project(
            "net-props",
            &[(
                "gate.luau",
                r#"return component("Gate", {
    lift = prop.number(0, { net_quantum = 0.01 }),
    spin = prop.number(0, { net_skip = true }),
    open = false,
})
"#,
            )],
        );
        let mut world = World::new();
        let scripts = load(&dir, &mut world);
        let id = scripts.component("Gate").unwrap();
        let TypeKind::Struct(fields) = &world.components().type_info(id).unwrap().kind else {
            unreachable!()
        };
        let attrs = |name: &str| fields.iter().find(|f| f.name == name).unwrap().attrs;
        assert_eq!(attrs("lift").net_quantum, Some(0.01));
        assert!(!attrs("lift").net_skip);
        assert!(attrs("spin").net_skip);
        assert_eq!(attrs("open").net_quantum, None);
        let lua = Lua::new();
        let declared = lua.create_table().unwrap();
        let opts = lua.create_table().unwrap();
        opts.set("net_quantum", 0).unwrap();
        declared.set("__prop", "number").unwrap();
        declared.set("default", 1).unwrap();
        declared.set("opts", opts).unwrap();
        let why = prop_of("Gate", String::from("lift"), Value::Table(declared)).unwrap_err();
        assert!(
            why.to_string().contains("net_quantum is a positive number"),
            "{why}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn logpoints_and_pause_points_in_a_components_hook() {
        let dir = project("logpoints", &[("door.luau", DOOR)]);
        let mut world = World::new();
        let mut scripts = load(&dir, &mut world);
        assert_eq!(
            scripts.module_of(Path::new("scripts/door.luau")),
            Some("door")
        );
        let door = world.spawn((Transform::IDENTITY,));
        world.spawn((Name::new("Player"), Transform::from_xyz(1.0, 0.0, 0.0)));
        scripts.add(&mut world, door, "Door");
        // Line 24: `self.opened = near`, where `near` and `step` are known.
        scripts.set_breakpoints(
            "door",
            vec![
                engine_script::Breakpoint {
                    line: 24,
                    action: engine_script::BreakAction::Log(String::from(
                        "near {near} lift {self.lift}",
                    )),
                    condition: None,
                    hits: engine_script::Hits::Every,
                    enabled: true,
                    only: Vec::new(),
                },
                engine_script::Breakpoint {
                    line: 24,
                    action: engine_script::BreakAction::Pause,
                    condition: Some(String::from("near and self.lift > 2")),
                    hits: engine_script::Hits::Every,
                    enabled: true,
                    only: Vec::new(),
                },
            ],
        );
        scripts.tick(&mut world, 0.1);
        assert!(scripts.reported.is_empty(), "{:?}", scripts.reported);
        let states = scripts.breakpoint_states("door");
        assert_eq!(states[0].placed, Some(24));
        assert_eq!(states[0].last.as_deref(), Some("near true lift 2.5"));
        assert!(scripts.debug_pause_asked());
        assert_eq!(
            scripts.take_debug_pause(),
            Some((String::from("scripts/door.luau"), 24)),
            "the pause point's condition held"
        );
        assert!(scripts.take_debug_pause().is_none());
        assert_eq!(
            module_name(Path::new("scripts/npc/guard.luau")).as_deref(),
            Some("npc/guard")
        );
        assert_eq!(module_name(Path::new("shaders/water.slang")), None);
        assert_eq!(module_name(Path::new("notes/door.luau")), None);
        // Kept through a reload of the script.
        let file = dir.join("scripts/door.luau");
        std::fs::write(&file, DOOR.replace("lift = 2.5", "lift = 3")).unwrap();
        scripts
            .reload_file(&mut world, &dir, &dir.join("scripts"), &file)
            .unwrap();
        scripts.tick(&mut world, 0.1);
        let states = scripts.breakpoint_states("door");
        assert_eq!(states[0].hits, 2);
        assert!(
            states[0]
                .last
                .as_deref()
                .is_some_and(|l| l.starts_with("near true"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_logpoint_in_on_tick_logs_at_every_tick_of_every_instance() {
        let hook = "local Spin = component(\"Spin\", { speed = 1.5 })
local UP = vector.create(0, 1, 0)
function Spin.on_tick(self, dt)
    self.Transform:rotate(UP, self.speed * dt)
end
return Spin
";
        let dir = project("logpoint-ticks", &[("spin.luau", hook)]);
        let mut world = World::new();
        let mut scripts = load(&dir, &mut world);
        for i in 0..3 {
            let e = world.spawn((Transform::from_xyz(i as f32, 0.0, 0.0),));
            scripts.add(&mut world, e, "Spin");
        }
        scripts.set_breakpoints(
            "spin",
            vec![engine_script::Breakpoint {
                line: 4,
                action: engine_script::BreakAction::Log(String::from("speed {self.speed}")),
                condition: None,
                hits: engine_script::Hits::Every,
                enabled: true,
                only: Vec::new(),
            }],
        );
        // Line 4's first instruction reads `self.Transform` through Luau's userdata
        // instruction, whose slow path writes its own opcode back over the breakpoint: it
        // is placed again after each hit.
        let mut seen = Vec::new();
        for _ in 0..4 {
            scripts.tick(&mut world, 1.0 / 64.0);
            seen.push(scripts.breakpoint_states("spin")[0].hits);
        }
        assert!(scripts.reported.is_empty(), "{:?}", scripts.reported);
        assert_eq!(seen, [3, 6, 9, 12], "every instance, every tick");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ADR 0119's costs (`--release -- --ignored --nocapture`): 1,000 instances of a
    /// one-line `on_tick`, with no breakpoint, a logpoint turned off, a pause point whose
    /// condition never holds, a logpoint on every 64th hit, and one logging every hit;
    /// medians of 51 ticks.
    #[test]
    #[ignore = "a measurement"]
    fn measure_breakpoints() {
        let hook = "local Spin = component(\"Spin\", { speed = 1.5 })
local UP = vector.create(0, 1, 0)
function Spin.on_tick(self, dt)
    self.Transform:rotate(UP, self.speed * dt)
end
return Spin
";
        let dir = project("measure-breakpoints", &[("spin.luau", hook)]);
        let log =
            |text: &str, hits: engine_script::Hits, enabled: bool| engine_script::Breakpoint {
                line: 4,
                action: engine_script::BreakAction::Log(text.to_owned()),
                condition: None,
                hits,
                enabled,
                only: Vec::new(),
            };
        let cases: [(&str, Vec<engine_script::Breakpoint>); 5] = [
            ("no breakpoint", Vec::new()),
            (
                "a logpoint, off",
                vec![log("speed {self.speed}", engine_script::Hits::Every, false)],
            ),
            (
                "a pause point, its condition false",
                vec![engine_script::Breakpoint {
                    line: 4,
                    action: engine_script::BreakAction::Pause,
                    condition: Some(String::from("self.speed > 100")),
                    hits: engine_script::Hits::Every,
                    enabled: true,
                    only: Vec::new(),
                }],
            ),
            (
                "a logpoint on every 64th hit",
                vec![log(
                    "speed {self.speed}",
                    engine_script::Hits::EveryNth(64),
                    true,
                )],
            ),
            (
                "a logpoint on every hit",
                vec![log("speed {self.speed}", engine_script::Hits::Every, true)],
            ),
        ];
        println!("| 1,000 instances | tick | per instance |");
        println!("|---|---:|---:|");
        let mut base = 0.0;
        for (name, breakpoints) in cases {
            let mut world = World::new();
            let mut scripts = load(&dir, &mut world);
            for i in 0..1000 {
                let e = world.spawn((Transform::from_xyz(i as f32, 0.0, 0.0),));
                scripts.add(&mut world, e, "Spin");
            }
            scripts.set_breakpoints("spin", breakpoints);
            scripts.tick(&mut world, 1.0 / 64.0);
            let mut times: Vec<f64> = (0..51)
                .map(|_| {
                    let t = std::time::Instant::now();
                    scripts.tick(&mut world, 1.0 / 64.0);
                    t.elapsed().as_secs_f64() * 1e6
                })
                .collect();
            assert!(scripts.reported.is_empty(), "{:?}", scripts.reported);
            assert!(scripts.take_debug_pause().is_none());
            times.sort_by(f64::total_cmp);
            let median = times[times.len() / 2];
            if base == 0.0 {
                base = median;
            }
            let hits = scripts
                .breakpoint_states("spin")
                .first()
                .map_or(0, |s| s.hits);
            println!(
                "| {name} ({hits} hits) | {:.3} ms | {:.2} µs (+{:.2}) |",
                median / 1e3,
                median / 1e3,
                (median - base) / 1e3
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn script_components_and_their_timers_come_back_with_a_save() {
        let dir = project(
            "saves",
            &[(
                "keeper.luau",
                r#"local Keeper = component("Keeper", { count = 0, friend = prop.entity(), loads = 0, rang = 0, word = false })
function Keeper.on_tick(self, dt)
    self.count += 1
    if self.count == 2 then
        self:after(0.5, "ring", "dong", self.friend)
    end
end
function Keeper.ring(self, word, who)
    self.rang += 1
    self.word = word == "dong" and who == self.friend and who.name == "Friend"
end
function Keeper.on_load(self)
    self.loads += 1
end
return Keeper
"#,
            )],
        );
        let keeper_of = |world: &World, scripts: &Scripts| {
            let id = scripts.component("Keeper").unwrap();
            world
                .archetypes()
                .iter()
                .filter(|a| a.components().contains(&id))
                .find_map(|a| {
                    (0..a.chunk_count()).find_map(|c| a.chunk_entities(c).first().copied())
                })
                .unwrap()
        };
        // Three ticks: the timer set at the second, due five ticks later (0.5 s at 0.1 s).
        let mut world = World::new();
        let mut scripts = load(&dir, &mut world);
        let friend = world.spawn((Name::new("Friend"), Transform::IDENTITY));
        let keeper = world.spawn((Transform::IDENTITY,));
        scripts.add(&mut world, keeper, "Keeper");
        let id = scripts.component("Keeper").unwrap();
        let TypeKind::Struct(fields) = &world.components().type_info(id).unwrap().kind else {
            unreachable!()
        };
        let at = |name: &str| fields.iter().find(|f| f.name == name).unwrap().offset;
        let (friend_at, word_at) = (at("friend"), at("word"));
        let base = world.get_raw_mut(keeper, id).unwrap().as_ptr();
        // SAFETY: Keeper's `friend`, an entity at its offset.
        unsafe { base.add(friend_at).cast::<Entity>().write_unaligned(friend) };
        for _ in 0..3 {
            scripts.tick(&mut world, 0.1);
        }
        // The save: the timer rides on its entity, then leaves it.
        scripts.before_save(&mut world);
        assert!(world.get::<ScriptTimers>(keeper).is_some());
        let image = engine_save::capture(&world, &engine_save::CaptureOptions::default());
        scripts.after_save(&mut world);
        assert!(
            world.get::<ScriptTimers>(keeper).is_none(),
            "plain data again"
        );
        let bytes =
            engine_save::encode(&image, engine_save::Encoding::Text, &Default::default()).unwrap();
        let save = engine_save::decode(&bytes).unwrap();

        // A world and a host made anew, the save applied.
        let mut world = World::new();
        let mut scripts = load(&dir, &mut world);
        let report = engine_save::apply(&mut world, &save).unwrap();
        assert!(
            !report.notes.iter().any(|n| n.contains("Keeper")),
            "{:?}",
            report.notes
        );
        scripts.after_load(&mut world);
        let keeper = keeper_of(&world, &scripts);
        assert!(world.get::<ScriptTimers>(keeper).is_none());
        assert_eq!(read_f32(&world, &scripts, keeper, "Keeper", "count"), 3.0);
        let friend = read_entity(&world, &scripts, keeper, "Keeper", "friend");
        assert_eq!(world.get::<Name>(friend).unwrap().as_str(), "Friend");
        // Its `on_load` once, at the next tick; the timer four ticks after the load.
        for _ in 0..3 {
            scripts.tick(&mut world, 0.1);
        }
        assert_eq!(read_f32(&world, &scripts, keeper, "Keeper", "loads"), 1.0);
        assert_eq!(read_f32(&world, &scripts, keeper, "Keeper", "rang"), 0.0);
        scripts.tick(&mut world, 0.1);
        assert_eq!(read_f32(&world, &scripts, keeper, "Keeper", "rang"), 1.0);
        let base = world.get_raw(keeper, id).unwrap().as_ptr();
        // SAFETY: Keeper's `word`, a bool at its offset.
        let word = unsafe { base.add(word_at).read() };
        assert_eq!(word, 1, "the timer's values came back, its entity remapped");
        assert_eq!(read_f32(&world, &scripts, keeper, "Keeper", "count"), 7.0);
        assert!(scripts.reported.is_empty(), "{:?}", scripts.reported);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_clients_ticks_run_its_predicted_entities_and_reruns_start_nothing_again() {
        let dir = project(
            "net",
            &[(
                "pawn.luau",
                r#"local Pawn = component("Pawn", { ticks = 0, sounds = 0, pings = 0, server = false, client = true, again = false })
function Pawn.on_tick(self, dt)
    self.ticks += 1
    if audio.play("ui/step.ogg") then
        self.sounds += 1
    end
    self:send("ping")
    self.server = net.is_server()
    self.client = net.is_client()
    self.again = net.resimulating()
end
function Pawn.ping(self)
    self.pings += 1
end
return Pawn
"#,
            )],
        );
        let mut world = World::new();
        let mut scripts = load(&dir, &mut world);
        let mine = world.spawn((Transform::IDENTITY, engine_net::Predicted));
        let theirs = world.spawn((Transform::IDENTITY,));
        scripts.add(&mut world, mine, "Pawn");
        scripts.add(&mut world, theirs, "Pawn");
        let flag = |world: &World, scripts: &Scripts, e: Entity, name: &str| {
            let at = fields_of(world, scripts, "Pawn", name);
            let base = world
                .get_raw(e, scripts.component("Pawn").unwrap())
                .unwrap()
                .as_ptr();
            // SAFETY: a bool prop of Pawn at its offset.
            unsafe { base.add(at).read() == 1 }
        };
        // A client's tick: its own entity only.
        scripts.tick_predicted(&mut world, 0.1);
        assert_eq!(read_f32(&world, &scripts, mine, "Pawn", "ticks"), 1.0);
        assert_eq!(read_f32(&world, &scripts, theirs, "Pawn", "ticks"), 0.0);
        assert_eq!(read_f32(&world, &scripts, mine, "Pawn", "sounds"), 1.0);
        assert_eq!(read_f32(&world, &scripts, mine, "Pawn", "pings"), 1.0);
        // Alone: this side decides the world.
        assert!(flag(&world, &scripts, mine, "server") && !flag(&world, &scripts, mine, "client"));
        assert!(!flag(&world, &scripts, mine, "again"));
        // Run again after a correction: no sound, no message a second time.
        world.insert_resource(engine_net::Resimulating(true));
        scripts.tick_predicted(&mut world, 0.1);
        assert_eq!(read_f32(&world, &scripts, mine, "Pawn", "ticks"), 2.0);
        assert_eq!(read_f32(&world, &scripts, mine, "Pawn", "sounds"), 1.0);
        assert_eq!(read_f32(&world, &scripts, mine, "Pawn", "pings"), 1.0);
        assert!(flag(&world, &scripts, mine, "again"));
        let mut sounds = engine_ecs::QueryState::<&crate::AudioSource>::new(&mut world);
        assert_eq!(sounds.iter_readonly(&world).count(), 1);
        // The host's tick: every entity.
        world.insert_resource(engine_net::Resimulating(false));
        scripts.tick(&mut world, 0.1);
        assert_eq!(read_f32(&world, &scripts, mine, "Pawn", "ticks"), 3.0);
        assert_eq!(read_f32(&world, &scripts, theirs, "Pawn", "ticks"), 1.0);
        assert!(scripts.reported.is_empty(), "{:?}", scripts.reported);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scripts_query_the_physics_world_and_its_bodies_name_their_entities() {
        use engine_physics::ecs::entity_user_data;
        use engine_physics::{BodyDesc, PhysicsBackend, PhysicsSettings, ShapeDesc};
        let dir = project(
            "physics",
            &[(
                "eye.luau",
                r#"local Eye = component("Eye", { floor = false, distance = 0, near = 0, moving = 0, missed = false })
local moving = physics.filter({ "Moving" })
function Eye.on_tick(self, dt)
    local e, point, normal, distance = physics.raycast(vector.create(0, 10, 0), vector.create(0, -1, 0), 100)
    self.floor = e ~= nil and e.name == "Floor" and normal.y > 0.99 and point.y > 0.99
    self.distance = distance
    local n = 0
    for other in physics.overlap_sphere(vector.create(0, 1, 0), 3) do
        n += 1
    end
    self.near = n
    local m = physics.raycast(vector.create(3, 10, 0), vector.create(0, -1, 0), 100, moving)
    self.moving = if m ~= nil and m.name == "Ball" then 1 else 0
    self.missed = physics.raycast(vector.create(50, 10, 0), vector.create(0, -1, 0), 100) == nil
end
return Eye
"#,
            )],
        );
        let mut world = World::new();
        let mut scripts = load(&dir, &mut world);
        let mut physics = crate::ScenePhysics::new(&PhysicsSettings {
            threads: 0,
            ..PhysicsSettings::default()
        })
        .unwrap();
        // A floor 1 m thick, its top at 1 (static: layer 0), and a ball above it at x = 3
        // on the moving layer, each naming its entity.
        let floor = world.spawn((Name::new("Floor"), Transform::IDENTITY));
        let ball = world.spawn((Name::new("Ball"), Transform::from_xyz(3.0, 3.0, 0.0)));
        let w = physics.physics();
        let slab = w
            .create_shape(&ShapeDesc::Box {
                half_extents: Vec3::new(5.0, 0.5, 5.0),
            })
            .unwrap();
        w.create_body(&BodyDesc {
            user_data: entity_user_data(floor),
            ..BodyDesc::fixed(slab, Vec3::new(0.0, 0.5, 0.0))
        })
        .unwrap();
        let round = w.create_shape(&ShapeDesc::Sphere { radius: 0.5 }).unwrap();
        w.create_body(&BodyDesc {
            user_data: entity_user_data(ball),
            gravity: engine_physics::GravityMode::Scaled(0.0),
            ..BodyDesc::dynamic(round, Vec3::new(3.0, 3.0, 0.0))
        })
        .unwrap();
        let eye = world.spawn((Transform::IDENTITY,));
        scripts.add(&mut world, eye, "Eye");
        scripts.tick_with(&mut world, 0.1, Some(&physics));
        assert!(scripts.reported.is_empty(), "{:?}", scripts.reported);
        let id = scripts.component("Eye").unwrap();
        let info = world.components().type_info(id).unwrap();
        let TypeKind::Struct(fields) = &info.kind else {
            unreachable!()
        };
        let at = |name: &str| fields.iter().find(|f| f.name == name).unwrap().offset;
        let base = world.get_raw(eye, id).unwrap().as_ptr();
        // SAFETY: Eye's props at their offsets: booleans and f32s.
        unsafe {
            assert_eq!(
                base.add(at("floor")).read(),
                1,
                "the floor's entity, point and normal"
            );
            let distance = base.add(at("distance")).cast::<f32>().read_unaligned();
            assert!((distance - 9.0).abs() < 1e-3, "{distance}");
            assert_eq!(
                base.add(at("near")).cast::<f32>().read_unaligned(),
                1.0,
                "the floor"
            );
            assert_eq!(
                base.add(at("moving")).cast::<f32>().read_unaligned(),
                1.0,
                "the ball"
            );
            assert_eq!(base.add(at("missed")).read(), 1, "nothing out there");
        }
        // Without a physics world: the error says where one comes from.
        scripts.tick(&mut world, 0.1);
        assert!(
            scripts
                .reported
                .iter()
                .any(|r| r.contains("no physics world")),
            "{:?}",
            scripts.reported
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_trigger_hears_a_ball_fall_through_it_once_in_and_once_out() {
        use engine_physics::ecs::entity_user_data;
        use engine_physics::{BodyDesc, PhysicsBackend, PhysicsSettings, ShapeDesc};
        let dir = project(
            "triggers",
            &[(
                "gate.luau",
                r#"local Gate = component("Gate", { touched = 0, left = 0, who = prop.entity(), near = false })
function Gate.on_touch(self, other, point, normal)
    self.touched += 1
    self.who = other
    self.near = vector.magnitude(point) < 2
end
function Gate.on_untouch(self, other)
    if other == self.who then
        self.left += 1
    end
end
return Gate
"#,
            )],
        );
        let mut world = World::new();
        let mut scripts = load(&dir, &mut world);
        let mut physics = crate::ScenePhysics::new(&PhysicsSettings {
            threads: 0,
            ..PhysicsSettings::default()
        })
        .unwrap();
        // A 2 m box trigger at the origin, and a ball dropped through it from 3 m.
        let gate = world.spawn((
            Name::new("Gate"),
            Transform::IDENTITY,
            engine_ecs::GlobalTransform::IDENTITY,
            crate::Trigger::default().with_size(Vec3::splat(2.0)),
        ));
        scripts.add(&mut world, gate, "Gate");
        let ball = world.spawn((Name::new("Ball"), Transform::from_xyz(0.0, 3.0, 0.0)));
        let w = physics.physics();
        let round = w.create_shape(&ShapeDesc::Sphere { radius: 0.25 }).unwrap();
        w.create_body(&BodyDesc {
            user_data: entity_user_data(ball),
            ..BodyDesc::dynamic(round, Vec3::new(0.0, 3.0, 0.0))
        })
        .unwrap();
        let dt = 1.0 / 64.0;
        for _ in 0..128 {
            physics.sync_triggers(&mut world).unwrap();
            scripts.tick_with(&mut world, dt, Some(&physics));
            physics.step(&mut world, dt).unwrap();
        }
        assert!(physics.trigger_body(gate).is_some());
        assert!(scripts.reported.is_empty(), "{:?}", scripts.reported);
        let id = scripts.component("Gate").unwrap();
        let info = world.components().type_info(id).unwrap();
        let TypeKind::Struct(fields) = &info.kind else {
            unreachable!()
        };
        let at = |name: &str| fields.iter().find(|f| f.name == name).unwrap().offset;
        let base = world.get_raw(gate, id).unwrap().as_ptr();
        // SAFETY: Gate's props at their offsets: f32s, an entity and a boolean.
        unsafe {
            assert_eq!(
                base.add(at("touched")).cast::<f32>().read_unaligned(),
                1.0,
                "in once"
            );
            assert_eq!(
                base.add(at("left")).cast::<f32>().read_unaligned(),
                1.0,
                "out once"
            );
            assert_eq!(base.add(at("who")).cast::<Entity>().read_unaligned(), ball);
            assert_eq!(base.add(at("near")).read(), 1, "where it touched");
        }
        // The trigger taken away: its body goes.
        world.remove::<crate::Trigger>(gate);
        physics.sync_triggers(&mut world).unwrap();
        assert!(physics.trigger_body(gate).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn props_take_their_kinds_and_are_clamped_and_a_component_that_goes_stops() {
        let dir = project(
            "props",
            &[(
                "counter.luau",
                r#"local Counter = component("Counter", {
    count = prop.integer(0, { max = 3 }),
    target = prop.entity(),
    on = true,
})
function Counter.on_tick(self, dt)
    self.count += 1
    self.on = not self.on
    self.locals.ticks = (self.locals.ticks or 0) + 1
end
function Counter.on_stop(self)
    print("stopped after", self.locals.ticks, "ticks on", self.entity)
end
return Counter
"#,
            )],
        );
        let mut world = World::new();
        let mut scripts = load(&dir, &mut world);
        let e = world.spawn((Transform::IDENTITY,));
        scripts.add(&mut world, e, "Counter");
        for _ in 0..5 {
            scripts.tick(&mut world, 0.1);
        }
        let id = scripts.component("Counter").unwrap();
        let info = world.components().type_info(id).unwrap();
        let TypeKind::Struct(fields) = &info.kind else {
            unreachable!()
        };
        let at = |name: &str| fields.iter().find(|f| f.name == name).unwrap().offset;
        let base = world.get_raw(e, id).unwrap().as_ptr();
        // SAFETY: Counter's props at their offsets: an i32, a bool, an entity.
        unsafe {
            assert_eq!(
                base.add(at("count")).cast::<i32>().read_unaligned(),
                3,
                "clamped to 3"
            );
            assert_eq!(base.add(at("on")).read(), 0, "five flips of true");
            assert_eq!(
                base.add(at("target")).cast::<Entity>().read_unaligned(),
                Entity::NULL
            );
        }
        // The component taken off: its instance stops (on_stop ran without an error).
        world.remove_raw(e, id);
        scripts.tick(&mut world, 0.1);
        assert!(scripts.instances.is_empty());
        assert!(scripts.reported.is_empty(), "{:?}", scripts.reported);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_script_made_after_the_start_loads_once_and_the_editor_finds_its_file() {
        let dir = project(
            "later",
            &[(
                "first.luau",
                "local LaterFirst = component(\"LaterFirst\", {})\nreturn LaterFirst\n",
            )],
        );
        let mut world = World::new();
        let mut scripts = load(&dir, &mut world);
        let folder = dir.join("scripts");
        // The editor's New Script: a file written after the start.
        let file = folder.join("tools/lever.luau");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(
            &file,
            r#"local LaterLever = component("LaterLever", { pulled = false })
function LaterLever.on_tick(self, dt)
    self.pulled = true
end
return LaterLever
"#,
        )
        .unwrap();
        let names = scripts.load_file(&mut world, &dir, &folder, &file).unwrap();
        assert_eq!(names, ["LaterLever"]);
        assert_eq!(
            scripts.file_of("LaterLever"),
            Some(Path::new("scripts/tools/lever.luau"))
        );
        let again = scripts.load_file(&mut world, &dir, &folder, &file);
        assert!(again.unwrap_err().contains("loaded already"));
        // Beside the program's types, with its file: what the editor shows and opens.
        let listed = engine_ecs::runtime_types()
            .into_iter()
            .find(|t| t.info.name == "LaterLever")
            .unwrap();
        assert_eq!(&*listed.source, "scripts/tools/lever.luau");
        // Its hooks run as the others'.
        let e = world.spawn((Transform::IDENTITY,));
        assert!(scripts.add(&mut world, e, "LaterLever"));
        scripts.tick(&mut world, 0.1);
        let id = scripts.component("LaterLever").unwrap();
        // SAFETY: the lever's one prop, a bool.
        assert_eq!(unsafe { world.get_raw(e, id).unwrap().as_ptr().read() }, 1);
        // A name the program's components hold is refused (scenes could not tell them
        // apart), and a file outside the folder is not a script.
        let clash = folder.join("clash.luau");
        std::fs::write(&clash, "local T = component(\"Transform\", {})\nreturn T\n").unwrap();
        let names = scripts
            .load_file(&mut world, &dir, &folder, &clash)
            .unwrap();
        assert!(names.is_empty());
        let outside = dir.join("outside.luau");
        assert!(
            scripts
                .load_file(&mut world, &dir, &folder, &outside)
                .is_err()
        );
        assert!(scripts.reported.is_empty(), "{:?}", scripts.reported);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Prop `name` of script component `component` on `e`, an f32.
    fn read_f32(world: &World, scripts: &Scripts, e: Entity, component: &str, name: &str) -> f32 {
        let id = scripts.component(component).unwrap();
        let info = world.components().type_info(id).unwrap();
        let TypeKind::Struct(fields) = &info.kind else {
            unreachable!()
        };
        let at = fields.iter().find(|f| f.name == name).unwrap().offset;
        let base = world.get_raw(e, id).unwrap().as_ptr();
        // SAFETY: an f32 prop of the entity's live value.
        unsafe { base.add(at).cast::<f32>().read_unaligned() }
    }

    /// Prop `name` of script component `component` on `e`, an entity.
    fn read_entity(
        world: &World,
        scripts: &Scripts,
        e: Entity,
        component: &str,
        name: &str,
    ) -> Entity {
        let id = scripts.component(component).unwrap();
        let info = world.components().type_info(id).unwrap();
        let TypeKind::Struct(fields) = &info.kind else {
            unreachable!()
        };
        let at = fields.iter().find(|f| f.name == name).unwrap().offset;
        let base = world.get_raw(e, id).unwrap().as_ptr();
        // SAFETY: an entity prop of the entity's live value.
        unsafe { base.add(at).cast::<Entity>().read_unaligned() }
    }

    const RELOAD_V1: &str = r#"local D = component("ReloadDoor", { lift = 2, open = false, starts = 0, target = prop.entity() })
function D.on_start(self)
    self.starts += 1
end
function D.on_tick(self, dt)
    self.lift += 1
end
return D
"#;

    #[test]
    fn a_reloaded_script_keeps_its_instances_and_moves_changed_props_by_name() {
        let dir = project("reload", &[("door.luau", RELOAD_V1)]);
        let folder = dir.join("scripts");
        let file = folder.join("door.luau");
        let mut world = World::new();
        let mut scripts = load(&dir, &mut world);
        let e = world.spawn((Transform::IDENTITY,));
        let other = world.spawn((Transform::IDENTITY,));
        assert!(scripts.add(&mut world, e, "ReloadDoor"));
        // The door's target: the other entity, written where the prop lies.
        {
            let id = scripts.component("ReloadDoor").unwrap();
            let info = world.components().type_info(id).unwrap();
            let TypeKind::Struct(fields) = &info.kind else {
                unreachable!()
            };
            let at = fields.iter().find(|f| f.name == "target").unwrap().offset;
            let base = world.get_raw_mut(e, id).unwrap().as_ptr();
            // SAFETY: the entity prop of the entity's live value.
            unsafe { base.add(at).cast::<Entity>().write_unaligned(other) };
        }
        scripts.tick(&mut world, 0.1);
        scripts.tick(&mut world, 0.1);
        let lift = |w: &World, s: &Scripts| read_f32(w, s, e, "ReloadDoor", "lift");
        assert_eq!(lift(&world, &scripts), 4.0);
        let first = scripts.component("ReloadDoor").unwrap();

        // The same props, new hooks: in place, the instance going on.
        std::fs::write(
            &file,
            RELOAD_V1.replace("self.lift += 1", "self.lift += 10"),
        )
        .unwrap();
        let r = scripts
            .reload_file(&mut world, &dir, &folder, &file)
            .unwrap();
        assert_eq!(r.kept, ["ReloadDoor"]);
        assert!(r.relaid.is_empty());
        assert_eq!(r.instances, 1);
        assert_eq!(r.file, "scripts/door.luau");
        assert_eq!(scripts.component("ReloadDoor"), Some(first));
        scripts.tick(&mut world, 0.1);
        assert_eq!(lift(&world, &scripts), 14.0, "its value kept, the new hook");
        assert_eq!(
            read_f32(&world, &scripts, e, "ReloadDoor", "starts"),
            1.0,
            "on_start ran once, not again at the reload"
        );
        // The same text again: nothing to do.
        assert!(
            scripts
                .reload_file(&mut world, &dir, &folder, &file)
                .unwrap()
                .is_empty()
        );

        // `open` became a number and `speed` came: each value by name.
        let v3 = r#"local D = component("ReloadDoor", { lift = 2, open = 0.5, speed = 3, starts = 0, target = prop.entity() })
function D.on_start(self)
    self.starts += 1
end
function D.on_tick(self, dt)
    self.lift += self.speed
end
return D
"#;
        std::fs::write(&file, v3).unwrap();
        let r = scripts
            .reload_file(&mut world, &dir, &folder, &file)
            .unwrap();
        assert_eq!(r.relaid.len(), 1);
        let (name, relayout) = &r.relaid[0];
        assert_eq!(name, "ReloadDoor");
        assert_eq!(relayout.old, first);
        let moved = scripts.component("ReloadDoor").unwrap();
        assert_eq!(relayout.new, moved);
        assert_eq!(world.components().name(moved), Some("script::ReloadDoor#2"));
        assert!(
            world.get_raw(e, first).is_none(),
            "moved off the old layout"
        );
        assert_eq!(lift(&world, &scripts), 14.0, "kept by name");
        assert_eq!(
            read_f32(&world, &scripts, e, "ReloadDoor", "open"),
            0.5,
            "a new type: its default"
        );
        assert_eq!(
            read_f32(&world, &scripts, e, "ReloadDoor", "speed"),
            3.0,
            "new: its default"
        );
        assert_eq!(
            read_entity(&world, &scripts, e, "ReloadDoor", "target"),
            other
        );
        let listed = engine_ecs::runtime_types()
            .into_iter()
            .find(|t| t.info.name == "ReloadDoor")
            .unwrap();
        assert_eq!(
            &*listed.component, "script::ReloadDoor#2",
            "scenes find the new layout"
        );
        scripts.tick(&mut world, 0.1);
        assert_eq!(lift(&world, &scripts), 17.0, "the instance went on");

        // A syntax error: the old version runs on, the error names the file and line.
        std::fs::write(&file, "local x = = 1\n").unwrap();
        let why = scripts
            .reload_file(&mut world, &dir, &folder, &file)
            .unwrap_err();
        assert!(why.starts_with("scripts/door.luau:1"), "{why}");
        scripts.tick(&mut world, 0.1);
        assert_eq!(lift(&world, &scripts), 20.0);

        // A runtime error stops the instance; the fix starts it again.
        std::fs::write(
            &file,
            v3.replace("self.lift += self.speed", "error(\"jammed\")"),
        )
        .unwrap();
        scripts
            .reload_file(&mut world, &dir, &folder, &file)
            .unwrap();
        scripts.tick(&mut world, 0.1);
        assert!(!scripts.reported.is_empty());
        std::fs::write(&file, v3).unwrap();
        let r = scripts
            .reload_file(&mut world, &dir, &folder, &file)
            .unwrap();
        assert_eq!(r.instances, 1);
        assert!(scripts.reported.is_empty(), "its errors may come again");
        scripts.tick(&mut world, 0.1);
        assert_eq!(lift(&world, &scripts), 23.0, "started again");

        // No longer declared: the data stays, the hooks go.
        std::fs::write(&file, "return {}\n").unwrap();
        let r = scripts
            .reload_file(&mut world, &dir, &folder, &file)
            .unwrap();
        assert_eq!(r.gone, ["ReloadDoor"]);
        scripts.tick(&mut world, 0.1);
        assert_eq!(lift(&world, &scripts), 23.0);
        assert!(world.get_raw(e, moved).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "a measurement"]
    fn measure_reload() {
        let source = RELOAD_V1.replace("ReloadDoor", "MeasuredDoor");
        let dir = project("reload-measure", &[("door.luau", &source)]);
        let folder = dir.join("scripts");
        let file = folder.join("door.luau");
        let mut world = World::new();
        let mut scripts = load(&dir, &mut world);
        for _ in 0..1000 {
            let e = world.spawn((Transform::IDENTITY,));
            scripts.add(&mut world, e, "MeasuredDoor");
        }
        scripts.tick(&mut world, 0.1);
        std::fs::write(&file, source.replace("self.lift += 1", "self.lift += 2")).unwrap();
        let in_place = scripts
            .reload_file(&mut world, &dir, &folder, &file)
            .unwrap();
        std::fs::write(&file, source.replace("lift = 2,", "lift = 2, speed = 1,")).unwrap();
        let moved = scripts
            .reload_file(&mut world, &dir, &folder, &file)
            .unwrap();
        assert_eq!((in_place.instances, moved.relaid.len()), (1000, 1));
        println!(
            "reload of a script on 1,000 entities: in place {:.2} ms, its props changed {:.2} ms",
            in_place.took.as_secs_f64() * 1e3,
            moved.took.as_secs_f64() * 1e3
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
