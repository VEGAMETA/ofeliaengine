//! A tiny co-op game (Portal 2 style): one player stands on a button so the other can
//! pass a door, then they shoot at each other with a lag-compensated hitscan gun.
//! Characters are Jolt `CharacterVirtual`s (ADR 0066).
//!
//! What is networking-specific here is marked `// net:`. The rest is the game as it
//! would be written for singleplayer.

#![allow(dead_code, unreachable_pub, missing_docs)]

use engine_core::math::{Quat, Vec3};
use engine_ecs::{
    Entity, QueryState, Transform, World, bytemuck, pod_component, reflect_component,
};
use engine_net::{HitCapsule, NetInput, NetSchema, Replicated, Server, SubTick, Target};
use engine_net_transport::Transport;
use engine_physics::{
    BodyDesc, BodyId, CharacterDesc, CharacterId, CharacterMotor, MotionType, PhysicsBackend,
    PhysicsSettings, QueryFilter, Ray, ShapeDesc,
};
use engine_physics_jolt::JoltBackend;
use engine_reflect::Reflect;

// net: the networked state, with its precision.
/// A player's character.
#[derive(Debug, Clone, Copy, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable, Reflect)]
#[repr(C)]
pub struct Player {
    #[reflect(net_quantum = 0.001)]
    pub pos: Vec3,
    #[reflect(net_quantum = 0.001)]
    pub vel: Vec3,
    #[reflect(net_quantum = 0.0001)]
    pub yaw: f32,
    pub grounded: u8,
    pub cooldown: u8,
    pub health: u8,
    pub _pad: u8,
}
reflect_component!(pod Player);

/// The door: how far open (0..1).
#[derive(Debug, Clone, Copy, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable, Reflect)]
#[repr(C)]
pub struct Door {
    #[reflect(net_quantum = 0.001)]
    pub open: f32,
    pub pressed: u32,
}
reflect_component!(pod Door);

// net: one-shot events (state is replicated; these only trigger effects).
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct DoorOpened {
    pub tick: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct Hit {
    pub point: Vec3,
    pub health_left: u32,
}

// net: what a player sends every tick (actions included, ADR 0019).
#[derive(Debug, Clone, Copy, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct PlayerInput {
    pub move_x: f32,
    pub move_z: f32,
    pub yaw: f32,
    pub jump: u8,
    /// 0: no shot; otherwise the trigger's sub-tick + 1.
    pub fire: u8,
    pub _pad: [u8; 2],
}

// net: the schema, built the same way on host and client.
pub const PROTOCOL: u64 = 0x0f1e_c001;

pub fn schema() -> NetSchema {
    NetSchema::new(PROTOCOL)
        .replicate::<Player>()
        .replicate::<Door>()
        .event::<DoorOpened>()
        .event::<Hit>()
}

/// Not replicated: the entity's Jolt character on this machine.
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct CharacterLink {
    pub id: u64,
}
pod_component!(CharacterLink);

pub const BUTTON: Vec3 = Vec3::new(-1.5, 0.0, -3.0);
pub const BUTTON_RADIUS: f32 = 1.0;
pub const DOOR_CLOSED: Vec3 = Vec3::new(0.0, 1.25, 0.0);
pub const EYE: f32 = 1.6;
pub const SPEED: f32 = 4.0;
pub const COOLDOWN: u8 = 12;
pub const DAMAGE: u8 = 10;

