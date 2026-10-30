//! ADR 0114 §11 and §13: what the engine gives scripts allocates nothing in steady state.
//! The door example (§5) on 1,000 entities, watchers using the rest of what scripts have
//! (`world.find`, entities as values and as table keys, an entity's name, an entity prop,
//! `self.entity`, `self.locals`, the time, a physics ray, contacts with a trigger), and
//! walkers using entities' and `Transform`'s methods (`rotate`, `forward`, `look_at`,
//! `find_child`, `children`, `has`, `parent`, a function of their own through `self:`),
//! the spinning example (§5) as a system over a query of 1,000 entities, relays sending
//! each other messages and setting timers, dice drawing the world's random numbers, and
//! chimes reading their text props and writing one, steppers hearing their clips' events
//! (ADR 0124's E12-3b), and riders setting and firing their animators' graphs' parameters
//! by name and reading their states (E12-5c), every tick, through 600 ticks and frames
//! after a warm-up: the heap allocations made in the scripts' stages are counted (the
//! physics steps between them are not). One test per file, so nothing else allocates
//! concurrently.

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

/// Methods: entities' and `Transform`'s (ADR 0114 §3, E7-5a).
const STROLLER: &str = r#"local UP = vector.create(0, 1, 0)
local Stroller = component("Stroller", { speed = 1.5, steps = 0 })

function Stroller.step(self, by)
    self.steps += by
end

function Stroller.on_tick(self, dt)
    local t = self.Transform
    t:rotate(UP, self.speed * dt)
    local ahead = t:forward() + t:right() * 0.5 + t:up() * 0.1
    t.translation += ahead * dt
    local camera = self:find_child("Camera")
    if camera and camera:has(Transform) and camera.parent == self.entity then
        camera.Transform:look_at(vector.create(0, 0, -10))
    end
    for child in self.entity:children() do
        if child:has(Transform) then
            self:step(1)
        end
    end
end

return Stroller
"#;

/// ADR 0114 §5's fourth example, as written there: the game's input.
const WALKER: &str = r#"--!strict
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
"#;

/// ADR 0114 §5's third example, as written there: a system over a query.
const SPIN: &str = r#"--!strict
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
"#;

/// Messages and timers (ADR 0114 §3, E7-5e): a relay sends its partner a message of three
/// values and sets itself a timer every tick.
const RELAY: &str = r#"local Relay = component("Relay", { got = 0, fired = 0, partner = prop.entity() })
function Relay.on_tick(self, dt)
    if self.partner ~= nil then
        self.partner:send("hello", "hi", self.entity, 1.5)
    end
    self:after(0.05, "tock", 2)
end
function Relay.hello(self, word, from, n)
    if word == "hi" and from ~= nil and n == 1.5 then
        self.got += 1
    end
end
function Relay.tock(self, n)
    self.fired += n
end
return Relay
"#;

/// The world's random numbers (E7-5e2): drawn every tick.
const DICE: &str = r#"local Dice = component("Dice", { sum = 0, inside = 0 })
function Dice.on_tick(self, dt)
    self.sum += random.integer(1, 6) + random.number()
    if vector.magnitude(random.vector_in_sphere()) <= 1 then
        self.inside += 1
    end
end
return Dice
"#;

/// Text props read and written every tick: the same text again makes nothing (the string
/// kept, the `String`'s room reused).
const CHIME: &str = r#"--!strict
local Chime = component("Chime", {
    sound = "ui/chime.ogg",
    said = prop.string(""),
    rings = 0,
})
function Chime.on_tick(self, dt)
    if self.sound ~= "" then
        self.rings += 1
    end
    self.said = self.sound
end
return Chime
"#;

/// Clips' events heard (ADR 0124's E12-3b): a name and its data, or none.
const STEPPER: &str = r#"local Stepper = component("Stepper", { lefts = 0, lands = 0 })
function Stepper.on_animation_event(self, name, data)
    if name == "step" and data == "left" then
        self.lefts += 1
    elseif name == "land" and data == nil then
        self.lands += 1
    end
end
return Stepper
"#;

/// An animator's graph driven by name (ADR 0124's E12-5c): a number, a flag and a trigger
/// set and fired every tick, and the state the player wrote read.
const RIDER: &str = r#"--!strict
local Rider = component("Rider", { idles = 0 })
function Rider.on_tick(self, dt: number)
    local a = self.Animator
    a:set("speed", 3.5)
    a:set("armed", true)
    a:trigger("jump")
    if a.state == "Idle" then
        self.idles += 1
    end
