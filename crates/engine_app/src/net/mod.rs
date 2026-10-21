//! Networking in the player (ADR 0106): the player runs the network session, the game talks
//! to it through the world's [`Network`] resource.
//!
//! - **The game says what travels** once: `PlayerSettings::network =
//!   Some(NetConfig::new::<Input>(schema))`, `schema` building the same [`NetSchema`] on
//!   every machine (ADR 0074).
//! - **Its tick is shared:** `Game::tick` runs on the host for everyone and on a joined
//!   player for its predictions, reading `NetInput<Input>` and the replicated components.
//! - **`Game::net_input`** writes the input before every host tick and joined frame
//!   ([`Network::set_input`]).
//! - **The player:**
//!   - hosting, runs the server around the game's ticks: its guests' inputs in, the host's
//!     input into its pawn, snapshots out;
//!   - joined, runs the client in place of its own ticks: corrections, predictions,
//!     remote entities interpolated ([`engine_runtime::Runtime::frame_driven`]);
//!   - offline, does nothing at all (ADR 0019: no loopback).
//! - **The game reaches it all through [`Network`]:** host, invite, join, leave; its pawn and
//!   the pawns it gives; events; the schema's one-shot events.

mod invite;

use engine_ecs::{Entity, QueryState, World, bytemuck};
use engine_net::{
    Client, ClientEvent, ClientSettings, InputData, NetSchema, Replicated, Server, ServerEvent,
    ServerSettings, Target,
};
use engine_net_transport::nat::{MappingStatus, NatConfig, PortMapper};
use engine_net_transport::{
    ConnectToken, Endpoint, Key, ServerConfig, TokenParams, USER_DATA_BYTES, UdpSocket, unix_now,
};
use engine_runtime::Driven;
use std::any::TypeId;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;

pub use engine_net::{ClientId as PlayerId, HitCapsule, NetInput, Predicted, SubTick};

/// Events kept for a game that does not read them.
const EVENTS_KEPT: usize = 256;

/// The game's network types (`PlayerSettings::network`).
#[derive(Clone)]
pub struct NetConfig {
    make: Arc<dyn Fn() -> Box<dyn NetDrive> + Send + Sync>,
    protocol_id: u64,
    input: TypeId,
    /// The server's settings when hosting (its tick rate is the player's `tick_hz`).
    pub server: ServerSettings,
    /// The client's settings when joined.
    pub client: ClientSettings,
}

impl core::fmt::Debug for NetConfig {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("NetConfig")
            .field("protocol_id", &self.protocol_id)
            .field("server", &self.server)
            .field("client", &self.client)
            .finish_non_exhaustive()
    }
}

impl NetConfig {
    /// A game whose input is `I` (plain data) and whose schema `schema` builds: the same
    /// function on every machine.
    pub fn new<I: InputData>(schema: fn() -> NetSchema) -> Self {
        Self {
            make: Arc::new(move || Box::new(Driver::<I>::new(schema)) as Box<dyn NetDrive>),
            protocol_id: schema().protocol_id(),
            input: TypeId::of::<I>(),
            server: ServerSettings::default(),
            client: ClientSettings::default(),
        }
    }

    pub(crate) fn driver(&self, tick_hz: f64) -> Box<dyn NetDrive> {
        let mut driver = (self.make)();
        driver.configure(
            ServerSettings {
                tick_rate: tick_hz,
                ..self.server
            },
            ClientSettings {
                tick_rate: tick_hz,
                ..self.client
            },
        );
        driver
    }

    pub(crate) fn network(&self) -> Network {
        Network {
            protocol_id: self.protocol_id,
            input_type: Some(self.input),
            ..Network::default()
        }
    }
}

/// How to host ([`Network::host`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostOptions {
    /// The UDP port: 0 for any free one.
    pub port: u16,
    /// Players besides the host, at most.
    pub max_players: u16,
    /// Ask the router to forward the port (UPnP, PCP, NAT-PMP), in the background; the
    /// mapping is removed when the session ends.
    pub map_port: bool,
    /// Seconds an invite may be used to start joining.
    pub invite_seconds: u64,
}