/// The level, its physics and the systems.
pub struct Game {
    pub physics: JoltBackend,
    pub door_body: BodyId,
    pub motor: CharacterMotor,
    players: QueryState<(
        &'static NetInput<PlayerInput>,
        &'static mut Player,
        &'static CharacterLink,
    )>,
    transforms: QueryState<(&'static Player, &'static mut Transform)>,
    remote: QueryState<
        (&'static Player, &'static CharacterLink),
        engine_ecs::Without<engine_net::Predicted>,
    >,
    doors: QueryState<(Entity, &'static mut Door)>,
    inputs: QueryState<(Entity, &'static Player, &'static NetInput<PlayerInput>)>,
    shooters: Vec<(Entity, Vec3, f32, f64)>,
}

impl Game {
    /// The level: ground, a wall along x at z = 0 with a 3 m doorway, the door in it.
    pub fn new(world: &mut World) -> Self {
        let mut physics = JoltBackend::new(&PhysicsSettings {
            threads: 0,
            ..PhysicsSettings::default()
        })
        .unwrap_or_else(|e| panic!("physics: {e}"));
        let ground = physics
            .create_shape(&ShapeDesc::Box {
                half_extents: Vec3::new(30.0, 0.5, 30.0),
            })
            .unwrap_or_else(|e| panic!("{e}"));
        let _ = physics.create_body(&BodyDesc::fixed(ground, Vec3::new(0.0, -0.5, 0.0)));
        let wall = physics
            .create_shape(&ShapeDesc::Box {
                half_extents: Vec3::new(9.0, 2.0, 0.2),
            })
            .unwrap_or_else(|e| panic!("{e}"));
        // Wall halves from |x| = 1.5 to 19.5: a 3 m doorway in the middle.
        for x in [-10.5, 10.5] {
            let _ = physics.create_body(&BodyDesc::fixed(wall, Vec3::new(x, 2.0, 0.0)));
        }
        let door = physics
            .create_shape(&ShapeDesc::Box {
                half_extents: Vec3::new(1.5, 1.25, 0.15),
            })
            .unwrap_or_else(|e| panic!("{e}"));
        let door_body = physics
            .create_body(&BodyDesc {
                motion: MotionType::Kinematic,
                ..BodyDesc::dynamic(door, DOOR_CLOSED)
            })
            .unwrap_or_else(|e| panic!("{e}"));
        Self {
            physics,
            door_body,
            motor: CharacterMotor {
                air_control: 0.3,
                ..CharacterMotor::default()
            },
            players: QueryState::new(world),
            transforms: QueryState::new(world),
            remote: QueryState::new(world),
            doors: QueryState::new(world),
            inputs: QueryState::new(world),
            shooters: Vec::new(),
        }
    }

    /// Server: the door entity (replicated, owned by the server).
    pub fn spawn_door(world: &mut World) -> Entity {
        world.spawn((Replicated::new(), Door::default()))
    }

    /// Server: a player's pawn.
    pub fn spawn_player(&mut self, world: &mut World, at: Vec3) -> Entity {
        let player = Player {
            pos: at,
            health: 100,
            ..Player::default()
        };
        let e = world.spawn((
            Replicated::new(), // net: networked
            player,
            Transform::from_translation(at),
            HitCapsule::upright(1.8, 0.3), // net: lag-compensated hit volume
        ));
        self.attach_character(world, e, at);
        e
    }

    /// A Jolt character for `entity` (both sides; on clients for every player, so the
    /// local one bumps into the others).
    pub fn attach_character(&mut self, world: &mut World, entity: Entity, at: Vec3) {
        let c = self
            .physics
            .create_character(&CharacterDesc::capsule(at, 1.8, 0.3))
            .unwrap_or_else(|e| panic!("{e}"));
        world.insert(entity, CharacterLink { id: c.0 });
        if !world.has::<Transform>(entity) {
            world.insert(entity, Transform::from_translation(at));
        }
    }

    pub fn detach_character(&mut self, link: CharacterLink) {
        self.physics.destroy_character(CharacterId(link.id));
    }

    /// The tick systems both sides run (the server for every player, a client for its
    /// own): input → walk, jump, aim, fire cooldown. State comes only from `Player`, so
    /// rollback needs nothing else.
    pub fn simulate(&mut self, world: &mut World, dt: f32) {
        let Self {
            physics,
            motor,
            players,
            ..
        } = self;
        players.for_each(world, |(input, p, link)| {
            let i = input.input;
            let c = CharacterId(link.id);
            let rotation = Quat::from_rotation_y(p.yaw);
            physics.set_character_transform(c, p.pos, rotation);
            let Some(mut state) = physics.character(c) else {
                return;
            };
            state.velocity = p.vel;
            let walk = Vec3::new(i.move_x, 0.0, i.move_z).clamp_length_max(1.0) * SPEED;
            let v = motor.velocity(&state, walk, i.jump != 0, dt);
            if let Some(s) = physics.move_character(c, v, dt) {
                p.pos = s.position;
                p.vel = s.velocity;
                p.grounded = u8::from(s.is_grounded());
            }
            p.yaw = i.yaw;
            p.cooldown = p.cooldown.saturating_sub(1);
            if i.fire != 0 && p.cooldown == 0 {
                p.cooldown = COOLDOWN;
            }
        });
        self.transforms.for_each(world, |(p, t)| {
            t.translation = p.pos;
            t.rotation = Quat::from_rotation_y(p.yaw);
        });
    }

