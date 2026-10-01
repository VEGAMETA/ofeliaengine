//! A small game and a harness that runs a server and clients over the in-memory network
//! on a virtual clock (deterministic for a seed).

#![allow(dead_code, clippy::unwrap_used, missing_docs, unreachable_pub)]

use engine_core::math::{Quat, Vec3};
use engine_ecs::{Entity, QueryState, Transform, World, bytemuck, reflect_component};
use engine_net::{
    Client, ClientEvent, ClientSettings, FieldHints, HitCapsule, NetInput, NetSchema, Replicated,
    Server, ServerEvent, ServerSettings, SubTick,
};
use engine_net_transport::memory::{LinkConditions, MemoryNetwork, MemorySocket};
use engine_net_transport::{
    ConnectToken, Endpoint, Key, ServerConfig, TokenParams, USER_DATA_BYTES, unix_now,
};
use engine_reflect::Reflect;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

pub const PROTOCOL: u64 = 0x0f1e_0074;
pub const SERVER: &str = "10.0.0.1:27015";

/// A server-driven object: moves and bounces inside a box.
#[derive(Debug, Clone, Copy, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable, Reflect)]
#[repr(C)]
pub struct Mover {
    #[reflect(net_quantum = 0.001)]
    pub pos: Vec3,
    #[reflect(net_quantum = 0.001)]
    pub vel: Vec3,
}
reflect_component!(pod Mover);

/// A player's character: moved by input on the server and predicted by its owner.
#[derive(Debug, Clone, Copy, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable, Reflect)]
#[repr(C)]
pub struct Pawn {
    #[reflect(net_quantum = 0.001)]
    pub pos: Vec3,
    #[reflect(net_quantum = 0.001)]
    pub vel: Vec3,
    #[reflect(net_quantum = 0.0001)]
    pub yaw: f32,
    /// Ticks until the weapon can fire again.
    pub cooldown: u32,
    pub health: i32,
    pub shots: u32,
}
reflect_component!(pod Pawn);

/// What a player sends every tick.
#[derive(Debug, Clone, Copy, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct Input {
    pub move_x: f32,
    pub move_z: f32,
    pub yaw: f32,
    /// 0: no shot; otherwise the sub-tick of the trigger pull (`SubTick`) + 1.
    pub fire: u8,
    pub _pad: [u8; 3],
}