impl Default for HostOptions {
    fn default() -> Self {
        Self {
            port: 0,
            max_players: 7,
            map_port: true,
            invite_seconds: 600,
        }
    }
}

/// Who an event goes to ([`Network::send_event`]). A joined player's events go to the
/// host whatever this says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum To {
    /// Every player.
    All,
    /// One player.
    Player(PlayerId),
    /// Every player but one (the one whose action it was, who predicted it).
    AllBut(PlayerId),
}

/// What the session is doing ([`Network::state`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum NetState {
    /// No session: singleplayer.
    #[default]
    Offline,
    /// Hosting, with this many players joined.
    Hosting {
        /// Players joined.
        players: u32,
    },
    /// Connecting to a host.
    Joining,
    /// Playing on a host.
    Joined,
}

/// Something that happened to the session ([`Network::events`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetEvent {
    /// Hosting started; invites list these addresses.
    Hosting,
    /// Hosting could not start.
    HostFailed(String),
    /// The router forwards the port: invites made from now on reach the host from the
    /// internet too (at this address).
    PortMapped(SocketAddr),
    /// Hosting: a player joined. Give them a pawn ([`Network::give`]).
    PlayerJoined(PlayerId),
    /// Hosting: a player left (their pawns are no longer theirs; despawn them if you like).
    PlayerLeft(PlayerId, String),
    /// Joined: the host accepted; the replicated world arrives.
    Connected,
    /// Joining failed, or the connection ended.
    Disconnected(String),
    /// Joined: the host runs another build of the game.
    WrongBuild,
    /// Joined: a replicated entity arrived. Attach what is not replicated (a mesh, a
    /// sound); `predicted` marks the pawn this player controls.
    Spawned {
        /// The local entity.
        entity: Entity,
        /// This player predicts it.
        predicted: bool,
    },
    /// Joined: a replicated entity is gone (despawned at the start of the next frame).
    Despawned {
        /// The local entity.
        entity: Entity,
    },
}

/// What the game asked of the session, done at the next frame's start.
#[derive(Debug, Clone, PartialEq)]
enum Request {
    Host(HostOptions),
    Join(String),
    Leave,
}

/// The world's resource for networking (ADR 0106; see the module). Everything asked takes
/// effect at the next frame (requests) or the next tick (input, pawns, events).
#[derive(Debug, Default)]
pub struct Network {
    protocol_id: u64,
    input_type: Option<TypeId>,
    request: Option<Request>,
    input: Vec<u8>,
    local_pawn: Option<Entity>,
    gives: Vec<(Entity, Option<PlayerId>)>,
    /// Outgoing events: `(type, to, reliable, start, len)` into `out_bytes`.
    outgoing: Vec<(TypeId, To, bool, u32, u32)>,
    out_bytes: Vec<u8>,
    /// Incoming events of the frame: `(type, from, tick, start, len)` into `in_bytes`.
    incoming: Vec<(TypeId, Option<PlayerId>, u32, u32, u32)>,
    in_bytes: Vec<u8>,
    events: Vec<NetEvent>,
    state: NetState,
    addresses: Vec<SocketAddr>,
    /// Hosting: what invites are made with.
    key: Option<Key>,
    invite_seconds: u64,
    next_client: u64,
    alpha: f32,
    tick: u32,
}

impl Network {
    /// Hosts a game at the next frame: this machine runs the server and plays in it
    /// (a listen server). [`NetEvent::Hosting`] or [`NetEvent::HostFailed`] follows.
    pub fn host(&mut self, options: HostOptions) {
        self.request = Some(Request::Host(options));
    }

    /// Joins the game of `invite` ([`Self::invite`] on the host) at the next frame.
    pub fn join(&mut self, invite: &str) {
        self.request = Some(Request::Join(invite.to_owned()));
    }

    /// Ends the session at the next frame. A joined player's replicated entities are
    /// despawned; a host's world stays as it is.
    pub fn leave(&mut self) {
        self.request = Some(Request::Leave);
    }

