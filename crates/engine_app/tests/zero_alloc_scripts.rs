//! ADR 0114 §11 and §13: what the engine gives scripts allocates nothing in steady state.
//! The door example (§5) on 1,000 entities, and watchers using the rest of what scripts
//! have so far (`world.find`, entities as values and as table keys, an entity's name, an
//! entity prop, `self.entity`, `self.locals`, the time, a physics ray, contacts with a
//! trigger), through 600 ticks and frames after a warm-up: the heap allocations made in
//! the scripts' stages are counted (the physics steps between them are not). One test per
//! file, so nothing else allocates concurrently.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

mod common;
#[path = "common/counting.rs"]
mod counting;

#[global_allocator]
static GLOBAL: counting::CountingAllocator = counting::CountingAllocator;

use engine_app::prelude::*;
use engine_app::scripts::Scripts;
use engine_app::{ScenePhysics, Trigger};
use engine_physics::ecs::entity_user_data;
use engine_physics::{BodyDesc, PhysicsBackend, PhysicsSettings, ShapeDesc};

/// ADR 0114 §5's first example, as written there.
const DOOR: &str = r#"--!strict
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
"#;

/// The rest of the API scripts have so far.
const WATCHER: &str = r#"local Watcher = component("Watcher", {
    looks = 0,
    hits = 0,
    touches = 0,
    leaves = 0,
    last = prop.entity(),
})

function Watcher.on_tick(self, dt)
    local player = world.find("Player")
    if player == nil or not player.alive or player.name ~= "Player" then
        return
    end
    -- Entities as table keys, counted.
    local marks = self.locals
    marks[player] = (marks[player] or 0) + 1
    marks[self.entity] = (marks[self.entity] or 0) + 1
    self.looks += 1
    local below = self.Transform.translation - vector.create(0, 2, 0)
    local hit = physics.raycast(below, vector.create(0, -1, 0), 4)
    if hit == self.entity then
        self.hits += 1
    end
    local _ = time.now() + time.tick()
end

function Watcher.on_frame(self, dt, alpha)
    local t = self.Transform
    t.translation = t.translation
end

function Watcher.on_touch(self, other, point, normal)
    if other ~= nil and other.name == "Ball" then
        self.touches += 1
        self.last = other
    end
end

function Watcher.on_untouch(self, other)
    if other == self.last then
        self.leaves += 1
    end
end

return Watcher
"#;

/// Watchers, each with a trigger, a ball falling through it and a floor named after it.
const WATCHERS: usize = 10;