end
return Rider
"#;

/// Watchers, each with a trigger, a ball falling through it and a floor named after it.
const WATCHERS: usize = 10;

/// Strollers, each with a camera and an arm; walkers, each with a camera.
const STROLLERS: usize = 10;
const WALKERS: usize = 10;

/// Number prop `name` of script component `component` on `e`.
fn read_f32_of(world: &World, scripts: &Scripts, e: Entity, component: &str, name: &str) -> f32 {
    let id = scripts.component(component).unwrap();
    let engine_reflect::TypeKind::Struct(fields) = &world.components().type_info(id).unwrap().kind
    else {
        unreachable!()
    };
    let at = fields.iter().find(|f| f.name == name).unwrap().offset;
    let base = world.get_raw(e, id).unwrap().as_ptr();
    // SAFETY: a number prop (f32) at its offset in the component's live value.
    unsafe { base.add(at).cast::<f32>().read_unaligned() }
}

#[test]
fn the_engines_api_for_scripts_allocates_nothing_in_steady_state() {
    let dir =
        std::env::temp_dir().join(format!("ofelia-scripts-zero-alloc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("scripts")).unwrap();
    std::fs::write(dir.join("scripts/door.luau"), DOOR).unwrap();
    std::fs::write(dir.join("scripts/watcher.luau"), WATCHER).unwrap();
    std::fs::write(dir.join("scripts/stroller.luau"), STROLLER).unwrap();
    std::fs::write(dir.join("scripts/walker.luau"), WALKER).unwrap();
    std::fs::write(dir.join("scripts/spin.luau"), SPIN).unwrap();
    std::fs::write(dir.join("scripts/relay.luau"), RELAY).unwrap();
    std::fs::write(dir.join("scripts/dice.luau"), DICE).unwrap();
    std::fs::write(dir.join("scripts/chime.luau"), CHIME).unwrap();
    std::fs::write(dir.join("scripts/stepper.luau"), STEPPER).unwrap();
    std::fs::write(dir.join("scripts/rider.luau"), RIDER).unwrap();
    let mut world = World::new();
    world.insert_resource(AnimationEvents::new());
    // The riders' graph, as the player reads it: its parameters by slot, sorted by name.
    let graph = engine_scene::AssetId::new();
    let mut params = engine_runtime::GraphParams::default();
    params.insert(graph, [("armed", false), ("jump", true), ("speed", false)]);
    world.insert_resource(params);
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

    let strollers: Vec<Entity> = (0..STROLLERS)
        .map(|i| {
            let stroller = world.spawn((Transform::from_xyz(i as f32 * 3.0, 0.0, -30.0),));
            for name in ["Arm", "Camera"] {
                let child = world.spawn((Name::new(name), Transform::IDENTITY));
                world.set_parent(child, Some(stroller)).unwrap();
            }
            assert!(scripts.add(&mut world, stroller, "Stroller"));
            stroller
        })
        .collect();
    // The game's input: walking with WASD, running with Shift, looking with the mouse.
    let mut map = engine_input::InputMap::new();
    let walk = map.add_axis2d("move").unwrap();
    map.bind(walk, engine_input::Binding::wasd()).unwrap();
    let run = map.add_button("run").unwrap();
    map.bind(
        run,
        engine_input::Binding::button(engine_input::Key::ShiftLeft),
    )
    .unwrap();
    let look = map.add_axis2d("look").unwrap();
    map.bind(look, engine_input::Binding::mouse_motion(1.0))
        .unwrap();
    world.insert_resource(engine_input::Input::new(map));
    let walkers: Vec<Entity> = (0..WALKERS)
        .map(|i| {
            let walker = world.spawn((Transform::from_xyz(i as f32 * 3.0, 0.0, -60.0),));
            let camera = world.spawn((Name::new("Camera"), Transform::IDENTITY));
            world.set_parent(camera, Some(walker)).unwrap();
            assert!(scripts.add(&mut world, walker, "Walker"));
            walker
        })
        .collect();
    // Ten pairs of relays, each the other's partner.
    let relay = scripts.component("Relay").unwrap();
    let engine_reflect::TypeKind::Struct(relay_fields) =
        &world.components().type_info(relay).unwrap().kind
    else {
        unreachable!()
    };
    let offset = |name: &str| relay_fields.iter().find(|f| f.name == name).unwrap().offset;
    let (partner, got, fired) = (offset("partner"), offset("got"), offset("fired"));
    let relays: Vec<Entity> = (0..20)
        .map(|_| {
            let e = world.spawn((Transform::IDENTITY,));
            assert!(scripts.add(&mut world, e, "Relay"));
            e
        })
        .collect();
    for pair in relays.chunks(2) {
        for (me, other) in [(pair[0], pair[1]), (pair[1], pair[0])] {
            let base = world.get_raw_mut(me, relay).unwrap().as_ptr();
            // SAFETY: Relay's `partner`, an entity at its offset.
            unsafe { base.add(partner).cast::<Entity>().write_unaligned(other) };
        }
    }
    let dice = world.spawn((Transform::IDENTITY,));
    assert!(scripts.add(&mut world, dice, "Dice"));
    let chimes: Vec<Entity> = (0..10)
        .map(|_| {
            let chime = world.spawn((Transform::IDENTITY,));
            assert!(scripts.add(&mut world, chime, "Chime"));
            chime
        })
        .collect();
    let steppers: Vec<Entity> = (0..10)
        .map(|_| {
            let stepper = world.spawn((Transform::IDENTITY,));
            assert!(scripts.add(&mut world, stepper, "Stepper"));
            stepper
        })
        .collect();
    let riders: Vec<Entity> = (0..10)
        .map(|_| {
            let rider = world.spawn((
                Transform::IDENTITY,
                Animator {
                    graph: engine_scene::AssetRef::file(graph),
                    state: String::from("Idle"),
                    ..Animator::default()
                },
            ));
            assert!(scripts.add(&mut world, rider, "Rider"));
            rider
        })
        .collect();
    // Each stepper's clip fires a step and a landing every tick, as the player fires them.
    let texts: [std::sync::Arc<str>; 4] = ["step", "left", "land", ""].map(std::sync::Arc::from);
    let started = std::time::Instant::now();

    let spinners: Vec<Entity> = (0..1000)
        .map(|i| {
            let e = world.spawn((Transform::from_xyz(i as f32, 10.0, 0.0),));
            assert!(scripts.add(&mut world, e, "Spin"));
            e
        })
        .collect();

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
        // W held for 40 ticks of every 80, Shift for 20; the mouse moves every tick.
        let now = started + std::time::Duration::from_millis(n as u64 * 16);
        if let Some(input) = world.resource_mut::<engine_input::Input>() {
            for (key, at) in [
                (engine_input::Key::KeyW, 40),
                (engine_input::Key::ShiftLeft, 20),
            ] {
                if n.is_multiple_of(80) || n % 80 == at {
                    input.handle(&engine_input::InputEvent {
                        time: now,
                        kind: engine_input::EventKind::Key {
                            key,
                            pressed: n.is_multiple_of(80),
                        },
                    });
                }
            }
            input.handle(&engine_input::InputEvent {
                time: now,
                kind: engine_input::EventKind::MouseMotion { dx: 1.0, dy: 2.0 },
            });
            input.sample_tick(now);
        }
        if let Some(fired) = world.resource_mut::<AnimationEvents>() {
            fired.clear();
            for &entity in &steppers {
                for (name, data) in [(&texts[0], &texts[1]), (&texts[2], &texts[3])] {
                    fired.push(AnimationEvent {
                        entity,
                        name: std::sync::Arc::clone(name),
                        data: std::sync::Arc::clone(data),
                    });
                }
            }
        }
        let before = counting::allocations();
        scripts.tick_with(world, dt, Some(&physics));
        *counted += counting::allocations() - before;
        physics.step(world, dt).unwrap();
        if let Some(input) = world.resource_mut::<engine_input::Input>() {
            input.sample_frame(now);
        }
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
    // Each relay heard its partner every tick, and its timers fired (all but the last few
    // set, still pending).
    for &r in &relays {
        let base = world.get_raw(r, relay).unwrap().as_ptr();
        // SAFETY: Relay's `got` and `fired`, f32s at their offsets.
        let (got, fired) = unsafe {
            (
                base.add(got).cast::<f32>().read_unaligned(),
                base.add(fired).cast::<f32>().read_unaligned(),
            )
        };
        assert_eq!(got, 840.0, "a relay missed messages");
        assert!(fired >= 2.0 * 830.0, "{fired}");
    }
    // The dice rolled every tick: about 4 a tick (3.5 and a half), every point inside.
    let id = scripts.component("Dice").unwrap();
    let engine_reflect::TypeKind::Struct(dice_fields) =
        &world.components().type_info(id).unwrap().kind
    else {
        unreachable!()
    };
    let at = |name: &str| dice_fields.iter().find(|f| f.name == name).unwrap().offset;
    let base = world.get_raw(dice, id).unwrap().as_ptr();
    // SAFETY: Dice's `sum` and `inside`, f32s at their offsets.
    let (sum, inside) = unsafe {
        (
            base.add(at("sum")).cast::<f32>().read_unaligned(),
            base.add(at("inside")).cast::<f32>().read_unaligned(),
        )
    };
    assert!((3.5..4.5).contains(&(sum / 840.0)), "{sum}");
    assert_eq!(inside, 840.0);
    // The spinners turned 90° a second for 840 ticks.
    let turned = world.get::<Transform>(spinners[0]).unwrap().rotation;
    let expected =
        Quat::from_rotation_y((840.0 * dt * 90f32.to_radians()) % core::f32::consts::TAU);
    assert!(
        turned.dot(expected).abs() > 0.999,
        "{turned:?} against {expected:?}"
    );
    let id = scripts.component("Stroller").unwrap();
    let engine_reflect::TypeKind::Struct(stroller_fields) =
        &world.components().type_info(id).unwrap().kind
    else {
        unreachable!()
    };
    let steps = stroller_fields
        .iter()
        .find(|f| f.name == "steps")
        .unwrap()
        .offset;
    for &w in &strollers {
        let base = world.get_raw(w, id).unwrap().as_ptr();
        // SAFETY: Stroller's `steps`, an f32 at its offset.
        let steps = unsafe { base.add(steps).cast::<f32>().read_unaligned() };
        assert_eq!(steps, 840.0 * 2.0, "a stroller stopped");
    }
    // The walkers walked forward (−z) while W was held, and their cameras looked down.
    for &w in &walkers {
        let t = world.get::<Transform>(w).unwrap();
        assert!(t.translation.z < -60.5, "{:?}", t.translation);
        let camera = world.children(w).next().unwrap();
        let pitch = world.get::<Transform>(camera).unwrap().rotation;
        assert!((pitch * Vec3::NEG_Z).y < -0.1, "{pitch:?}");
    }
    for &w in &watchers {
        assert_eq!(read(w, "looks"), 840.0, "a watcher stopped");
        assert!(read(w, "hits") > 0.0, "no ray hit its floor");
        assert!(read(w, "touches") > 0.0, "no ball touched its trigger");
        assert!(read(w, "leaves") > 0.0, "no ball left its trigger");
    }
    // The chimes rang every tick, and said what they sound.
    let id = scripts.component("Chime").unwrap();
    let engine_reflect::TypeKind::Struct(chime_fields) =
        &world.components().type_info(id).unwrap().kind
    else {
        unreachable!()
    };
    let at = |name: &str| chime_fields.iter().find(|f| f.name == name).unwrap().offset;
    for &c in &chimes {
        let base = world.get_raw(c, id).unwrap().as_ptr();
        // SAFETY: Chime's `rings`, an f32, and `said`, a `String`, at their offsets.
        let (rings, said) = unsafe {
            (
                base.add(at("rings")).cast::<f32>().read_unaligned(),
                engine_reflect::read_str(base.add(at("said"))),
            )
        };
        assert_eq!(rings, 840.0, "a chime stopped");
        assert_eq!(said, "ui/chime.ogg");
    }
    // The steppers heard every tick's step and landing.
    let id = scripts.component("Stepper").unwrap();
    let engine_reflect::TypeKind::Struct(stepper_fields) =
        &world.components().type_info(id).unwrap().kind
    else {
        unreachable!()
    };
    let at = |name: &str| {
        stepper_fields
            .iter()
            .find(|f| f.name == name)
            .unwrap()
            .offset
    };
    for &s in &steppers {
        let base = world.get_raw(s, id).unwrap().as_ptr();
        // SAFETY: Stepper's `lefts` and `lands`, f32s at their offsets.
        let heard = unsafe {
            (
                base.add(at("lefts")).cast::<f32>().read_unaligned(),
                base.add(at("lands")).cast::<f32>().read_unaligned(),
            )
        };
        assert_eq!(heard, (840.0, 840.0), "a stepper missed its events");
    }
    // The riders set their numbers and flags and fired their trigger every tick (nothing
    // took it), and read the state.
    for &r in &riders {
        let a = world.get::<Animator>(r).unwrap();
        assert_eq!(&a.params[..3], &[1.0, 840.0, 3.5], "armed, jump, speed");
        assert_eq!(read_f32_of(&world, &scripts, r, "Rider", "idles"), 840.0);
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(counted, 0, "allocations in 600 ticks of scripts");
}
