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
//!   - `on_stop` once the component went from a living entity: its props are gone with
//!     it, `self.entity` and `self.locals` stay.
//!
//!   Instances run in their entities' order (spawn order, the same on every machine).
//! - **`self`** holds the instance's props (`self.speed`), its entity's components, live
//!   (`self.Transform.translation`), `self.entity` and `self.locals`, a table kept between
//!   hooks and never saved.
//! - **The libraries of this step:**
//!   - `world.find(name)`;
//!   - `time.now()`, `time.tick()`, `time.dt()`;
//!   - `quat.identity()`, `quat.axis_angle(axis, angle)`, `quat.euler(x, y, z)`;
//!   - `prop.number(default, { min, max, step, tooltip })`, `prop.integer`, `prop.entity`;
//!   - `print` and `warn`, to the log with the script's file and line.
//! - **A runtime error stops that instance:** its hooks are called no more. The error goes
//!   to the log once with `scripts/door.luau:14` (a click in the editor's Output opens it).
//!   A script that does not load is reported, and the others load.

use engine_core::math::{Quat, Vec2, Vec3, Vec3A};
use engine_ecs::{ComponentId, Entity, World};
use engine_reflect::{Attrs, Field, Primitive, Reflect, TypeInfo, TypeKind};
use engine_scene::Name;
use engine_script::mlua::{
    self, AnyUserData, Function, Lua, MetaMethod, Table, UserData, UserDataMethods, Value, Vector,
};
use engine_script::{ScriptError, ScriptSettings, ScriptVm};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

/// What a prop holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Number,
    Integer,
    Boolean,
    Vector,
    Entity,
}