    /// Server only, after `simulate`: the button opens the door, shots hit (lag
    /// compensated), physics steps.
    pub fn server_rules<T: Transport>(
        &mut self,
        world: &mut World,
        server: &mut Server<PlayerInput, T>,
        dt: f32,
    ) {
        // The button: anyone standing on it holds the door open.
        let mut on_button = false;
        let shooters = &mut self.shooters;
        shooters.clear();
        self.inputs.for_each(world, |(e, p, input)| {
            let flat = Vec3::new(p.pos.x - BUTTON.x, 0.0, p.pos.z - BUTTON.z);
            on_button |= flat.length() < BUTTON_RADIUS;
            // Fired this tick (the cooldown was just reset by `simulate`).
            if let Some(sub) = input.input.fire.checked_sub(1).map(SubTick)
                && p.cooldown == COOLDOWN
            {
                // net: the time this player saw, for lag compensation.
                shooters.push((e, p.pos + Vec3::Y * EYE, p.yaw, input.header.view_time(sub)));
            }
        });
        let tick = server.tick();
        let mut opened = false;
        self.doors.for_each(world, |(_, door)| {
            let was = door.pressed;
            door.pressed = u32::from(on_button);
            if door.pressed == 1 && was == 0 && door.open == 0.0 {
                opened = true;
            }
            let target = if on_button { 1.0 } else { 0.0 };
            let step = dt * 1.5;
            door.open = if door.open < target {
                (door.open + step).min(target)
            } else {
                (door.open - step).max(target)
            };
        });
        if opened {
            // net: a reliable one-shot event for sounds and UI.
            server.send_event(Target::All, true, &DoorOpened { tick });
        }
        self.place_door(world);
        for i in 0..self.shooters.len() {
            let (shooter, eye, yaw, view_time) = self.shooters[i];
            let dir = Vec3::new(yaw.sin(), 0.0, yaw.cos());
            // The world as it is now (walls do not move), the players as the shooter saw
            // them.
            let wall = self
                .physics
                .raycast(
                    &Ray {
                        origin: eye,
                        direction: dir * 50.0,
                    },
                    QueryFilter::default(),
                )
                .map_or(50.0, |h| h.fraction * 50.0);
            // net: lag-compensated hit test against the shooter's view.
            if let Some(hit) = server
                .lag()
                .raycast(eye, dir, wall, view_time, Some(shooter))
                && let Some(p) = world.get_mut::<Player>(hit.entity)
            {
                p.health = p.health.saturating_sub(DAMAGE);
                let left = u32::from(p.health);
                // net: an unreliable event: effects only, losing one is harmless.
                server.send_event(
                    Target::All,
                    false,
                    &Hit {
                        point: hit.point,
                        health_left: left,
                    },
                );
            }
        }
        let _ = self.physics.step(dt, 1);
    }

    /// Moves the door body to the replicated `Door` (server after the rules; a client
    /// every frame, from the interpolated value).
    pub fn place_door(&mut self, world: &mut World) {
        let mut open = None;
        self.doors.for_each(world, |(_, d)| open = Some(d.open));
        if let Some(open) = open {
            let at = DOOR_CLOSED + Vec3::Y * open * 2.6;
            self.physics
                .set_transform(self.door_body, at, Quat::IDENTITY);
        }
    }