    /// Hosting: an invite for one player (text: paste it, or send it through a platform's
    /// invite). Each call makes a new one. `None` when not hosting.
    pub fn invite(&mut self) -> Option<String> {
        let key = self.key.as_ref()?;
        self.next_client = self.next_client.wrapping_add(1);
        let token = ConnectToken::generate(
            &TokenParams {
                protocol_id: self.protocol_id,
                client_id: self.next_client,
                expire_seconds: self.invite_seconds,
                timeout_seconds: 10,
                server_addresses: &self.addresses,
                user_data: [0; USER_DATA_BYTES],
            },
            key,
            unix_now(),
        )
        .ok()?;
        Some(invite::encode(&token))
    }

    /// The input for the next tick (the game's input type, `NetConfig::new::<I>`). Call it
    /// in `Game::net_input`. Another type is ignored.
    pub fn set_input<I: InputData>(&mut self, input: &I) {
        if self.input_type == Some(TypeId::of::<I>()) {
            self.input.clear();
            self.input.extend_from_slice(bytemuck::bytes_of(input));
        }
    }

    /// Hosting: the host's own pawn, fed its input every tick.
    pub fn set_local_pawn(&mut self, pawn: Option<Entity>) {
        self.local_pawn = pawn;
    }

    /// Hosting: `pawn` (a `Replicated` entity) is `player`'s: they control and predict it
    /// (`None`: the server's again). Done after the current tick.
    pub fn give(&mut self, pawn: Entity, player: Option<PlayerId>) {
        self.gives.push((pawn, player));
    }

    /// Sends one of the schema's one-shot events after the current tick (`reliable`:
    /// arrives once, in order; otherwise at most once). Never for state: that is
    /// replicated.
    pub fn send_event<E: bytemuck::Pod>(&mut self, event: &E, to: To, reliable: bool) {
        let start = self.out_bytes.len() as u32;
        self.out_bytes.extend_from_slice(bytemuck::bytes_of(event));
        self.outgoing.push((
            TypeId::of::<E>(),
            to,
            reliable,
            start,
            size_of::<E>() as u32,
        ));
    }

    /// Calls `f(tick, from, event)` for every event of type `E` received this frame
    /// (`from`: the player who sent it, `None` from the host).
    pub fn read_events<E: bytemuck::Pod>(&self, mut f: impl FnMut(u32, Option<PlayerId>, E)) {
        for &(kind, from, tick, start, len) in &self.incoming {
            if kind == TypeId::of::<E>() && len as usize == size_of::<E>() {
                let bytes = &self.in_bytes[start as usize..(start + len) as usize];
                f(tick, from, bytemuck::pod_read_unaligned(bytes));
            }
        }
    }

    /// What happened since the last call (the session's news: joins, leaves, spawns).
    pub fn events(&mut self) -> impl Iterator<Item = NetEvent> + '_ {
        self.events.drain(..)
    }

    /// What the session is doing.
    pub fn state(&self) -> &NetState {
        &self.state
    }

    /// Hosting: the addresses invites list (the LAN's, the router's once mapped).
    pub fn addresses(&self) -> &[SocketAddr] {
        &self.addresses
    }

    /// Joined: how far the prediction is into its next tick, 0..1: where to show the
    /// player's own pawn between its last two ticks (`Game::present`).
    pub fn alpha(&self) -> f32 {
        self.alpha
    }

    /// The newest simulated tick (the host's, or a joined player's prediction).
    pub fn tick(&self) -> u32 {
        self.tick
    }

    fn push_event(&mut self, event: NetEvent) {
        if self.events.len() == EVENTS_KEPT {
            self.events.remove(0);
        }
        self.events.push(event);
    }

    fn receive(&mut self, kind: TypeId, from: Option<PlayerId>, tick: u32, bytes: &[u8]) {
        let start = self.in_bytes.len() as u32;
        self.in_bytes.extend_from_slice(bytes);
        self.incoming
            .push((kind, from, tick, start, bytes.len() as u32));
    }
}