impl Kind {
    fn info(self) -> &'static TypeInfo {
        match self {
            Self::Number => <f32 as Reflect>::TYPE_INFO,
            Self::Integer => <i32 as Reflect>::TYPE_INFO,
            Self::Boolean => <bool as Reflect>::TYPE_INFO,
            Self::Vector => <Vec3 as Reflect>::TYPE_INFO,
            Self::Entity => <Entity as Reflect>::TYPE_INFO,
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
    /// The module that declared it, and its file (errors name it).
    module: String,
    /// Its hooks, found on its table once its module ran.
    hooks: Hooks,
    info: &'static TypeInfo,
    id: ComponentId,
    /// Its props' defaults, laid out.
    defaults: Vec<u8>,
}

#[derive(Default)]
struct Hooks {
    start: Option<Function>,
    tick: Option<Function>,
    frame: Option<Function>,
    stop: Option<Function>,
}

impl Hooks {
    fn any(&self) -> bool {
        self.start.is_some() || self.tick.is_some() || self.frame.is_some() || self.stop.is_some()
    }
}

/// An instance: a component on an entity, its `self` made once.
struct Instance {
    this: AnyUserData,
    started: bool,
    /// An error stopped it: its hooks are called no more.
    stopped: bool,
}

/// What a module's run declared, gathered while it loads.
#[derive(Default)]
struct Loading {
    declared: Vec<(String, Table, Vec<Prop>)>,
}

thread_local! {
    /// The world, open to the scripts for the length of a stage (null outside): a
    /// thread-local, not the VM's app data, as every field's access reads it.
    static OPEN_WORLD: core::cell::Cell<*mut World> = const { core::cell::Cell::new(core::ptr::null_mut()) };
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

/// The entities by name, made at a stage's first `world.find` (ADR 0114 §3: cheap every
/// tick): of several of one name, the first in the entities' order.
#[derive(Default)]
struct NameIndex {
    /// The stage it was made in.
    stage: u64,
    by_name: HashMap<String, Entity>,
}

/// The stage the world is open for: a name index made in another is stale.
#[derive(Default, Clone, Copy)]
struct StageCount(u64);

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
    /// Each module's file, relative to the project.
    files: HashMap<String, PathBuf>,
    components: Vec<Declared>,
    by_name: HashMap<String, usize>,
    /// The instances in their entities' order: (entity index, component), the entity.
    instances: BTreeMap<(u32, u32), (Entity, Instance)>,
    /// The entities seen in this stage, for the instances gone (scratch).
    seen: HashSet<(u32, u32)>,
    clock: Clock,
    /// Errors already in the log.
    reported: HashSet<String>,
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
        engine_profiler::zone!("scripts::load");
        let vm = ScriptVm::new_with(ScriptSettings::default(), install)?;
        let mut names = Names::default();
        for t in engine_ecs::reflected_types() {
            if let engine_ecs::ReflectedKind::Component { id, .. } = t.kind {
                names
                    .by_name
                    .insert(t.info.name.to_owned(), (id(world), t.info));
            }
        }
        vm.lua().set_app_data(names);
        vm.lua().set_app_data(Clock::default());
        let mut scripts = Self {
            vm,
            files: HashMap::new(),
            components: Vec::new(),
            by_name: HashMap::new(),
            instances: BTreeMap::new(),
            seen: HashSet::new(),
            clock: Clock::default(),
            reported: HashSet::new(),
        };
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
            let source = match std::fs::read_to_string(&file) {
                Ok(s) => s,
                Err(e) => {
                    log::error!(target: "script", "{}: {e}", shown.display());
                    continue;
                }
            };
            scripts.load_module(world, &module, &source);
        }
        Ok(scripts)
    }

    /// Runs module `module`'s `source` and registers what it declares.
    fn load_module(&mut self, world: &mut World, module: &str, source: &str) {
        let lua = self.vm.lua();
        lua.set_app_data(Loading::default());
        let loaded = self.vm.load_module(module, source);
        let declared = self
            .vm
            .lua()
            .remove_app_data::<Loading>()
            .map(|l| l.declared)
            .unwrap_or_default();
        if let Err(e) = loaded {
            log::error!(target: "script", "{}", self.place(&e));
            return;
        }
        for (name, table, props) in declared {
            if let Err(why) = self.register(world, module, &name, &table, props) {
                let file = self.file(module);
                log::error!(target: "script", "{file}: component `{name}`: {why}");
            }
        }
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
        // Laid out by name: a table's keys have no order.
        props.sort_by(|a, b| a.name.cmp(&b.name));
        let mut offset = 0usize;
        let mut align = 1usize;
        for p in &mut props {
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
        for p in &props {
            write_default(&mut defaults[p.offset..], p.kind, &p.default);
        }
        let layout = std::alloc::Layout::from_size_align(size, align).map_err(|e| e.to_string())?;
        // Beside the program's types, so scenes and the editor find it by name.
        let runtime = engine_ecs::RuntimeType {
            info,
            component: format!("script::{name}").into(),
            layout,
            defaults: defaults.clone().into_boxed_slice(),
        };
        let id = runtime
            .id(world)
            .ok_or_else(|| String::from("the world holds another component of its name"))?;
        // SAFETY: the values are plain data of `layout` (numbers, booleans, vectors and
        // entities by value), with no drop glue, `Send` and `Sync`; the defaults are one.
        unsafe { engine_ecs::add_runtime_type(runtime) };
        let hook = |key: &str| table.get::<Option<Function>>(key).ok().flatten();
        let hooks = Hooks {
            start: hook("on_start"),
            tick: hook("on_tick"),
            frame: hook("on_frame"),
            stop: hook("on_stop"),
        };
        if let Some(mut names) = self.vm.lua().app_data_mut::<Names>() {
            names.by_name.insert(name.to_owned(), (id, info));
        }
        self.by_name.insert(name.to_owned(), self.components.len());
        self.components.push(Declared {
            name: name.to_owned(),
            module: module.to_owned(),
            hooks,
            info,
            id,
            defaults,
        });
        Ok(())
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
            .get(&self.components[k].module)
            .map(PathBuf::as_path)
    }

    /// Gives `entity` script component `name`, its props at their defaults; false for an
    /// unknown component or a dead entity.
    pub fn add(&self, world: &mut World, entity: Entity, name: &str) -> bool {
        let Some(c) = self.by_name.get(name).map(|&k| &self.components[k]) else {
            return false;
        };
        // SAFETY: the defaults are a value of the component's layout, plain data, copied.
        unsafe { world.insert_raw(entity, c.id, c.defaults.as_ptr()) }
    }

    /// A fixed tick's stage (before the game's own): instances that came are started, then
    /// every instance's `on_tick`, in their entities' order.
    pub fn tick(&mut self, world: &mut World, dt: f32) {
        engine_profiler::zone!("scripts::tick");
        self.clock.ticks += 1;
        self.clock.dt = dt;
        self.vm.lua().set_app_data(self.clock);
        self.open(world);
        self.follow(world);
        let ids: Vec<(u32, u32)> = self.instances.keys().copied().collect();
        for key in ids {
            self.call(key, Stage::Tick, dt, 0.0);
        }
        self.close();
    }

    /// A frame's stage, before its presentation: every instance's `on_frame`.
    pub fn frame(&mut self, world: &mut World, dt: f32, alpha: f32) {
        engine_profiler::zone!("scripts::frame");
        if !self.components.iter().any(|c| c.hooks.frame.is_some()) {
            return;
        }
        self.open(world);
        self.follow(world);
        let ids: Vec<(u32, u32)> = self.instances.keys().copied().collect();
        for key in ids {
            self.call(key, Stage::Frame, dt, alpha);
        }
        self.close();
    }

    /// The world open to the scripts until [`Self::close`].
    fn open(&self, world: &mut World) {
        let lua = self.vm.lua();
        OPEN_WORLD.set(world);
        let stage = lua.app_data_ref::<StageCount>().map_or(0, |s| s.0) + 1;
        lua.set_app_data(StageCount(stage));
    }

    fn close(&self) {
        OPEN_WORLD.set(core::ptr::null_mut());
    }

    /// The instances made to match the world: those that came get a `self` and are started
    /// (in their entities' order), those whose component went are stopped.
    fn follow(&mut self, world: &mut World) {
        self.seen.clear();
        let mut came: Vec<((u32, u32), Entity)> = Vec::new();
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
            if let Some((e, inst)) = self.instances.remove(&key)
                && world.contains(e)
                && !inst.stopped
                && let Some(stop) = self.components[key.1 as usize].hooks.stop.clone()
            {
                let module = self.components[key.1 as usize].module.clone();
                if let Err(err) = self.vm.call_function::<_, ()>(&module, &stop, inst.this) {
                    self.report(key.1 as usize, "on_stop", e, &err);
                }
            }
        }
        came.sort_unstable_by_key(|(k, _)| *k);
        for (key, e) in came {
            let c = &self.components[key.1 as usize];
            let this = This {
                entity: e,
                id: c.id,
                info: c.info,
            };
            let Ok(this) = self.vm.lua().create_userdata(this) else {
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
        let module = c.module.clone();
        if !inst.started && stage == Stage::Tick {
            inst.started = true;
            if let Some(start) = c.hooks.start.clone()
                && let Err(err) = self
                    .vm
                    .call_function::<_, ()>(&module, &start, this.clone())
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
            Stage::Tick => self.vm.call_function::<_, ()>(&module, &hook, (this, dt)),
            Stage::Frame => self
                .vm
                .call_function::<_, ()>(&module, &hook, (this, dt, alpha)),
        };
        if let Err(err) = result {
            self.stop(key, name, e, &err);
        }
    }

    /// Instance `key` stopped by `err` in hook `hook`.
    fn stop(&mut self, key: (u32, u32), hook: &str, e: Entity, err: &ScriptError) {
        if let Some((_, inst)) = self.instances.get_mut(&key) {
            inst.stopped = true;
        }
        self.report(key.1 as usize, hook, e, err);
    }

    /// `err` in component `k`'s `hook` on `e`, in the log once.
    fn report(&mut self, k: usize, hook: &str, e: Entity, err: &ScriptError) {
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

/// Which hooks a stage calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Tick,
    Frame,
}

/// The `.luau` files under `dir`, hidden ones left out.
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
        } else if path.extension().is_some_and(|x| x == "luau") {
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
    }
}