impl Input {
    pub fn fire_subtick(&self) -> Option<SubTick> {
        self.fire.checked_sub(1).map(SubTick)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct Ping {
    pub n: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct Chat {
    pub n: u32,
}

pub fn schema() -> NetSchema {
    NetSchema::new(PROTOCOL)
        .replicate::<Mover>()
        .replicate::<Pawn>()
        .replicate_with::<Transform>(
            FieldHints::new()
                .quantum("translation", 0.001)
                .skip("scale"),
        )
        .event::<Ping>()
        .event::<Chat>()
}

pub const SPEED: f32 = 5.0;
pub const COOLDOWN: u32 = 8;

/// The systems both sides run for pawns (prediction on the client).
pub fn simulate(
    world: &mut World,
    q: &mut QueryState<(&'static NetInput<Input>, &'static mut Pawn)>,
    dt: f32,
) {
    q.for_each(world, |(input, pawn)| {
        let i = input.input;
        let dir = Vec3::new(i.move_x, 0.0, i.move_z).clamp_length_max(1.0);
        pawn.vel = dir * SPEED;
        pawn.pos += pawn.vel * dt;
        pawn.yaw = i.yaw;
        if pawn.cooldown > 0 {
            pawn.cooldown -= 1;
        }
        if i.fire != 0 && pawn.cooldown == 0 {
            pawn.cooldown = COOLDOWN;
            pawn.shots += 1;
        }
    });
}

/// Server-only: movers bounce inside ±50 m.
pub fn move_movers(world: &mut World, q: &mut QueryState<&'static mut Mover>, dt: f32) {
    q.for_each(world, |m| {
        m.pos += m.vel * dt;
        for k in 0..3 {
            if m.pos[k].abs() > 50.0 {
                m.vel[k] = -m.vel[k];
                m.pos[k] = m.pos[k].clamp(-50.0, 50.0);
            }
        }
    });
}

/// Keeps `Transform` (hit shapes, rendering) on the pawn's position.
pub fn sync_transforms(
    world: &mut World,
    q: &mut QueryState<(&'static Pawn, &'static mut Transform)>,
) {
    q.for_each(world, |(p, t)| {
        t.translation = p.pos;
        t.rotation = Quat::from_rotation_y(p.yaw);
    });
}

pub fn addr(s: &str) -> SocketAddr {
    s.parse().unwrap()
}

pub struct ServerSide {
    pub world: World,
    pub server: Server<Input, Endpoint<MemorySocket>>,
    pub pawn_q: QueryState<(&'static NetInput<Input>, &'static mut Pawn)>,
    pub mover_q: QueryState<&'static mut Mover>,
    pub sync_q: QueryState<(&'static Pawn, &'static mut Transform)>,
    pub pawns: Vec<(engine_net::ClientId, Entity)>,
    pub events: Vec<ServerEvent>,
}

pub struct ClientSide {
    pub world: World,
    pub client: Client<Input, Endpoint<MemorySocket>>,
    pub pawn_q: QueryState<(&'static NetInput<Input>, &'static mut Pawn)>,
    pub sync_q: QueryState<(&'static Pawn, &'static mut Transform)>,
    pub input: Input,
    pub events: Vec<ClientEvent>,
    pub frames: Vec<engine_net::FrameInfo>,
}

/// A server-side hook run every tick after the shared systems.
pub type Rules = Box<dyn FnMut(&mut ServerSide)>;

pub struct Sim {
    pub net: MemoryNetwork,
    pub key: Key,
    pub t0: Instant,
    pub now_us: u64,
    pub server: ServerSide,
    pub clients: Vec<ClientSide>,
    pub next_tick_us: u64,
    pub next_frame_us: u64,
    pub frame_us: u64,
    pub tick_us: u64,
    pub ticks: u64,
    /// Server-side hook run each tick after the shared systems.
    pub rules: Option<Rules>,
}

pub fn token(key: &Key, client_id: u64) -> ConnectToken {
    ConnectToken::generate(
        &TokenParams {
            protocol_id: PROTOCOL,
            client_id,
            expire_seconds: 60,
            timeout_seconds: 10,
            server_addresses: &[addr(SERVER)],
            user_data: [0; USER_DATA_BYTES],
        },
        key,
        unix_now(),
    )
    .unwrap()
}

impl Sim {
    pub fn new(seed: u64, conditions: LinkConditions, settings: ServerSettings) -> Self {
        let net = MemoryNetwork::new(seed);
        net.set_default_conditions(conditions);
        let key = Key::random();
        let t0 = Instant::now();
        let schema = schema();
        let endpoint = Endpoint::server(
            net.bind(addr(SERVER)),
            schema.endpoint_config(),
            ServerConfig {
                private_key: key.clone(),
                max_clients: 16,
                public_addresses: vec![addr(SERVER)],
            },
            t0,
        )
        .unwrap();
        let mut world = World::new();
        let server = Server::new(endpoint, schema, settings, &mut world);
        let pawn_q = QueryState::new(&mut world);
        let mover_q = QueryState::new(&mut world);
        let sync_q = QueryState::new(&mut world);
        let tick_us = (1e6 / settings.tick_rate).round() as u64;
        Self {
            net,
            key,
            t0,
            now_us: 0,
            server: ServerSide {
                world,
                server,
                pawn_q,
                mover_q,
                sync_q,
                pawns: Vec::new(),
                events: Vec::new(),
            },
            clients: Vec::new(),
            next_tick_us: 0,
            next_frame_us: 0,
            frame_us: 8333,
            tick_us,
            ticks: 0,
            rules: None,
        }
    }

    pub fn now(&self) -> Instant {
        self.t0 + Duration::from_micros(self.now_us)
    }

    pub fn add_client(&mut self) -> usize {
        let i = self.clients.len();
        let schema = schema();
        let endpoint = Endpoint::client(
            self.net.bind(addr(&format!("10.0.1.{}:5000", i + 1))),
            schema.endpoint_config(),
            token(&self.key, 100 + i as u64),
            self.now(),
        )
        .unwrap();
        let mut world = World::new();
        let client = Client::new(endpoint, schema, ClientSettings::default(), &mut world);
        let pawn_q = QueryState::new(&mut world);
        let sync_q = QueryState::new(&mut world);
        self.clients.push(ClientSide {
            world,
            client,
            pawn_q,
            sync_q,
            input: Input::default(),
            events: Vec::new(),
            frames: Vec::new(),
        });
        i
    }

    pub fn spawn_movers(&mut self, n: usize, seed: u32) {
        let mut x = seed | 1;
        let mut rnd = move || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x as f32 / u32::MAX as f32) * 2.0 - 1.0
        };
        for _ in 0..n {
            let m = Mover {
                pos: Vec3::new(rnd() * 40.0, rnd() * 5.0 + 5.0, rnd() * 40.0),
                vel: Vec3::new(rnd() * 3.0, 0.0, rnd() * 3.0),
            };
            self.server.world.spawn((Replicated::new(), m));
        }
    }

    pub fn server_tick(&mut self) {
        let now = self.now();
        let s = &mut self.server;
        s.server.update(&mut s.world, now);
        while let Some(e) = s.server.poll_event() {
            s.events.push(e);
            match e {
                ServerEvent::ClientConnected(c) => {
                    let start = Vec3::new(f32::from(c.0) * 2.0, 0.0, 0.0);
                    let pawn = s.world.spawn((
                        Replicated::new(),
                        Pawn {
                            pos: start,
                            health: 100,
                            ..Pawn::default()
                        },
                        Transform::from_translation(start),
                        HitCapsule::upright(1.8, 0.3),
                    ));
                    s.server.set_owner(&mut s.world, pawn, Some(c));
                    s.pawns.push((c, pawn));
                }
                ServerEvent::ClientDisconnected(c, _) => {
                    if let Some(i) = s.pawns.iter().position(|p| p.0 == c) {
                        let (_, e) = s.pawns.remove(i);
                        s.world.despawn(e);
                    }
                }
            }
        }
        let dt = s.server.dt();
        simulate(&mut s.world, &mut s.pawn_q, dt);
        move_movers(&mut s.world, &mut s.mover_q, dt);
        sync_transforms(&mut s.world, &mut s.sync_q);
        if let Some(rules) = &mut self.rules {
            rules(&mut self.server);
        }
        let s = &mut self.server;
        s.server.send(&mut s.world, now);
        self.ticks += 1;
    }

    pub fn client_frame(&mut self, i: usize) {
        let now = self.now();
        let c = &mut self.clients[i];
        let ClientSide {
            world,
            client,
            pawn_q,
            sync_q,
            input,
            ..
        } = c;
        let info = client.frame(world, now, *input, |w, dt| {
            simulate(w, pawn_q, dt);
            sync_transforms(w, sync_q);
        });
        sync_transforms(world, sync_q);
        c.frames.push(info);
        while let Some(e) = c.client.poll_event() {
            c.events.push(e);
        }
    }

    /// Runs `seconds` of virtual time: server ticks at its rate, client frames at
    /// `frame_us`, interleaved in time order. `each_frame` runs after every client frame.
    pub fn run(&mut self, seconds: f64) {
        self.run_with(seconds, |_| {});
    }

    pub fn run_with(&mut self, seconds: f64, mut each_frame: impl FnMut(&mut Sim)) {
        let end = self.now_us + (seconds * 1e6) as u64;
        while self.now_us < end {
            if self.next_tick_us <= self.next_frame_us {
                self.now_us = self.next_tick_us;
                self.server_tick();
                self.next_tick_us += self.tick_us;
            } else {
                self.now_us = self.next_frame_us;
                for i in 0..self.clients.len() {
                    self.client_frame(i);
                }
                each_frame(self);
                self.next_frame_us += self.frame_us;
            }
        }
    }

    /// The server's replicated entities by network slot.
    pub fn server_entity(&self, slot: u32) -> Option<Entity> {
        let mut q = None;
        for (e, r) in iter_replicated(&self.server.world) {
            if r.net_id().is_some_and(|id| id.slot == slot) {
                q = Some(e);
            }
        }
        q
    }
}

/// Every replicated entity of a world with its `Replicated`.
pub fn iter_replicated(world: &World) -> Vec<(Entity, Replicated)> {
    let id = world.component_id::<Replicated>().unwrap();
    let mut out = Vec::new();
    for arch in world.archetypes() {
        let Some(col) = arch.column_index(id) else {
            continue;
        };
        for chunk in 0..arch.chunk_count() {
            let reps: &[Replicated] = bytemuck::cast_slice(arch.pod_column(chunk, col).unwrap());
            for (e, r) in arch.chunk_entities(chunk).iter().zip(reps) {
                out.push((*e, *r));
            }
        }
    }
    out.sort_by_key(|(_, r)| r.net_id());
    out
}

/// Pairs of (server entity, client entity) with the same network id.
pub fn matched(server: &World, client: &World) -> (Vec<(Entity, Entity)>, usize, usize) {
    let s = iter_replicated(server);
    let c = iter_replicated(client);
    let mut pairs = Vec::new();
    for (ce, cr) in &c {
        if let Some((se, _)) = s.iter().find(|(_, sr)| sr.net_id() == cr.net_id()) {
            pairs.push((*se, *ce));
        }
    }
    (pairs, s.len(), c.len())
}

pub fn conditions(latency_ms: u64, jitter_ms: u64, loss: f32) -> LinkConditions {
    LinkConditions::new(
        Duration::from_millis(latency_ms),
        Duration::from_millis(jitter_ms),
        loss,
    )
}