/// The session, erased to the game's input type: what the player drives.
pub(crate) trait NetDrive: Send {
    fn configure(&mut self, server: ServerSettings, client: ClientSettings);
    /// The frame's start: the game's requests, the port mapping's news.
    fn begin_frame(&mut self, world: &mut World, now: Instant);
    /// Ends the session now, whatever it is (the editor's Stop).
    fn end(&mut self, world: &mut World);
    /// Joined: the client runs this frame's ticks.
    fn is_client(&self) -> bool;
    /// Hosting: before the game's tick (after `Game::net_input`).
    fn host_tick_start(&mut self, world: &mut World, now: Instant);
    /// Hosting: after the game's tick.
    fn host_tick_end(&mut self, world: &mut World, now: Instant);
    /// Joined: the frame, the game's tick as `step`.
    fn client_frame(
        &mut self,
        world: &mut World,
        now: Instant,
        step: &mut dyn FnMut(&mut World, f32),
    ) -> Driven;
}

enum Session<I: InputData> {
    Offline,
    Hosting {
        server: Box<Server<I, Endpoint<UdpSocket>>>,
        mapper: Option<PortMapper>,
        mapped: bool,
    },
    Joined {
        client: Box<Client<I, Endpoint<UdpSocket>>>,
        connected: bool,
    },
}

/// The session for a game whose input is `I`.
struct Driver<I: InputData> {
    schema: fn() -> NetSchema,
    server_settings: ServerSettings,
    client_settings: ClientSettings,
    session: Session<I>,
    replicated: Option<QueryState<(Entity, &'static Replicated)>>,
    gone: Vec<Entity>,
}

impl<I: InputData> Driver<I> {
    fn new(schema: fn() -> NetSchema) -> Self {
        Self {
            schema,
            server_settings: ServerSettings::default(),
            client_settings: ClientSettings::default(),
            session: Session::Offline,
            replicated: None,
            gone: Vec::new(),
        }
    }

    fn input(world: &World) -> I {
        world
            .resource::<Network>()
            .and_then(|n| bytemuck::try_pod_read_unaligned(&n.input).ok())
            .unwrap_or_default()
    }

    fn host(
        &mut self,
        world: &mut World,
        options: HostOptions,
        now: Instant,
    ) -> Result<(), String> {
        let socket = UdpSocket::bind(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            options.port,
        ))
        .map_err(|e| format!("a UDP port: {e}"))?;
        let port = socket
            .as_std()
            .local_addr()
            .map_err(|e| e.to_string())?
            .port();
        let addresses = local_addresses(port);
        let key = Key::random();
        let schema = (self.schema)();
        let endpoint = Endpoint::server(
            socket,
            schema.endpoint_config(),
            ServerConfig {
                private_key: key.clone(),
                max_clients: usize::from(options.max_players.max(1)),
                public_addresses: addresses.clone(),
            },
            now,
        )
        .map_err(|e| format!("the server: {e:?}"))?;
        let server = Server::new(endpoint, schema, self.server_settings, world);
        let mapper = options
            .map_port
            .then(|| PortMapper::start(NatConfig::default(), port));
        self.session = Session::Hosting {
            server: Box::new(server),
            mapper,
            mapped: false,
        };
        if let Some(net) = world.resource_mut::<Network>() {
            net.addresses = addresses;
            net.key = Some(key);
            net.invite_seconds = options.invite_seconds;
            net.next_client = engine_net_transport::crypto::random_u64() >> 16;
            net.state = NetState::Hosting { players: 0 };
        }
        Ok(())
    }