/// The host's libraries, before the sandbox seals the globals.
fn install(lua: &Lua) -> mlua::Result<()> {
    let globals = lua.globals();
    globals.set("component", lua.create_function(declare_component)?)?;
    let prop = lua.create_table()?;
    for (name, kind) in [
        ("number", "number"),
        ("integer", "integer"),
        ("boolean", "boolean"),
        ("vector", "vector"),
        ("entity", "entity"),
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
    world.set(
        "find",
        lua.create_function(|lua, name: mlua::LuaString| {
            let stage = lua.app_data_ref::<StageCount>().map_or(0, |s| s.0);
            let stale = lua
                .app_data_ref::<NameIndex>()
                .is_none_or(|index| index.stage != stage);
            if stale {
                // SAFETY: the host's world for this stage (`world_ptr`), used within this callback only.
                let w = unsafe { &mut *world_ptr(lua)? };
                let mut by_name: HashMap<String, Entity> = HashMap::new();
                let mut q = engine_ecs::QueryState::<(Entity, &Name)>::new(w);
                for (e, n) in q.iter_readonly(w) {
                    by_name
                        .entry(n.as_str().to_owned())
                        .and_modify(|f| {
                            if e.index() < f.index() {
                                *f = e;
                            }
                        })
                        .or_insert(e);
                }
                lua.set_app_data(NameIndex { stage, by_name });
            }
            let name = name.to_str()?;
            let found = lua
                .app_data_ref::<NameIndex>()
                .and_then(|index| index.by_name.get(&*name).copied());
            found
                .map(|e| lua.create_userdata(ScriptEntity(e)))
                .transpose()
        })?,
    )?;
    globals.set("world", world)?;
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
        return Err(bad(match value {
            Value::String(_) => {
                "strings in props come in a later step; use a number, a boolean, a vector or prop.entity()"
            }
            _ => "a prop is a number, a boolean, a vector or a prop.* value",
        }));
    };
    let helper: Option<String> = t.get("__prop")?;
    let kind = match helper.as_deref() {
        Some("number") => Kind::Number,
        Some("integer") => Kind::Integer,
        Some("boolean") => Kind::Boolean,
        Some("vector") => Kind::Vector,
        Some("entity") => Kind::Entity,
        _ => {
            return Err(bad(
                "a table is not a prop; use prop.number(…) and the others",
            ));
        }
    };
    let default: Value = t.get("default")?;
    let mut attrs = Attrs::NONE;
    if let Some(opts) = t.get::<Option<Table>>("opts")? {
        attrs.min = opts.get("min")?;
        attrs.max = opts.get("max")?;
        attrs.step = opts.get("step")?;
        attrs.hidden = opts.get::<Option<bool>>("hidden")?.unwrap_or(false);
        attrs.skip = opts.get::<Option<bool>>("skip")?.unwrap_or(false);
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

/// `self` in a hook.
#[derive(Clone, Copy)]
struct This {
    entity: Entity,
    id: ComponentId,
    info: &'static TypeInfo,
}

impl UserData for This {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_function(
            MetaMethod::Index,
            |lua, (ud, key): (AnyUserData, mlua::LuaString)| {
                let this = *ud.borrow::<This>()?;
                let key = key.to_str()?;
                match &*key {
                    "entity" => {
                        return lua
                            .create_userdata(ScriptEntity(this.entity))
                            .map(Value::UserData);
                    }
                    "locals" => {
                        if let Ok(Value::Table(t)) = ud.named_user_value::<Value>("locals") {
                            return Ok(Value::Table(t));
                        }
                        let t = lua.create_table()?;
                        ud.set_named_user_value("locals", t.clone())?;
                        return Ok(Value::Table(t));
                    }
                    _ => {}
                }
                if key.starts_with(|c: char| c.is_ascii_uppercase()) {
                    return component_view(lua, this.entity, &key);
                }
                // SAFETY: the host's world for this stage (`world_ptr`), used within this callback only.
                let w = unsafe { &mut *world_ptr(lua)? };
                field(lua, w, this.entity, this.id, this.info, 0, &key)
            },
        );
        methods.add_meta_function(
            MetaMethod::NewIndex,
            |lua, (ud, key, value): (AnyUserData, mlua::LuaString, Value)| {
                let this = *ud.borrow::<This>()?;
                let key = key.to_str()?;
                if key.starts_with(|c: char| c.is_ascii_uppercase()) {
                    return Err(mlua::Error::runtime(format!(
                        "`self.{key}` is a component: write its fields (`self.{key}.field = …`)"
                    )));
                }
                // SAFETY: the host's world for this stage (`world_ptr`), used within this callback only.
                let w = unsafe { &mut *world_ptr(lua)? };
                set_field(w, this.entity, this.id, this.info, 0, &key, value)
            },
        );
    }
}

/// An entity in a script: its components by name, its `name`.
#[derive(Clone, Copy, PartialEq, Eq)]
struct ScriptEntity(Entity);

impl UserData for ScriptEntity {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::Index, |lua, this, key: mlua::LuaString| {
            let key = key.to_str()?;
            if key.starts_with(|c: char| c.is_ascii_uppercase()) {
                return component_view(lua, this.0, &key);
            }
            match &*key {
                "name" => {
                    // SAFETY: the host's world for this stage (`world_ptr`), used within this callback only.
                    let w = unsafe { &mut *world_ptr(lua)? };
                    let name = w.get::<Name>(this.0).map(|n| n.as_str().to_owned());
                    Ok(name.map_or(Value::Nil, |n| {
                        lua.create_string(&n).map_or(Value::Nil, Value::String)
                    }))
                }
                "alive" => {
                    // SAFETY: the host's world for this stage (`world_ptr`), used within this callback only.
                    let alive = world_ptr(lua).is_ok_and(|w| unsafe { &*w }.contains(this.0));
                    Ok(Value::Boolean(alive))
                }
                other => Err(mlua::Error::runtime(format!("an entity has no `{other}`"))),
            }
        });
        methods.add_meta_method(MetaMethod::Eq, |_, this, other: AnyUserData| {
            Ok(other.borrow::<ScriptEntity>().is_ok_and(|o| *o == *this))
        });
        methods.add_meta_method(MetaMethod::ToString, |_, this, ()| {
            Ok(format!("Entity({})", this.0.index()))
        });
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
    lua.create_userdata(View {
        entity,
        id,
        info,
        offset: 0,
    })
    .map(Value::UserData)
}