#[test]
fn the_engines_api_for_scripts_allocates_nothing_in_steady_state() {
    let dir =
        std::env::temp_dir().join(format!("ofelia-scripts-zero-alloc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("scripts")).unwrap();
    std::fs::write(dir.join("scripts/door.luau"), DOOR).unwrap();
    std::fs::write(dir.join("scripts/watcher.luau"), WATCHER).unwrap();
    let mut world = World::new();
    let mut scripts = Scripts::load(&mut world, &dir, &dir.join("scripts")).unwrap();
    let mut physics = ScenePhysics::new(&PhysicsSettings {
        threads: 0,
        ..PhysicsSettings::default()
    })
    .unwrap();

    // The player walks along 1,000 doors, 2 m apart: they open and close.
    let player = world.spawn((Name::new("Player"), Transform::from_xyz(0.0, 0.0, 0.0)));
    let doors: Vec<Entity> = (0..1000)
        .map(|i| {
            let e = world.spawn((Transform::from_xyz(i as f32 * 2.0, 0.0, 0.0),));
            assert!(scripts.add(&mut world, e, "Door"));
            e
        })
        .collect();
    // Watchers 10 m apart, off the doors' row.
    let round = physics
        .physics()
        .create_shape(&ShapeDesc::Sphere { radius: 0.25 })
        .unwrap();
    let floor = physics
        .physics()
        .create_shape(&ShapeDesc::Box {
            half_extents: Vec3::new(1.0, 0.25, 1.0),
        })
        .unwrap();
    let mut balls = Vec::new();
    let mut watchers = Vec::new();
    for i in 0..WATCHERS {
        let at = Vec3::new(i as f32 * 10.0, 0.0, 50.0);
        let watcher = world.spawn((
            Name::new("Watcher"),
            Transform::from_translation(at),
            GlobalTransform(Affine3A::from_translation(at)),
            Trigger::default().with_size(Vec3::splat(2.0)),
        ));
        assert!(scripts.add(&mut world, watcher, "Watcher"));
        watchers.push(watcher);
        let w = physics.physics();
        w.create_body(&BodyDesc {
            user_data: entity_user_data(watcher),
            ..BodyDesc::fixed(floor, at - Vec3::new(0.0, 3.0, 0.0))
        })
        .unwrap();
        let ball = world.spawn((Name::new("Ball"), Transform::from_translation(at)));
        let body = w
            .create_body(&BodyDesc {
                user_data: entity_user_data(ball),
                ..BodyDesc::dynamic(round, at + Vec3::new(0.0, 3.0, 0.0))
            })
            .unwrap();
        balls.push((body, at + Vec3::new(0.0, 3.0, 0.0)));
    }

    let dt = 1.0 / 64.0;
    let mut tick = |world: &mut World, scripts: &mut Scripts, n: usize, counted: &mut usize| {
        // The player paces along the row; every 80 ticks the balls are dropped again.
        let x = (n % 400) as f32 * 5.0;
        world.get_mut::<Transform>(player).unwrap().translation.x = x;
        if n.is_multiple_of(80) {
            for &(body, start) in &balls {
                let w = physics.physics();
                w.set_transform(body, start, Quat::IDENTITY);
                w.set_velocity(body, Vec3::ZERO, Vec3::ZERO);
            }
        }
        physics.sync_triggers(world).unwrap();
        let before = counting::allocations();
        scripts.tick_with(world, dt, Some(&physics));
        *counted += counting::allocations() - before;
        physics.step(world, dt).unwrap();
        let before = counting::allocations();
        scripts.frame_with(world, dt, 1.0, Some(&physics));
        *counted += counting::allocations() - before;
    };
    // Warm-up: the instances start, the name index and the views are made, mlua's and
    // Luau's own stacks and lists grow to what the stages need.
    let mut warm = 0;
    for n in 0..240 {
        tick(&mut world, &mut scripts, n, &mut warm);
    }
    let mut counted = 0;
    for n in 240..840 {
        tick(&mut world, &mut scripts, n, &mut counted);
    }
    println!("{warm} allocations in the warm-up's 240 ticks, {counted} in the next 600");

    // The scripts ran: doors moved, watchers looked, were hit and touched.
    let opened = doors
        .iter()
        .filter(|&&d| world.get::<Transform>(d).unwrap().translation.y > 0.01)
        .count();
    assert!(opened > 0, "no door opened");
    let id = scripts.component("Watcher").unwrap();
    let info = world.components().type_info(id).unwrap();
    let engine_reflect::TypeKind::Struct(fields) = &info.kind else {
        unreachable!()
    };
    let read = |e: Entity, name: &str| {
        let at = fields.iter().find(|f| f.name == name).unwrap().offset;
        let base = world.get_raw(e, id).unwrap().as_ptr();
        // SAFETY: a number prop (f32) at its offset in the watcher's component.
        unsafe { base.add(at).cast::<f32>().read_unaligned() }
    };
    for &w in &watchers {
        assert_eq!(read(w, "looks"), 840.0, "a watcher stopped");
        assert!(read(w, "hits") > 0.0, "no ray hit its floor");
        assert!(read(w, "touches") > 0.0, "no ball touched its trigger");
        assert!(read(w, "leaves") > 0.0, "no ball left its trigger");
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(counted, 0, "allocations in 600 ticks of scripts");
}