    fn join(&mut self, world: &mut World, invite: &str, now: Instant) -> Result<(), String> {
        let token = invite::decode(invite)?;
        let socket = UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0))
            .map_err(|e| format!("a UDP port: {e}"))?;
        let schema = (self.schema)();
        let endpoint = Endpoint::client(socket, schema.endpoint_config(), token, now)
            .map_err(|e| format!("the connection: {e:?}"))?;
        let client = Client::new(endpoint, schema, self.client_settings, world);
        self.session = Session::Joined {
            client: Box::new(client),
            connected: false,
        };
        if let Some(net) = world.resource_mut::<Network>() {
            net.state = NetState::Joining;
        }
        Ok(())
    }

    /// Ends the session; a joined player's replicated entities go with it.
    fn leave(&mut self, world: &mut World) {
        let joined = matches!(self.session, Session::Joined { .. });
        // Dropping the mapper removes the router's mapping.
        self.session = Session::Offline;
        if joined {
            let query = self
                .replicated
                .get_or_insert_with(|| QueryState::new(world));
            self.gone.clear();
            let gone = &mut self.gone;
            query.for_each_readonly(world, |(e, _)| gone.push(e));
            for &e in &self.gone {
                world.despawn(e);
            }
        }
        if let Some(net) = world.resource_mut::<Network>() {
            net.state = NetState::Offline;
            net.key = None;
            net.addresses.clear();
            net.alpha = 0.0;
        }
    }
}

impl<I: InputData> NetDrive for Driver<I> {
    fn configure(&mut self, server: ServerSettings, client: ClientSettings) {
        self.server_settings = server;
        self.client_settings = client;
    }

    fn end(&mut self, world: &mut World) {
        if !matches!(self.session, Session::Offline) {
            self.leave(world);
        }
    }

    fn begin_frame(&mut self, world: &mut World, now: Instant) {
        let Some(net) = world.resource_mut::<Network>() else {
            return;
        };
        net.incoming.clear();
        net.in_bytes.clear();
        let request = net.request.take();
        match request {
            Some(Request::Host(options)) => {
                self.leave(world);
                let event = match self.host(world, options, now) {
                    Ok(()) => NetEvent::Hosting,
                    Err(why) => NetEvent::HostFailed(why),
                };
                if let Some(net) = world.resource_mut::<Network>() {
                    net.push_event(event);
                }
            }
            Some(Request::Join(invite)) => {
                self.leave(world);
                if let Err(why) = self.join(world, &invite, now)
                    && let Some(net) = world.resource_mut::<Network>()
                {
                    net.push_event(NetEvent::Disconnected(why));
                }
            }
            Some(Request::Leave) => self.leave(world),
            None => {}
        }
        // The router's answer: its address goes into the invites made from now on.
        if let Session::Hosting {
            server,
            mapper: Some(mapper),
            mapped,
        } = &mut self.session
            && !*mapped
            && let MappingStatus::Mapped(mapping) = mapper.status()
        {
            *mapped = true;
            if let Some(net) = world.resource_mut::<Network>() {
                if !net.addresses.contains(&mapping.external) {
                    net.addresses.push(mapping.external);
                }
                server.transport_mut().set_public_addresses(&net.addresses);
                net.push_event(NetEvent::PortMapped(mapping.external));
            }
        }
    }

    fn is_client(&self) -> bool {
        matches!(self.session, Session::Joined { .. })
    }

    fn host_tick_start(&mut self, world: &mut World, now: Instant) {
        let Session::Hosting { server, .. } = &mut self.session else {
            return;
        };
        server.update(world, now);
        let input = Self::input(world);
        let Some(net) = world.resource_mut::<Network>() else {
            return;
        };
        while let Some(event) = server.poll_event() {
            let event = match event {
                ServerEvent::ClientConnected(id) => NetEvent::PlayerJoined(id),
                ServerEvent::ClientDisconnected(id, why) => {
                    NetEvent::PlayerLeft(id, format!("{why:?}"))
                }
            };
            net.push_event(event);
        }
        net.state = NetState::Hosting {
            players: server.clients().count() as u32,
        };
        server.read_event_bytes(|from, kind, tick, bytes| {
            net.receive(kind, Some(from), tick, bytes);
        });
        net.tick = server.tick();
        let pawn = net.local_pawn;
        if let Some(pawn) = pawn {
            server.set_local_input(world, pawn, input);
        }
    }