    /// Client, every frame: the other players' characters follow their interpolated
    /// positions (the local one is predicted by `simulate`), the door its state.
    pub fn client_present(&mut self, world: &mut World) {
        let Self {
            physics, remote, ..
        } = self;
        remote.for_each(world, |(p, link)| {
            physics.set_character_transform(
                CharacterId(link.id),
                p.pos,
                Quat::from_rotation_y(p.yaw),
            );
        });
        self.place_door(world);
        self.transforms.for_each(world, |(p, t)| {
            t.translation = p.pos;
            t.rotation = Quat::from_rotation_y(p.yaw);
        });
    }
}

/// Scripted play: walk toward `target` (flat), looking along `yaw`.
pub fn walk_to(from: Vec3, target: Vec3) -> (f32, f32) {
    let d = Vec3::new(target.x - from.x, 0.0, target.z - from.z);
    if d.length() < 0.15 {
        (0.0, 0.0)
    } else {
        let d = d.normalize();
        (d.x, d.z)
    }
}

/// The yaw that looks from `from` toward `to`.
pub fn aim(from: Vec3, to: Vec3) -> f32 {
    let d = to - from;
    d.x.atan2(d.z)
}

// ---------------------------------------------------------------------------------------
// The two roles, over any transport (UDP in the example, the in-memory network in the
// test).
// ---------------------------------------------------------------------------------------

use engine_net::{Client, ClientEvent, ClientId, ClientSettings, ServerEvent, ServerSettings};
use std::time::Instant;

pub const HOST_START: Vec3 = Vec3::new(-1.5, 0.0, -7.0);
pub const GUEST_START: Vec3 = Vec3::new(2.0, 0.0, -7.0);

/// The listen server: the host plays in the server's world.
pub struct Host<T: Transport> {
    pub world: World,
    pub game: Game,
    pub server: Server<PlayerInput, T>,
    pub me: Entity,
    pub door: Entity,
    pub guests: Vec<(ClientId, Entity)>,
    pub log: Vec<String>,
}

impl<T: Transport> Host<T> {
    pub fn new(transport: T) -> Self {
        let mut world = World::new();
        let mut game = Game::new(&mut world);
        // net: the server over the transport.
        let server = Server::new(transport, schema(), ServerSettings::default(), &mut world);
        let door = Game::spawn_door(&mut world);
        let me = game.spawn_player(&mut world, HOST_START);
        Self {
            world,
            game,
            server,
            me,
            door,
            guests: Vec::new(),
            log: Vec::new(),
        }
    }

    /// One tick with the host's own input.
    pub fn tick(&mut self, now: Instant, input: PlayerInput) {
        // net: receive (inputs of every guest land in their pawns).
        self.server.update(&mut self.world, now);
        while let Some(event) = self.server.poll_event() {
            match event {
                ServerEvent::ClientConnected(id) => {
                    let pawn = self.game.spawn_player(&mut self.world, GUEST_START);
                    // net: this guest predicts and controls it.
                    self.server.set_owner(&mut self.world, pawn, Some(id));
                    self.guests.push((id, pawn));
                    self.log.push(format!("guest {} joined", id.0));
                }
                ServerEvent::ClientDisconnected(id, why) => {
                    if let Some(i) = self.guests.iter().position(|g| g.0 == id) {
                        let (_, pawn) = self.guests.remove(i);
                        if let Some(link) = self.world.get::<CharacterLink>(pawn).copied() {
                            self.game.detach_character(link);
                        }
                        self.world.despawn(pawn);
                    }
                    self.log.push(format!("guest {} left: {why:?}", id.0));
                }
            }
        }
        // net: the host's input goes straight into its pawn.
        self.server.set_local_input(&mut self.world, self.me, input);
        let dt = self.server.dt();
        self.game.simulate(&mut self.world, dt);
        self.game
            .server_rules(&mut self.world, &mut self.server, dt);
        // net: snapshots out.
        self.server.send(&mut self.world, now);
    }

    pub fn player(&self, e: Entity) -> Player {
        self.world.get::<Player>(e).copied().unwrap_or_default()
    }
}

/// A guest: predicts its own player, sees the rest interpolated.
pub struct Guest<T: Transport> {
    pub world: World,
    pub game: Game,
    pub client: Client<PlayerInput, T>,
    pub me: Option<Entity>,
    pub host: Option<Entity>,
    pub door: Option<Entity>,
    pub doors_opened: u32,
    pub hits_seen: u32,
    pub log: Vec<String>,
}

impl<T: Transport> Guest<T> {
    pub fn new(transport: T) -> Self {
        let mut world = World::new();
        let game = Game::new(&mut world);
        // net: the client over the transport.
        let client = Client::new(transport, schema(), ClientSettings::default(), &mut world);
        Self {
            world,
            game,
            client,
            me: None,
            host: None,
            door: None,
            doors_opened: 0,
            hits_seen: 0,
            log: Vec::new(),
        }
    }