/// A component (or a struct inside one) of an entity, seen live: its fields read and write
/// the world.
#[derive(Clone, Copy)]
struct View {
    entity: Entity,
    id: ComponentId,
    info: &'static TypeInfo,
    /// Where it lies in the component.
    offset: usize,
}

impl UserData for View {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::Index, |lua, this, key: mlua::LuaString| {
            let key = key.to_str()?;
            // SAFETY: the host's world for this stage (`world_ptr`), used within this callback only.
            let w = unsafe { &mut *world_ptr(lua)? };
            field(lua, w, this.entity, this.id, this.info, this.offset, &key)
        });
        methods.add_meta_method(
            MetaMethod::NewIndex,
            |lua, this, (key, value): (mlua::LuaString, Value)| {
                let key = key.to_str()?;
                // SAFETY: the host's world for this stage (`world_ptr`), used within this callback only.
                let w = unsafe { &mut *world_ptr(lua)? };
                set_field(w, this.entity, this.id, this.info, this.offset, &key, value)
            },
        );
    }
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
    info: &'static TypeInfo,
    offset: usize,
    key: &str,
) -> mlua::Result<Value> {
    let f = struct_field(info, key)?;
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
            let e = at.cast::<Entity>().read_unaligned();
            if e == Entity::NULL {
                return Ok(Value::Nil);
            }
            return lua.create_userdata(ScriptEntity(e)).map(Value::UserData);
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
            TypeKind::Struct(_) => lua
                .create_userdata(View {
                    entity,
                    id,
                    info: ty,
                    offset,
                })
                .map(Value::UserData),
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

/// Writes `value` into field `key` of `info` at `offset` in `entity`'s component `id`,
/// clamped to the field's range as the inspector does.
fn set_field(
    w: &mut World,
    entity: Entity,
    id: ComponentId,
    info: &'static TypeInfo,
    offset: usize,
    key: &str,
    value: Value,
) -> mlua::Result<()> {
    let f = struct_field(info, key)?;
    let base = w
        .get_raw_mut(entity, id)
        .ok_or_else(|| mlua::Error::runtime("the entity is gone, or lost the component"))?;
    // SAFETY: the component's bytes, `f` inside them by its offset from reflection.
    let at = unsafe { base.as_ptr().add(offset + f.offset) };
    let given = value.type_name();
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
                Value::UserData(u) => {
                    u.borrow::<ScriptEntity>()
                        .map_err(|_| wrong("an entity"))?
                        .0
                }
                _ => return Err(wrong("an entity")),
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
            ],
        );
        let median = |mut v: Vec<f64>| {
            v.sort_by(f64::total_cmp);
            v[v.len() / 2]
        };
        let run = |name: &str, count: usize| {
            let mut world = World::new();
            let mut scripts = load(&dir, &mut world);
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
}
