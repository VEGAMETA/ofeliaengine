//! Systems and their parallel, deterministic schedule (ADR 0026).
//!
//! A [`System`] declares what it touches once, in [`System::init`]: queries are created
//! through [`Access::query`] and resources through [`Access::read`] / [`Access::write`].
//! The [`Schedule`] keeps systems in the order they were added. Two systems get an
//! ordering edge only when their accesses conflict (one writes what the other reads or
//! writes); everything else may run at the same time. Systems are grouped into levels
//! (a system's level is one more than the highest level of the earlier systems it
//! conflicts with) and each level runs in parallel on the job system.
//!
//! Structural changes go through each system's [`Commands`]; they are applied after the
//! whole schedule, in the order the systems were added. So the result is the same as
//! running the systems one after another in that order, on any number of threads.

use crate::commands::Commands;
use crate::component::ComponentId;
use crate::query::{Fetch, Filter, QueryState};
use crate::world::World;
use core::marker::PhantomData;
use engine_core::jobs::ThreadPool;

/// What a system reads and writes, filled in by [`System::init`].
#[derive(Debug, Default, Clone)]
pub struct Access {
    system: u32,
    components: Vec<(ComponentId, bool)>,
    resources: Vec<(usize, bool)>,
    exclusive: bool,
}

impl Access {
    fn new(system: u32) -> Self {
        Self {
            system,
            ..Self::default()
        }
    }

    /// Declares a query and returns it, tagged for this system.
    pub fn query<Q: Fetch + 'static, F: Filter + 'static>(
        &mut self,
        world: &mut World,
    ) -> SystemQuery<Q, F> {
        let mut components = Vec::new();
        Q::register(world, &mut components);
        self.components.extend(components);
        SystemQuery {
            state: QueryState::new(world),
            system: self.system,
        }
    }

    /// Declares shared access to the resource `T`.
    pub fn read<T: Send + Sync + 'static>(&mut self, world: &mut World) -> Res<T> {
        let id = world.resources.register::<T>();
        self.resources.push((id, false));
        Res {
            id,
            system: self.system,
            _marker: PhantomData,
        }
    }

    /// Declares exclusive access to the resource `T`.
    pub fn write<T: Send + Sync + 'static>(&mut self, world: &mut World) -> ResMut<T> {
        let id = world.resources.register::<T>();
        self.resources.push((id, true));
        ResMut {
            id,
            system: self.system,
            _marker: PhantomData,
        }
    }

    fn conflicts(&self, other: &Self) -> bool {
        if self.exclusive || other.exclusive {
            return true;
        }
        let clash = |a: &[(ComponentId, bool)], b: &[(ComponentId, bool)]| {
            a.iter()
                .any(|&(x, wx)| b.iter().any(|&(y, wy)| x == y && (wx || wy)))
        };
        let clash_res = |a: &[(usize, bool)], b: &[(usize, bool)]| {
            a.iter()
                .any(|&(x, wx)| b.iter().any(|&(y, wy)| x == y && (wx || wy)))
        };
        clash(&self.components, &other.components) || clash_res(&self.resources, &other.resources)
    }
}

/// A query declared by a system; iterate it through [`SystemContext`].
pub struct SystemQuery<Q: Fetch, F: Filter = ()> {
    state: QueryState<Q, F>,
    system: u32,
}

/// Shared access to a resource, declared by a system.
pub struct Res<T> {
    id: usize,
    system: u32,
    _marker: PhantomData<fn() -> T>,
}

/// Exclusive access to a resource, declared by a system.
pub struct ResMut<T> {
    id: usize,
    system: u32,
    _marker: PhantomData<fn() -> T>,
}

/// What a running system sees: its declared queries and resources, and its commands.
pub struct SystemContext<'a> {
    world: &'a World,
    tick: u64,
    system: u32,
    commands: &'a mut Commands,
}

impl<'a> SystemContext<'a> {
    fn check(&self, system: u32) {
        assert_eq!(
            system, self.system,
            "a system used a query or resource declared by another system"
        );
    }

