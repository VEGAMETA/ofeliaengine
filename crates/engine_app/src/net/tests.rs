#![allow(clippy::unwrap_used, clippy::float_cmp)]

use super::*;
use engine_core::math::Mat4;
use engine_ecs::{Transform, reflect_component};
use engine_reflect::Reflect;
use engine_render::gpu_scene::SceneInstance;
use engine_render::material::MaterialId;
use engine_render::mesh::MeshId;
use engine_runtime::{Runtime, SceneSink};

const PROTOCOL: u64 = 0x0f1e_7e57;

/// A player's position, replicated.
#[derive(Debug, Clone, Copy, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable, Reflect)]
#[repr(C)]
struct Pos {
    #[reflect(net_quantum = 0.001)]
    x: f32,
}
reflect_component!(pod Pos);

/// What a player sends every tick.
#[derive(Debug, Clone, Copy, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Move {
    dx: f32,
}

/// A one-shot event both ways.
#[derive(Debug, Clone, Copy, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Ping {
    n: u32,
}

fn schema() -> NetSchema {
    NetSchema::new(PROTOCOL).replicate::<Pos>().event::<Ping>()
}

/// A scene that takes everything and remembers nothing.
#[derive(Default)]
struct Nowhere;

impl SceneSink for Nowhere {
    type Id = u32;
    fn insert(&mut self, _: SceneInstance) -> u32 {
        0
    }
    fn remove(&mut self, _: u32) -> bool {
        true
    }
    fn set_transform(&mut self, _: u32, _: &Mat4) -> bool {
        true
    }
    fn set_mesh(&mut self, _: u32, _: MeshId) {}
    fn set_material(&mut self, _: u32, _: MaterialId) {}
}

/// The shared tick: every pawn moves by its input.
fn tick(world: &mut World, dt: f32) {
    let mut q = QueryState::<(&NetInput<Move>, &mut Pos)>::new(world);
    q.for_each(world, |(input, pos)| pos.x += input.input.dx * dt);
}

/// One machine: its world and runtime, its session, what it sends.
struct Machine {
    runtime: Runtime<u32>,
    scene: Nowhere,
    net: Box<dyn NetDrive>,
    input: Move,
}

impl Machine {
    fn new() -> Self {
        let config = NetConfig::new::<Move>(schema);
        let mut world = World::new();
        world.insert_resource(config.network());
        Self {
            runtime: Runtime::new(world, 64.0),
            scene: Nowhere,
            net: config.driver(64.0),
            input: Move::default(),
        }
    }

    fn net(&mut self) -> &mut Network {
        self.runtime.world.resource_mut::<Network>().unwrap()
    }

    /// One frame as the player runs it (`Player::frame`'s order).
    fn frame(&mut self, elapsed: f64) {
        let Self {
            runtime,
            scene,
            net,
            input,
        } = self;
        net.begin_frame(&mut runtime.world, Instant::now());
        let set_input = |world: &mut World| {
            world.resource_mut::<Network>().unwrap().set_input(&*input);
        };
        if net.is_client() {
            set_input(&mut runtime.world);
            runtime.frame_driven(
                &(),
                scene,
                |w, (), dt| tick(w, dt),
                |w, step| net.client_frame(w, Instant::now(), step),
                |_, (), _| {},
            );
        } else {
            let hosting = matches!(
                runtime.world.resource::<Network>().unwrap().state(),
                NetState::Hosting { .. }
            );
            runtime.frame(
                elapsed,
                &(),
                scene,
                |w, (), dt| {
                    if hosting {
                        set_input(w);
                        net.host_tick_start(w, Instant::now());
                    }
                    tick(w, dt);
                    if hosting {
                        net.host_tick_end(w, Instant::now());
                    }
                },
                |_, (), _| {},
            );
        }
    }
}

#[allow(clippy::disallowed_methods)]
fn wait_ms(ms: u64) {
    std::thread::sleep(std::time::Duration::from_millis(ms));
}

/// Frames of both machines in real time until `done`, at most 5 s.
fn run_until(
    host: &mut Machine,
    guest: &mut Machine,
    mut done: impl FnMut(&mut Machine, &mut Machine) -> bool,
) {
    let mut last = Instant::now();
    for _ in 0..1_000 {
        wait_ms(4);
        let now = Instant::now();
        let elapsed = (now - last).as_secs_f64();
        last = now;
        host.frame(elapsed);
        guest.frame(elapsed);
        if done(host, guest) {
            return;
        }
    }
    panic!("timed out");
}

