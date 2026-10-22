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
//! - **A changed script reloads** ([`Scripts::reload_file`], E7-4b): its instances go on
//!   with their state. Props that kept their places take the new hooks in place; props that
//!   changed move each entity's values by name to a component of the new layout.

use crate::script_views::{self, Tables};
use engine_core::math::{Quat, Vec2, Vec3, Vec3A};
use engine_ecs::{ComponentId, Entity, Relayout, World};
use engine_reflect::{Attrs, Field, Primitive, Reflect, TypeInfo, TypeKind};
use engine_script::mlua::{
    self, AnyUserData, Function, Lua, MetaMethod, Table, UserData, UserDataMethods, Value, Vector,
};
use engine_script::{ScriptError, ScriptSettings, ScriptVm};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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
    clock: Clock,
    /// Errors already in the log.
    reported: HashSet<String>,
    /// Each module's text as it last ran, hashed: a reload of the same text does nothing.
    sources: HashMap<String, u64>,
    /// Each component's registry name's version: the next layout of its props is
    /// `script::Door#2`, and so on.
    versions: HashMap<String, u32>,
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
        engine_profiler::zone!("scripts::load");
        let vm = ScriptVm::new_with(ScriptSettings::default(), install)?;
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
        vm.lua().set_app_data(names);
        vm.lua().set_app_data(Clock::default());
        let mut scripts = Self {
            vm,
            tables,
            files: HashMap::new(),
            components: Vec::new(),
            by_name: HashMap::new(),
            instances: BTreeMap::new(),
            seen: HashSet::new(),
            clock: Clock::default(),
            reported: HashSet::new(),
            sources: HashMap::new(),
            versions: HashMap::new(),
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
        self.sources.insert(
            module.to_owned(),
            engine_core::hash::state_hash64(source.as_bytes()),
        );
        for (name, table, props) in declared {
            if let Err(why) = self.register(world, module, &name, &table, props) {
                let file = self.file(module);
                log::error!(target: "script", "{file}: component `{name}`: {why}");
            }
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
        let mut out = Reloaded {
            file: shown.clone(),
            ..Reloaded::default()
        };
        if self.sources.get(&module) == Some(&hash) {
            out.took = started.elapsed();
            return Ok(out);
        }
        self.vm.lua().set_app_data(Loading::default());
        let loaded = self.vm.load_module(&module, &source);
        let declared = self
            .vm
            .lua()
            .remove_app_data::<Loading>()
            .map(|l| l.declared)
            .unwrap_or_default();
        loaded.map_err(|e| self.place(&e))?;
        self.sources.insert(module.clone(), hash);
        let before = self.components.len();
        let mut again = Vec::new();
        for (name, table, props) in declared {
            let result = match self.by_name.get(&name).copied() {
                Some(k) if self.components[k].module == module => {
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
            if c.module == module && !again.contains(&k) {
                c.hooks = Hooks::default();
                out.gone.push(c.name.clone());
            }
        }
        // Its instances go on; the stopped ones start again.
        for (&(_, k), (_, inst)) in &mut self.instances {
            if self
                .components
                .get(k as usize)
                .is_some_and(|c| c.module == module && c.hooks.any())
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
        let (info, defaults, layout) = describe(&name, &mut props)?;
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
            source: source.into(),
        };
        let relayout = if in_place {
            world.set_type_info(old, info);
            None
        } else {
            let new = runtime
                .id(world)
                .ok_or_else(|| String::from("the world holds another component of its name"))?;
            let r = Relayout::between(old, old_info, new, info, runtime.defaults.clone());
            move_values(world, &r);
            Some(r)
        };
        let id = relayout.as_ref().map_or(old, |r| r.new);
        // SAFETY: the values are plain data of `layout`, as `register`'s.
        unsafe { engine_ecs::add_runtime_type(runtime) };
        if let Some(mut names) = self.vm.lua().app_data_mut::<Names>() {
            names.by_name.insert(name.clone(), (id, info));
        }
        self.tables.name(&name, id, info);
        let c = &mut self.components[k];
        c.hooks = hooks_of(table);
        c.info = info;
        c.id = id;
        c.defaults = defaults;
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
        let (info, defaults, layout) = describe(name, &mut props)?;
        // Beside the program's types, so scenes and the editor find it by name.
        let runtime = engine_ecs::RuntimeType {
            info,
            component: format!("script::{name}").into(),
            layout,
            defaults: defaults.clone().into_boxed_slice(),
            source: self.file(module).into(),
        };
        let id = runtime
            .id(world)
            .ok_or_else(|| String::from("the world holds another component of its name"))?;
        // SAFETY: the values are plain data of `layout` (numbers, booleans, vectors and
        // entities by value), with no drop glue, `Send` and `Sync`; the defaults are one.
        unsafe { engine_ecs::add_runtime_type(runtime) };
        let hooks = hooks_of(table);
        if let Some(mut names) = self.vm.lua().app_data_mut::<Names>() {
            names.by_name.insert(name.to_owned(), (id, info));
        }
        self.tables.name(name, id, info);
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
    fn open(&mut self, world: &mut World) {
        let lua = self.vm.lua();
        let _ = lua;
        OPEN_WORLD.set(world);
        script_views::open(&mut self.tables);
    }

    fn close(&self) {
        OPEN_WORLD.set(core::ptr::null_mut());
        script_views::close();
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
            let Some((e, inst)) = self.instances.remove(&key) else {
                continue;
            };
            if world.contains(e)
                && !inst.stopped
                && let Some(stop) = self.components[key.1 as usize].hooks.stop.clone()
            {
                let module = self.components[key.1 as usize].module.clone();
                if let Err(err) = self
                    .vm
                    .call_function::<_, ()>(&module, &stop, inst.this.clone())
                {
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
/// no order): its type, its default value and its layout.
fn describe(
    name: &str,
    props: &mut [Prop],
) -> Result<(&'static TypeInfo, Vec<u8>, std::alloc::Layout), String> {
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
    let layout = std::alloc::Layout::from_size_align(size, align).map_err(|e| e.to_string())?;
    Ok((info, defaults, layout))
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
    Hooks {
        start: hook("on_start"),
        tick: hook("on_tick"),
        frame: hook("on_frame"),
        stop: hook("on_stop"),
    }
}

/// Every entity's value of `r.old` moved into `r.new` ([`Relayout::apply`]).
fn move_values(world: &mut World, r: &Relayout) {
    let entities: Vec<Entity> = world
        .archetypes()
        .iter()
        .filter(|a| a.components().contains(&r.old))
        .flat_map(|a| (0..a.chunk_count()).flat_map(move |c| a.chunk_entities(c).iter().copied()))
        .collect();
    let size = world.components().layout(r.old).map_or(0, |l| l.size());
    for e in entities {
        let Some(ptr) = world.get_raw(e, r.old) else {
            continue;
        };
        // SAFETY: the entity's live value of the old layout: `size` bytes of plain data,
        // copied out before the world changes.
        let value = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), size) }.to_vec();
        let moved = r.apply(&value);
        // SAFETY: a value of the new layout, plain data, which `insert_raw` copies.
        unsafe { world.insert_raw(e, r.new, moved.as_ptr()) };
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
    }
}

/// Function `f` in the registry, by a reference the fast path pushes it by.
fn registry_ref(lua: &Lua, f: Function) -> mlua::Result<std::ffi::c_int> {
    let mut made = 0;
    // SAFETY: the function is the one argument pushed; the reference keeps it.
    unsafe {
        lua.exec_raw::<()>(f, |state| {
            made = mlua::ffi::lua_ref(state, -1);
        })?;
    }
    Ok(made)
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
    };
    lua.set_app_data(slow);
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
    // SAFETY: a C function of the fast path's, which checks its argument.
    world.set("find", unsafe {
        lua.create_c_function(script_views::world_find)?
    })?;
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

/// The world the host opened for this stage (null outside), for the fast path.
pub(crate) fn open_world() -> *mut World {
    OPEN_WORLD.get()
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

/// `self.key` where the fast path hands over (ADR 0114 E7-1c): `entity`, `locals`, props
/// of the kinds it does not read, and what is missing.
fn self_index_slow(lua: &Lua, (ud, key): (AnyUserData, mlua::LuaString)) -> mlua::Result<Value> {
    // SAFETY: the fast path hands over `self`s of its tag only.
    let this = unsafe { *script_views::this_at(ud.to_pointer()) };
    let key = key.to_str()?;
    // `self.entity` and `self.locals` are the fast path's own.
    if key.starts_with(|c: char| c.is_ascii_uppercase()) {
        return component_view(lua, this.entity, &key);
    }
    // SAFETY: the host's world for this stage (`world_ptr`), used within this callback only.
    let w = unsafe { &mut *world_ptr(lua)? };
    field(lua, w, this.entity, this.id, this.info, 0, &key)
}

/// `self.key = value` where the fast path hands over: props of the kinds it does not write,
/// and what cannot be written.
fn self_newindex_slow(
    lua: &Lua,
    (ud, key, value): (AnyUserData, mlua::LuaString, Value),
) -> mlua::Result<()> {
    // SAFETY: as `self_index_slow`.
    let this = unsafe { *script_views::this_at(ud.to_pointer()) };
    let key = key.to_str()?;
    if key.starts_with(|c: char| c.is_ascii_uppercase()) {
        return Err(mlua::Error::runtime(format!(
            "`self.{key}` is a component: write its fields (`self.{key}.field = …`)"
        )));
    }
    // SAFETY: the host's world for this stage (`world_ptr`), used within this callback only.
    let w = unsafe { &mut *world_ptr(lua)? };
    set_field(lua, w, this.entity, this.id, this.info, 0, &key, value)
}

/// `view.key` where the fast path hands over.
fn view_index_slow(lua: &Lua, (ud, key): (AnyUserData, mlua::LuaString)) -> mlua::Result<Value> {
    // SAFETY: the fast path hands over views of their tag only.
    let view = unsafe { *script_views::view_at(ud.to_pointer()) };
    let key = key.to_str()?;
    // SAFETY: the host's world for this stage (`world_ptr`), used within this callback only.
    let w = unsafe { &mut *world_ptr(lua)? };
    field(lua, w, view.entity, view.id, view.info, view.offset, &key)
}

/// `view.key = value` where the fast path hands over.
fn view_newindex_slow(
    lua: &Lua,
    (ud, key, value): (AnyUserData, mlua::LuaString, Value),
) -> mlua::Result<()> {
    // SAFETY: as `view_index_slow`.
    let view = unsafe { *script_views::view_at(ud.to_pointer()) };
    let key = key.to_str()?;
    // SAFETY: the host's world for this stage (`world_ptr`), used within this callback only.
    let w = unsafe { &mut *world_ptr(lua)? };
    set_field(
        lua,
        w,
        view.entity,
        view.id,
        view.info,
        view.offset,
        &key,
        value,
    )
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

/// The entity `value` is, if one.
fn entity_of(lua: &Lua, value: &Value) -> mlua::Result<Option<Entity>> {
    if !matches!(value, Value::UserData(_)) {
        return Ok(None);
    }
    let mut found = None;
    // SAFETY: the value is the one argument pushed, read where it lies.
    unsafe {
        lua.exec_raw::<()>(value.clone(), |state| {
            found = script_views::entity_at(state, -1);
        })?;
    }
    Ok(found)
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
            // SAFETY: room for the one value pushed.
            return lua.exec_raw::<Value>((), |state| script_views::push_entity(state, e));
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

/// Writes `value` into field `key` of `info` at `offset` in `entity`'s component `id`,
/// clamped to the field's range as the inspector does.
#[allow(clippy::too_many_arguments)]
fn set_field(
    lua: &Lua,
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
                Value::UserData(_) => entity_of(lua, &value)?.ok_or_else(|| wrong("an entity"))?,
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