    /// Calls `f` for every entity matching `query`.
    pub fn for_each<Q: Fetch, F: Filter>(
        &mut self,
        query: &mut SystemQuery<Q, F>,
        f: impl FnMut(Q::Item<'_>),
    ) {
        self.check(query.system);
        // SAFETY: the query was declared by this system, and the schedule never runs two
        // systems with conflicting accesses at the same time, nor changes the world
        // structurally while systems run.
        unsafe { query.state.for_each_shared(self.world, self.tick, f) }
    }

    /// Calls `f` with each matching chunk's component arrays.
    pub fn for_each_chunk<Q: Fetch, F: Filter>(
        &mut self,
        query: &mut SystemQuery<Q, F>,
        f: impl FnMut(Q::Slice<'_>),
    ) {
        self.check(query.system);
        // SAFETY: as in `for_each`.
        unsafe { query.state.for_each_chunk_shared(self.world, self.tick, f) }
    }

    /// The resource, or `None` if it was never inserted.
    pub fn res<T: 'static>(&self, res: &Res<T>) -> Option<&T> {
        self.check(res.system);
        // SAFETY: declared as a shared access by this system; no system writing it runs
        // concurrently.
        unsafe { self.world.resources.get(res.id) }
    }

    /// The resource, mutably, or `None` if it was never inserted.
    pub fn res_mut<T: 'static>(&mut self, res: &ResMut<T>) -> Option<&mut T> {
        self.check(res.system);
        // SAFETY: declared as an exclusive access by this system; no other system
        // touching it runs concurrently, and `&mut self` prevents a second borrow here.
        unsafe { self.world.resources.get_mut(res.id) }
    }

    /// This system's commands, applied after the schedule in system order.
    pub fn commands(&mut self) -> &mut Commands {
        self.commands
    }

    /// The change tick of this run.
    pub fn tick(&self) -> u64 {
        self.tick
    }
}