#[test]
fn a_guest_joins_a_host_by_invite_plays_and_leaves() {
    let mut host = Machine::new();
    let mut guest = Machine::new();
    host.net().host(HostOptions {
        map_port: false,
        ..HostOptions::default()
    });
    host.frame(0.0);
    assert!(host.net().events().any(|e| e == NetEvent::Hosting));
    assert!(matches!(
        host.net().state(),
        NetState::Hosting { players: 0 }
    ));
    assert!(host.net().addresses().iter().any(|a| a.ip().is_loopback()));
    // The host plays in its own world.
    let me = host
        .runtime
        .world
        .spawn((Replicated::new(), Pos::default(), Transform::IDENTITY));
    host.net().set_local_pawn(Some(me));
    host.input = Move { dx: 0.5 };

    let invite = host.net().invite().unwrap();
    guest.net().join(&invite);
    guest.input = Move { dx: 1.0 };
    let mut joined = None;
    let mut connected = false;
    let mut mine = None;
    let mut theirs = None;
    run_until(&mut host, &mut guest, |host, guest| {
        // The host gives a joining player a pawn.
        for event in host.net().events().collect::<Vec<_>>() {
            if let NetEvent::PlayerJoined(id) = event {
                let pawn = host.runtime.world.spawn((
                    Replicated::new(),
                    Pos::default(),
                    Transform::IDENTITY,
                ));
                host.net().give(pawn, Some(id));
                joined = Some((id, pawn));
            }
        }
        for event in guest.net().events().collect::<Vec<_>>() {
            match event {
                NetEvent::Connected => connected = true,
                NetEvent::Spawned { entity, predicted } if predicted => mine = Some(entity),
                NetEvent::Spawned { entity, .. } => theirs = Some(entity),
                _ => {}
            }
        }
        // Both pawns have moved on both machines.
        let moved = |m: &Machine, e: Option<Entity>| {
            e.and_then(|e| m.runtime.world.get::<Pos>(e))
                .is_some_and(|p| p.x > 0.2)
        };
        connected && moved(guest, mine) && moved(guest, theirs) && moved(host, joined.map(|j| j.1))
    });
    assert!(matches!(guest.net().state(), NetState::Joined));
    assert!(matches!(
        host.net().state(),
        NetState::Hosting { players: 1 }
    ));
    // The guest predicts its own pawn: ahead of where the host has it.
    let (id, pawn) = joined.unwrap();
    let on_host = host.runtime.world.get::<Pos>(pawn).unwrap().x;
    let predicted = guest.runtime.world.get::<Pos>(mine.unwrap()).unwrap().x;
    assert!(predicted >= on_host - 0.05, "{predicted} vs {on_host}");
    assert!(guest.net().alpha() >= 0.0 && guest.net().alpha() <= 1.0);

    // One-shot events, both ways.
    guest.net().send_event(&Ping { n: 7 }, To::All, true);
    host.net().send_event(&Ping { n: 9 }, To::Player(id), true);
    let (mut at_host, mut at_guest) = (None, None);
    run_until(&mut host, &mut guest, |host, guest| {
        host.net()
            .read_events::<Ping>(|_, from, p| at_host = Some((from, p.n)));
        guest
            .net()
            .read_events::<Ping>(|_, from, p| at_guest = Some((from, p.n)));
        at_host.is_some() && at_guest.is_some()
    });
    assert_eq!(at_host, Some((Some(id), 7)));
    assert_eq!(at_guest, Some((None, 9)));

    // The guest leaves: its replicated world goes, the host hears it.
    guest.net().leave();
    let mut left = false;
    run_until(&mut host, &mut guest, |host, _| {
        left |= host
            .net()
            .events()
            .any(|e| matches!(e, NetEvent::PlayerLeft(p, _) if p == id));
        left
    });
    assert!(matches!(guest.net().state(), NetState::Offline));
    assert!(guest.runtime.world.get::<Pos>(mine.unwrap()).is_none());
    assert!(matches!(
        host.net().state(),
        NetState::Hosting { players: 0 }
    ));
}

#[test]
fn a_bad_invite_is_refused_and_an_offline_machine_does_nothing() {
    let mut guest = Machine::new();
    guest.net().join("not an invite");
    guest.frame(0.016);
    assert!(
        guest
            .net()
            .events()
            .any(|e| matches!(e, NetEvent::Disconnected(_)))
    );
    assert!(matches!(guest.net().state(), NetState::Offline));
    assert!(guest.net().invite().is_none());
    // Offline: the ticks run as in singleplayer, nothing is sent.
    let e = guest
        .runtime
        .world
        .spawn((NetInput::<Move>::default(), Pos::default()));
    guest
        .runtime
        .world
        .get_mut::<NetInput<Move>>(e)
        .unwrap()
        .input
        .dx = 1.0;
    for _ in 0..64 {
        guest.frame(1.0 / 64.0);
    }
    assert!((guest.runtime.world.get::<Pos>(e).unwrap().x - 1.0).abs() < 0.05);
}