    fn host_tick_end(&mut self, world: &mut World, now: Instant) {
        let Session::Hosting { server, .. } = &mut self.session else {
            return;
        };
        let (gives, outgoing, bytes) = match world.resource_mut::<Network>() {
            Some(net) => (
                core::mem::take(&mut net.gives),
                core::mem::take(&mut net.outgoing),
                core::mem::take(&mut net.out_bytes),
            ),
            None => return,
        };
        for &(pawn, player) in &gives {
            server.set_owner(world, pawn, player);
        }
        for &(kind, to, reliable, start, len) in &outgoing {
            let target = match to {
                To::All => Target::All,
                To::Player(p) => Target::Client(p),
                To::AllBut(p) => Target::AllExcept(p),
            };
            server.send_event_bytes(
                target,
                reliable,
                kind,
                &bytes[start as usize..(start + len) as usize],
            );
        }
        server.send(world, now);
        // The buffers go back, emptied, so the next tick allocates nothing.
        if let Some(net) = world.resource_mut::<Network>() {
            restore(&mut net.gives, gives);
            restore(&mut net.outgoing, outgoing);
            restore(&mut net.out_bytes, bytes);
        }
    }

    fn client_frame(
        &mut self,
        world: &mut World,
        now: Instant,
        step: &mut dyn FnMut(&mut World, f32),
    ) -> Driven {
        let Session::Joined { client, connected } = &mut self.session else {
            return Driven::default();
        };
        // The frame's events go out with its inputs.
        if let Some(net) = world.resource_mut::<Network>() {
            let (outgoing, bytes) = (
                core::mem::take(&mut net.outgoing),
                core::mem::take(&mut net.out_bytes),
            );
            for &(kind, _, reliable, start, len) in &outgoing {
                client.send_event_bytes(
                    reliable,
                    kind,
                    &bytes[start as usize..(start + len) as usize],
                );
            }
            restore(&mut net.outgoing, outgoing);
            restore(&mut net.out_bytes, bytes);
        }
        let input = Self::input(world);
        let info = client.frame(world, now, input, |w, dt| step(w, dt));
        let mut ended = false;
        if let Some(net) = world.resource_mut::<Network>() {
            while let Some(event) = client.poll_event() {
                let event = match event {
                    ClientEvent::Connected => {
                        *connected = true;
                        net.state = NetState::Joined;
                        NetEvent::Connected
                    }
                    ClientEvent::Disconnected(why) => {
                        ended = true;
                        NetEvent::Disconnected(format!("{why:?}"))
                    }
                    ClientEvent::SchemaMismatch => {
                        ended = true;
                        NetEvent::WrongBuild
                    }
                    ClientEvent::Spawned {
                        entity, predicted, ..
                    } => NetEvent::Spawned { entity, predicted },
                    ClientEvent::Despawned { entity, .. } => NetEvent::Despawned { entity },
                };
                net.push_event(event);
            }
            client.read_event_bytes(|kind, tick, bytes| net.receive(kind, None, tick, bytes));
            net.alpha = client.alpha();
            net.tick = info.tick;
        }
        let driven = Driven {
            alpha: client.alpha(),
            tick: u64::from(info.tick),
        };
        if ended {
            self.leave(world);
        }
        driven
    }
}

/// Puts an emptied buffer back where it was taken from, keeping its capacity, unless the
/// game filled the place meanwhile.
fn restore<T>(slot: &mut Vec<T>, mut taken: Vec<T>) {
    taken.clear();
    if slot.is_empty() {
        *slot = taken;
    }
}

/// The addresses a host on this machine is reached at: the LAN's (the interface of the
/// default route, found without sending anything) and the loopback.
fn local_addresses(port: u16) -> Vec<SocketAddr> {
    let mut out = Vec::new();
    // Connecting a UDP socket sends nothing; it picks the interface a packet would leave by.
    if let Ok(probe) = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        && probe.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).is_ok()
        && let Ok(local) = probe.local_addr()
        && !local.ip().is_unspecified()
    {
        out.push(SocketAddr::new(local.ip(), port));
    }
    out.push(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port));
    out
}

#[cfg(test)]
mod tests;