    /// One rendered frame with the input sampled for it.
    pub fn frame(&mut self, now: Instant, input: PlayerInput) -> engine_net::FrameInfo {
        let Self {
            world,
            game,
            client,
            ..
        } = self;
        // net: receive, correct, predict (with the same systems), interpolate, send.
        let info = client.frame(world, now, input, |w, dt| game.simulate(w, dt));
        while let Some(event) = self.client.poll_event() {
            match event {
                ClientEvent::Spawned {
                    entity, predicted, ..
                } => {
                    if let Some(p) = self.world.get::<Player>(entity).copied() {
                        self.game.attach_character(&mut self.world, entity, p.pos);
                        if predicted {
                            self.me = Some(entity);
                        } else {
                            self.host = Some(entity);
                        }
                    } else if self.world.has::<Door>(entity) {
                        self.door = Some(entity);
                    }
                }
                ClientEvent::Despawned { entity, .. } => {
                    if let Some(link) = self.world.get::<CharacterLink>(entity).copied() {
                        self.game.detach_character(link);
                    }
                }
                ClientEvent::Connected => self.log.push(String::from("connected")),
                ClientEvent::Disconnected(why) => self.log.push(format!("disconnected: {why:?}")),
                ClientEvent::SchemaMismatch => self.log.push(String::from("another build")),
            }
        }
        // net: one-shot events.
        let (log, opened, hits) = (&mut self.log, &mut self.doors_opened, &mut self.hits_seen);
        self.client.read_events::<DoorOpened>(|tick, _| {
            *opened += 1;
            log.push(format!("the door opens (tick {tick})"));
        });
        self.client.read_events::<Hit>(|_, h| {
            *hits += 1;
            log.push(format!(
                "hit at {:.2?}, {} health left",
                h.point, h.health_left
            ));
        });
        self.game.client_present(&mut self.world);
        info
    }

    pub fn player(&self, e: Option<Entity>) -> Option<Player> {
        self.world.get::<Player>(e?).copied()
    }

    pub fn door_open(&self) -> f32 {
        self.door
            .and_then(|d| self.world.get::<Door>(d))
            .map_or(0.0, |d| d.open)
    }
}

/// The scripted host: waits 3 s, walks onto the button, then strafes on it.
pub fn host_script(t: f64, me: Player) -> PlayerInput {
    let mut input = PlayerInput::default();
    if t < 3.0 {
        return input;
    }
    let target = if t < 5.0 {
        BUTTON
    } else if (((t - 5.0) / 0.6) as u32).is_multiple_of(2) {
        BUTTON + Vec3::new(0.7, 0.0, 0.0)
    } else {
        BUTTON - Vec3::new(0.7, 0.0, 0.0)
    };
    let (x, z) = walk_to(me.pos, target);
    input.move_x = x;
    input.move_z = z;
    input
}

/// The scripted guest: walks through the doorway (pushing against the door until it
/// opens), comes back into the doorway, then shoots at the host as it sees it.
pub fn guest_script(t: f64, phase: &mut u32, me: Player, host: Option<Player>) -> PlayerInput {
    let mut input = PlayerInput::default();
    let target = match *phase {
        0 => Vec3::new(0.0, 0.0, 4.0),
        _ => Vec3::new(-1.0, 0.0, 1.0),
    };
    let (x, z) = walk_to(me.pos, target);
    if *phase == 0 && me.pos.z > 3.8 {
        *phase = 1;
    }
    input.move_x = x;
    input.move_z = z;
    if *phase == 1
        && x == 0.0
        && z == 0.0
        && let Some(h) = host
    {
        input.yaw = aim(me.pos + Vec3::Y * EYE, h.pos + Vec3::Y * 1.0);
        if ((t * 10.0) as u32).is_multiple_of(3) {
            input.fire = SubTick::END.0; // pulled as this frame's input was sampled
        }
    }
    input
}