/// A unit of game logic.
pub trait System: Send {
    /// Name for profiling and errors.
    fn name(&self) -> &str;
    /// Declares queries and resources.
    fn init(&mut self, world: &mut World, access: &mut Access);
    /// Runs once per [`Schedule::run`].
    fn run(&mut self, ctx: &mut SystemContext<'_>);
}

/// A system made of a state built in `init` and a function run with it.
pub struct FnSystem<S, I, R> {
    name: &'static str,
    state: Option<S>,
    init: Option<I>,
    run: R,
}

/// Builds a system from a name, an `init` that declares accesses and returns the state
/// (typically the queries and resource handles), and a `run` function.
///
/// ```
/// use engine_ecs::{Schedule, World, pod_component, bytemuck, system};
///
/// #[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
/// #[repr(C)]
/// struct Pos(f32);
/// #[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
/// #[repr(C)]
/// struct Vel(f32);
/// pod_component!(Pos, Vel);
///
/// let mut world = World::new();
/// world.spawn((Pos(0.0), Vel(2.0)));
/// world.insert_resource(0.5f32); // time step
/// let mut schedule = Schedule::new();
/// schedule.add(system(
///     "integrate",
///     |world, access| (access.query::<(&mut Pos, &Vel), ()>(world), access.read::<f32>(world)),
///     |(q, dt), ctx| {
///         let dt = *ctx.res(dt).unwrap();
///         ctx.for_each(q, |(p, v)| p.0 += v.0 * dt);
///     },
/// ));
/// schedule.run(&mut world, None);
/// ```
pub fn system<S, I, R>(name: &'static str, init: I, run: R) -> FnSystem<S, I, R>
where
    S: Send,
    I: FnOnce(&mut World, &mut Access) -> S + Send,
    R: FnMut(&mut S, &mut SystemContext<'_>) + Send,
{
    FnSystem {
        name,
        state: None,
        init: Some(init),
        run,
    }
}

impl<S, I, R> System for FnSystem<S, I, R>
where
    S: Send,
    I: FnOnce(&mut World, &mut Access) -> S + Send,
    R: FnMut(&mut S, &mut SystemContext<'_>) + Send,
{
    fn name(&self) -> &str {
        self.name
    }

    fn init(&mut self, world: &mut World, access: &mut Access) {
        if let Some(init) = self.init.take() {
            self.state = Some(init(world, access));
        }
    }

    fn run(&mut self, ctx: &mut SystemContext<'_>) {
        if let Some(state) = &mut self.state {
            (self.run)(state, ctx);
        }
    }
}

enum Kind {
    System(Box<dyn System>),
    Exclusive(&'static str, Box<dyn FnMut(&mut World) + Send>),
}

struct Entry {
    kind: Kind,
    access: Access,
    commands: Commands,
    level: u32,
    /// Position in the order systems were added.
    order: usize,
    initialized: bool,
}

/// Systems in a fixed order, run level by level; see the module documentation.
#[derive(Default)]
pub struct Schedule {
    entries: Vec<Entry>,
    /// Ranges of `entries` (sorted by level) that run together.
    levels: Vec<core::ops::Range<usize>>,
    /// Entry index by the order systems were added (for applying commands).
    by_order: Vec<usize>,
    initialized: bool,
}

impl Schedule {
    /// An empty schedule.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a system after the ones already added.
    pub fn add(&mut self, system: impl System + 'static) -> &mut Self {
        self.push(Kind::System(Box::new(system)));
        self
    }

    /// Adds a function with exclusive access to the world: it runs alone, after
    /// everything added before it and before everything added after it.
    pub fn add_exclusive(
        &mut self,
        name: &'static str,
        f: impl FnMut(&mut World) + Send + 'static,
    ) -> &mut Self {
        self.push(Kind::Exclusive(name, Box::new(f)));
        self
    }

    fn push(&mut self, kind: Kind) {
        let order = self.entries.len();
        self.entries.push(Entry {
            kind,
            access: Access::new(order as u32),
            commands: Commands::new(),
            level: 0,
            order,
            initialized: false,
        });
        self.initialized = false;
    }

    /// Number of systems.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether there are no systems.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// System names grouped by level (the systems in one group may run in parallel).
    /// Available after the first [`Schedule::run`] or [`Schedule::initialize`].
    pub fn levels(&self) -> Vec<Vec<&str>> {
        self.levels
            .iter()
            .map(|r| {
                self.entries[r.clone()]
                    .iter()
                    .map(|e| match &e.kind {
                        Kind::System(s) => s.name(),
                        Kind::Exclusive(n, _) => n,
                    })
                    .collect()
            })
            .collect()
    }

    /// Declares every system's accesses and computes the levels. Called by the first
    /// [`Schedule::run`] after systems were added.
    pub fn initialize(&mut self, world: &mut World) {
        if self.initialized {
            return;
        }
        // Back to the order systems were added, then declare new ones.
        self.entries.sort_by_key(|e| e.order);
        for e in &mut self.entries {
            if core::mem::replace(&mut e.initialized, true) {
                continue;
            }
            match &mut e.kind {
                Kind::System(s) => s.init(world, &mut e.access),
                Kind::Exclusive(..) => e.access.exclusive = true,
            }
        }
        for j in 0..self.entries.len() {
            let mut level = 0;
            for i in 0..j {
                if self.entries[i].access.conflicts(&self.entries[j].access) {
                    level = level.max(self.entries[i].level + 1);
                }
            }
            self.entries[j].level = level;
        }
        self.entries.sort_by_key(|e| (e.level, e.order));
        self.levels.clear();
        let mut start = 0;
        while start < self.entries.len() {
            let level = self.entries[start].level;
            let end = start
                + self.entries[start..]
                    .iter()
                    .take_while(|e| e.level == level)
                    .count();
            self.levels.push(start..end);
            start = end;
        }
        self.by_order = vec![0; self.entries.len()];
        for (i, e) in self.entries.iter().enumerate() {
            self.by_order[e.order] = i;
        }
        self.initialized = true;
    }

    /// Runs every system once: level by level, the systems of a level in parallel on
    /// `pool` (or one after another without it), then applies all commands in the order
    /// the systems were added.
    pub fn run(&mut self, world: &mut World, pool: Option<&ThreadPool>) {
        engine_profiler::zone!("ecs::Schedule::run");
        self.initialize(world);
        let tick = world.bump_change_tick();
        for li in 0..self.levels.len() {
            let range = self.levels[li].clone();
            let exclusive = self.entries[range.start].access.exclusive;
            if exclusive {
                // Exclusive entries conflict with everything, so they are alone.
                for e in &mut self.entries[range] {
                    if let Kind::Exclusive(_, f) = &mut e.kind {
                        f(world);
                    }
                }
                continue;
            }
            let shared: &World = world;
            let entries = &mut self.entries[range];
            match pool {
                Some(pool) if entries.len() > 1 => pool.scope(|s| {
                    for e in entries.iter_mut() {
                        s.spawn(move || run_entry(e, shared, tick));
                    }
                }),
                _ => {
                    for e in entries.iter_mut() {
                        run_entry(e, shared, tick);
                    }
                }
            }
        }
        for &i in &self.by_order {
            let commands = &mut self.entries[i].commands;
            if !commands.is_empty() {
                world.apply(commands);
            }
        }
    }
}

fn run_entry(e: &mut Entry, world: &World, tick: u64) {
    if let Kind::System(system) = &mut e.kind {
        let mut ctx = SystemContext {
            world,
            tick,
            system: e.access.system,
            commands: &mut e.commands,
        };
        system.run(&mut ctx);
    }
}
