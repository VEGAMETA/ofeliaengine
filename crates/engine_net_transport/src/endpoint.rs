//! The endpoint: a client or a server on one datagram socket.
//!
//! Sans-I/O at heart: everything happens inside [`Endpoint::update`] (receive, timeouts,
//! loss detection) and [`Endpoint::flush`] (build and send packets), called by the game's
//! network step with the current time. No threads, no async runtime, no allocation after
//! a connection's slot exists (CLAUDE.md §5).

use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use crate::ack::{AckHeader, AckTracker, MAX_ACK_WORDS, Outcome};
use crate::bytes::{Reader, Writer};
use crate::channel::{
    ChannelConfig, ChannelKind, EntryList, MAX_CHANNELS, MessageId, Notice, RECORD_FRAG, ReliableRecv,
    ReliableSend, SendError, UnreliableRecv, UnreliableSend,
};
use crate::crypto::{Cipher, Key, TAG_BYTES, random_u64};
use crate::packet::{self, Kind, MAX_HEADER_BYTES, REQUEST_BYTES, ReplayWindow};
use crate::socket::DatagramSocket;
use crate::token::{
    CHALLENGE_TOKEN_BYTES, ConnectToken, PRIVATE_TOKEN_BYTES, PrivateToken, USER_DATA_BYTES, VERSION,
    open_challenge, seal_challenge,
};

/// Largest datagram the endpoint reads (bigger foreign datagrams are truncated).
const RECV_BUFFER_BYTES: usize = 2048;
/// Disconnect packets sent when a side leaves (one may be lost).
const DISCONNECT_REDUNDANCY: usize = 3;
/// Bytes of one queued foreign datagram.
const FOREIGN_BYTES: usize = 1500;

/// Endpoint settings. Client and server must agree on `protocol_id` and `channels`.
#[derive(Clone, Debug)]
pub struct EndpointConfig {
    /// The game and protocol revision (see [`crate::TokenParams::protocol_id`]).
    pub protocol_id: u64,
    /// The channels of every connection, at most 8.
    pub channels: Vec<ChannelConfig>,
    /// The largest datagram sent, bytes. 1200 fits every path that carries IPv6 (RFC 8200
    /// minimum MTU 1280 minus headers) and survives VPN and tunnel overhead.
    pub max_packet_bytes: usize,
    /// Send an empty packet when nothing else went out for this long.
    pub keepalive_interval: Duration,
    /// How long a peer may hold an acknowledgement (its flush interval); part of the
    /// loss timeout.
    pub max_ack_delay: Duration,
    /// Per-connection send budget, bytes per second (token bucket, 100 ms burst). `None`:
    /// unlimited. Unreliable messages over budget are dropped at flush (reported lost when
    /// notified); reliable ones wait.
    pub send_rate: Option<u32>,
    /// Packets per connection per flush, at most.
    pub max_packets_per_flush: usize,
    /// Client: how often to repeat a connection request or response.
    pub connect_retry: Duration,
    /// Client: how long to try one server address.
    pub connect_timeout: Duration,
    /// Datagrams of other protocols (STUN, hole punching) kept for
    /// [`Endpoint::poll_foreign`]. 0 drops them.
    pub foreign_capacity: usize,
}

impl EndpointConfig {
    /// Defaults with `protocol_id` and channels: 0 reliable-ordered, 1 unreliable,
    /// 2 unreliable-sequenced.
    pub fn new(protocol_id: u64) -> Self {
        Self {
            protocol_id,
            channels: vec![
                ChannelConfig::reliable(),
                ChannelConfig::unreliable(),
                ChannelConfig::unreliable_sequenced(),
            ],
            max_packet_bytes: 1200,
            keepalive_interval: Duration::from_millis(100),
            max_ack_delay: Duration::from_millis(50),
            send_rate: None,
            max_packets_per_flush: 64,
            connect_retry: Duration::from_millis(100),
            connect_timeout: Duration::from_secs(5),
            foreign_capacity: 32,
        }
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.channels.is_empty() || self.channels.len() > MAX_CHANNELS {
            return Err(ConfigError("1 to 8 channels"));
        }
        for c in &self.channels {
            c.validate().map_err(ConfigError)?;
        }
        if !(1100..=RECV_BUFFER_BYTES - 100).contains(&self.max_packet_bytes) {
            return Err(ConfigError("max_packet_bytes must be 1100..=1948"));
        }
        if self.max_packets_per_flush == 0 {
            return Err(ConfigError("max_packets_per_flush must be at least 1"));
        }
        Ok(())
    }
}

/// An invalid [`EndpointConfig`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid endpoint configuration: {0}")]
pub struct ConfigError(&'static str);

/// Server settings.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// The key connect tokens are sealed with.
    pub private_key: Key,
    /// Connections at most.
    pub max_clients: usize,
    /// The addresses clients reach this server at (the bound address, the address a
    /// port mapping or STUN found). A token is accepted only if it names one of them.
    /// Empty: any token sealed with `private_key` is accepted, safe only when the key is
    /// used by this server alone (a listen server issuing its own tokens).
    pub public_addresses: Vec<SocketAddr>,
}

/// Identifies a connection; stale ids (of a closed connection) are refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConnectionId {
    /// Slot index (0 on a client).
    pub index: u16,
    /// Reuse counter of the slot.
    pub generation: u16,
}

/// A channel index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChannelId(pub u8);

/// Why a connection ended or never started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisconnectReason {
    /// The peer disconnected.
    ByPeer,
    /// No packet for the token's timeout.
    TimedOut,
    /// Client: no server address answered in time.
    ConnectTimedOut,
    /// Client: the server refused (full).
    Denied,
    /// Client: the token expired before a server answered.
    TokenExpired,
    /// The peer broke the protocol (an oversized reliable message).
    ProtocolError,
}

/// Something that happened, from [`Endpoint::poll_event`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// A connection is up (server: a client joined; client: the server accepted).
    Connected(ConnectionId),
    /// A connection ended; its id is no longer valid.
    Disconnected(ConnectionId, DisconnectReason),
    /// A notified unreliable message arrived (every fragment acknowledged).
    Delivered(ConnectionId, ChannelId, MessageId),
    /// A notified unreliable message was lost (or dropped over budget).
    Lost(ConnectionId, ChannelId, MessageId),
}

/// A connection's estimates and counters.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ConnectionStats {
    /// Smoothed round trip (RFC 6298), without the peer's acknowledgement delay.
    pub rtt: Duration,
    /// Round-trip variation (RFC 6298 RTTVAR): the jitter estimate.
    pub rtt_variance: Duration,
    /// Smallest round trip seen.
    pub min_rtt: Duration,
    /// Fraction of packets lost, exponentially averaged (about the last 20 packets).
    pub packet_loss: f32,
    /// Packets sent.
    pub packets_sent: u64,
    /// Packets received and authenticated.
    pub packets_received: u64,
    /// Packets declared lost.
    pub packets_lost: u64,
    /// Bytes sent (UDP payload).
    pub bytes_sent: u64,
    /// Bytes received (UDP payload).
    pub bytes_received: u64,
    /// Send rate over the last second, bytes per second.
    pub send_bytes_per_second: f32,
    /// Receive rate over the last second, bytes per second.
    pub receive_bytes_per_second: f32,
    /// Bytes in packets not yet acknowledged or lost.
    pub bytes_in_flight: u64,
    /// Reliable bytes queued and not yet acknowledged, all channels.
    pub reliable_unacked: u64,
    /// Unreliable messages dropped: over budget or packet limits when sending, buffer
    /// full or stale when receiving.
    pub unreliable_dropped: u64,
}

/// Endpoint-wide counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EndpointStats {
    /// Datagrams dropped: malformed, unauthenticated, replayed, unknown sender.
    pub dropped: u64,
    /// Foreign datagrams dropped (queue full or disabled).
    pub foreign_dropped: u64,
    /// Packets dropped because a reliable receive window was full.
    pub backpressure: u64,
}

enum ChannelState {
    Reliable { send: ReliableSend, recv: ReliableRecv },
    Unreliable { send: UnreliableSend, recv: UnreliableRecv },
}

struct RateWindow {
    start: Instant,
    bytes: u64,
    rate: f32,
}

impl RateWindow {
    fn add(&mut self, now: Instant, bytes: usize) {
        self.bytes += bytes as u64;
        let elapsed = now.saturating_duration_since(self.start);
        if elapsed >= Duration::from_secs(1) {
            self.rate = self.bytes as f32 / elapsed.as_secs_f32();
            self.bytes = 0;
            self.start = now;
        }
    }
}

struct Connection {
    active: bool,
    generation: u16,
    addr: SocketAddr,
    client_id: u64,
    user_data: [u8; USER_DATA_BYTES],
    send_cipher: Option<Cipher>,
    recv_cipher: Option<Cipher>,
    send_seq: u64,
    replay: ReplayWindow,
    acks: AckTracker,
    channels: Box<[ChannelState]>,
    last_recv: Instant,
    last_send: Instant,
    timeout: Duration,
    tokens: f64,
    last_refill: Instant,
    packets_sent: u64,
    packets_received: u64,
    bytes_sent: u64,
    bytes_received: u64,
    send_rate: RateWindow,
    recv_rate: RateWindow,
    send_dropped: u64,
}

impl Connection {
    fn new(config: &EndpointConfig, now: Instant) -> Box<Self> {
        let channels = config
            .channels
            .iter()
            .map(|c| match c.kind {
                ChannelKind::ReliableOrdered => ChannelState::Reliable {
                    send: ReliableSend::new(c),
                    recv: ReliableRecv::new(c),
                },
                ChannelKind::Unreliable | ChannelKind::UnreliableSequenced => ChannelState::Unreliable {
                    send: UnreliableSend::new(c),
                    recv: UnreliableRecv::new(c),
                },
            })
            .collect();
        Box::new(Self {
            active: false,
            generation: 0,
            addr: SocketAddr::from(([0, 0, 0, 0], 0)),
            client_id: 0,
            user_data: [0; USER_DATA_BYTES],
            send_cipher: None,
            recv_cipher: None,
            send_seq: 0,
            replay: ReplayWindow::new(),
            acks: AckTracker::new(now, config.max_ack_delay),
            channels,
            last_recv: now,
            last_send: now,
            timeout: Duration::from_secs(10),
            tokens: 0.0,
            last_refill: now,
            packets_sent: 0,
            packets_received: 0,
            bytes_sent: 0,
            bytes_received: 0,
            send_rate: RateWindow { start: now, bytes: 0, rate: 0.0 },
            recv_rate: RateWindow { start: now, bytes: 0, rate: 0.0 },
            send_dropped: 0,
        })
    }

    /// Prepares the slot for a new peer. The send sequence starts at a random point
    /// below 2^62 so a key reused after a crash (a token presented again) never repeats
    /// a nonce (AES-GCM must never see one twice).
    #[allow(clippy::too_many_arguments)]
    fn open(
        &mut self,
        now: Instant,
        addr: SocketAddr,
        client_id: u64,
        user_data: [u8; USER_DATA_BYTES],
        send: Cipher,
        recv: Cipher,
        timeout: Duration,
        rate: Option<u32>,
    ) {
        self.active = true;
        self.generation = self.generation.wrapping_add(1);
        self.addr = addr;
        self.client_id = client_id;
        self.user_data = user_data;
        self.send_cipher = Some(send);
        self.recv_cipher = Some(recv);
        self.send_seq = random_u64() >> 2;
        self.replay.reset();
        self.acks.reset(now, self.send_seq);
        for ch in self.channels.iter_mut() {
            match ch {
                ChannelState::Reliable { send, recv } => {
                    send.reset();
                    recv.reset();
                }
                ChannelState::Unreliable { send, recv } => {
                    send.reset();
                    recv.reset();
                }
            }
        }
        self.last_recv = now;
        self.last_send = now;
        self.timeout = timeout;
        self.tokens = burst(rate);
        self.last_refill = now;
        self.packets_sent = 0;
        self.packets_received = 0;
        self.bytes_sent = 0;
        self.bytes_received = 0;
        self.send_rate = RateWindow { start: now, bytes: 0, rate: 0.0 };
        self.recv_rate = RateWindow { start: now, bytes: 0, rate: 0.0 };
        self.send_dropped = 0;
    }

    fn close(&mut self) {
        self.active = false;
        self.send_cipher = None;
        self.recv_cipher = None;
    }

    fn id(&self, index: usize) -> ConnectionId {
        ConnectionId {
            index: index as u16,
            generation: self.generation,
        }
    }

    fn has_pending(&self) -> bool {
        self.channels.iter().any(|c| match c {
            ChannelState::Reliable { send, .. } => send.has_pending(),
            ChannelState::Unreliable { send, .. } => send.has_pending(),
        })
    }

    fn stats(&self) -> ConnectionStats {
        let mut reliable_unacked = 0;
        let mut dropped = self.send_dropped;
        for c in self.channels.iter() {
            match c {
                ChannelState::Reliable { send, .. } => reliable_unacked += send.unacked(),
                ChannelState::Unreliable { recv, .. } => dropped += recv.dropped,
            }
        }
        ConnectionStats {
            rtt: self.acks.rtt(),
            rtt_variance: self.acks.rtt_variance(),
            min_rtt: self.acks.min_rtt(),
            packet_loss: self.acks.loss(),
            packets_sent: self.packets_sent,
            packets_received: self.packets_received,
            packets_lost: self.acks.packets_lost,
            bytes_sent: self.bytes_sent,
            bytes_received: self.bytes_received,
            send_bytes_per_second: self.send_rate.rate,
            receive_bytes_per_second: self.recv_rate.rate,
            bytes_in_flight: self.acks.bytes_in_flight(),
            reliable_unacked,
            unreliable_dropped: dropped,
        }
    }
}

fn burst(rate: Option<u32>) -> f64 {
    rate.map_or(f64::MAX, |r| f64::from(r) * 0.1)
}

/// Routes an ack or loss of a packet's entry to its channel.
fn apply_outcome(
    channels: &mut [ChannelState],
    outcome: Outcome,
    events: &mut VecDeque<Event>,
    conn: ConnectionId,
) {
    let (entry, acked) = match outcome {
        Outcome::Acked(e) => (e, true),
        Outcome::Lost(e) => (e, false),
    };
    let channel = ChannelId(entry.channel);
    match channels.get_mut(usize::from(entry.channel)) {
        Some(ChannelState::Reliable { send, .. }) => {
            if acked {
                send.on_acked(entry.id);
            } else {
                send.on_lost(entry.id);
            }
        }
        Some(ChannelState::Unreliable { send, .. }) => {
            let mut notice = |n: Notice| {
                events.push_back(match n {
                    Notice::Delivered(id) => Event::Delivered(conn, channel, id),
                    Notice::Lost(id) => Event::Lost(conn, channel, id),
                });
            };
            if acked {
                send.on_acked(entry.id, &mut notice);
            } else {
                send.on_lost(entry.id, &mut notice);
            }
        }
        None => {}
    }
}

/// Writes the acknowledgement header: flags 1, newest u16, delay u16, word count u8, words.
fn write_ack(w: &mut Writer<'_>, ack: &AckHeader) -> Option<()> {
    w.u8(1)?;
    w.u16(ack.largest)?;
    w.u16(ack.delay)?;
    w.u8(ack.count as u8)?;
    for word in &ack.words[..ack.count] {
        w.u64(*word)?;
    }
    Some(())
}

fn read_ack(r: &mut Reader<'_>) -> Option<AckHeader> {
    let largest = r.u16()?;
    let delay = r.u16()?;
    let count = usize::from(r.u8()?);
    if !(1..=MAX_ACK_WORDS).contains(&count) {
        return None;
    }
    let mut words = [0u64; MAX_ACK_WORDS];
    for word in &mut words[..count] {
        *word = r.u64()?;
    }
    Some(AckHeader { largest, delay, words, count })
}

/// Why a payload was not accepted.
enum PayloadError {
    Malformed,
    Backpressure,
}

/// Walks the records of a payload; with `apply` false only validates.
fn walk_records(
    channels: &mut [ChannelState],
    records: &[u8],
    apply: bool,
) -> Result<(), PayloadError> {
    let mut r = Reader::new(records);
    while !r.is_empty() {
        let header = r.u8().ok_or(PayloadError::Malformed)?;
        if header & 0xf0 != 0 {
            return Err(PayloadError::Malformed);
        }
        let ch = channels
            .get_mut(usize::from(header & 0x07))
            .ok_or(PayloadError::Malformed)?;
        let frag = header & RECORD_FRAG != 0;
        match ch {
            ChannelState::Reliable { recv, .. } => {
                if frag {
                    return Err(PayloadError::Malformed);
                }
                let offset = r.u32().ok_or(PayloadError::Malformed)?;
                let len = r.varint().ok_or(PayloadError::Malformed)? as usize;
                let data = r.bytes(len).ok_or(PayloadError::Malformed)?;
                if apply {
                    recv.accept(offset, data);
                } else if !recv.fits(offset, len) {
                    return Err(PayloadError::Backpressure);
                }
            }
            ChannelState::Unreliable { recv, .. } => {
                if frag {
                    let seq = r.u16().ok_or(PayloadError::Malformed)?;
                    let index = r.varint().ok_or(PayloadError::Malformed)? as usize;
                    let count = r.varint().ok_or(PayloadError::Malformed)? as usize;
                    let len = r.varint().ok_or(PayloadError::Malformed)? as usize;
                    let data = r.bytes(len).ok_or(PayloadError::Malformed)?;
                    if !recv.fragment_valid(index, count, len) {
                        return Err(PayloadError::Malformed);
                    }
                    if apply {
                        recv.fragment(seq, index, count, data);
                    }
                } else {
                    let sequenced = recv.is_sequenced();
                    let seq = if sequenced { r.u16().ok_or(PayloadError::Malformed)? } else { 0 };
                    let len = r.varint().ok_or(PayloadError::Malformed)? as usize;
                    let data = r.bytes(len).ok_or(PayloadError::Malformed)?;
                    if apply {
                        recv.whole(seq, data);
                    }
                }
            }
        }
    }
    Ok(())
}

struct Pending {
    addr: SocketAddr,
    client_id: u64,
    user_data: [u8; USER_DATA_BYTES],
    timeout: Duration,
    /// Client-to-server key: opens the response.
    recv: Cipher,
    /// Server-to-client key: seals the challenge.
    send: Cipher,
    expire: Instant,
}

struct TokenUse {
    mac: [u8; TAG_BYTES],
    addr: SocketAddr,
    expire_unix: u64,
}

struct ServerState {
    private: Cipher,
    challenge_key: Cipher,
    challenge_seq: u64,
    /// Sequence of packets sent before a connection exists (challenges, denials), in the
    /// upper half of the sequence space so they never share a nonce with connection
    /// packets under the same key.
    global_seq: u64,
    max_clients: usize,
    public_addresses: Vec<SocketAddr>,
    pending: Vec<Pending>,
    history: Vec<TokenUse>,
    history_next: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Requesting,
    Responding,
    Connected,
    Failed,
}

struct ClientState {
    token: ConnectToken,
    phase: Phase,
    address_index: usize,
    phase_start: Instant,
    last_send: Instant,
    challenge_seq: u64,
    challenge: [u8; CHALLENGE_TOKEN_BYTES],
}

enum Role {
    Server(Box<ServerState>),
    Client(Box<ClientState>),
}

/// Fixed queue of foreign datagrams.
struct Foreign {
    data: Box<[u8]>,
    meta: VecDeque<(usize, usize, SocketAddr)>,
    capacity: usize,
    next: usize,
}

/// A client or server. See the crate documentation for the game loop.
pub struct Endpoint<S: DatagramSocket> {
    socket: S,
    config: EndpointConfig,
    role: Role,
    slots: Vec<Box<Connection>>,
    active: usize,
    by_addr: HashMap<SocketAddr, u16>,
    events: VecDeque<Event>,
    buf: Box<[u8]>,
    out: Box<[u8]>,
    foreign: Foreign,
    base_instant: Instant,
    base_unix: u64,
    stats: EndpointStats,
}

impl<S: DatagramSocket> std::fmt::Debug for Endpoint<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint")
            .field("server", &matches!(self.role, Role::Server(_)))
            .field("connections", &self.active)
            .finish_non_exhaustive()
    }
}

impl<S: DatagramSocket> Endpoint<S> {
    fn with_role(socket: S, config: EndpointConfig, role: Role, now: Instant, slots: usize) -> Self {
        let foreign = Foreign {
            data: vec![0u8; config.foreign_capacity * FOREIGN_BYTES].into_boxed_slice(),
            meta: VecDeque::with_capacity(config.foreign_capacity),
            capacity: config.foreign_capacity,
            next: 0,
        };
        Self {
            socket,
            role,
            slots: Vec::with_capacity(slots),
            active: 0,
            by_addr: HashMap::with_capacity(slots * 2),
            events: VecDeque::with_capacity(4096),
            buf: vec![0u8; RECV_BUFFER_BYTES].into_boxed_slice(),
            out: vec![0u8; RECV_BUFFER_BYTES].into_boxed_slice(),
            foreign,
            base_instant: now,
            base_unix: crate::token::unix_now(),
            stats: EndpointStats::default(),
            config,
        }
    }

    /// A server accepting tokens sealed with `server.private_key`.
    pub fn server(socket: S, config: EndpointConfig, server: ServerConfig, now: Instant) -> Result<Self, ConfigError> {
        config.validate()?;
        if server.max_clients == 0 || server.max_clients > usize::from(u16::MAX) {
            return Err(ConfigError("max_clients must be 1..=65535"));
        }
        let max = server.max_clients;
        let state = ServerState {
            private: Cipher::new(&server.private_key),
            challenge_key: Cipher::new(&Key::random()),
            challenge_seq: 0,
            global_seq: (1 << 63) | (random_u64() >> 2),
            max_clients: max,
            public_addresses: server.public_addresses,
            pending: Vec::with_capacity(max * 4),
            history: Vec::with_capacity(max * 8),
            history_next: 0,
        };
        Ok(Self::with_role(socket, config, Role::Server(Box::new(state)), now, max))
    }

    /// A client that starts connecting with `token` on the first `update`.
    pub fn client(socket: S, config: EndpointConfig, token: ConnectToken, now: Instant) -> Result<Self, ConfigError> {
        config.validate()?;
        if token.protocol_id != config.protocol_id {
            return Err(ConfigError("the token's protocol_id differs from the endpoint's"));
        }
        let state = ClientState {
            phase: Phase::Requesting,
            address_index: 0,
            phase_start: now,
            last_send: now.checked_sub(Duration::from_secs(3600)).unwrap_or(now),
            challenge_seq: 0,
            challenge: [0; CHALLENGE_TOKEN_BYTES],
            token,
        };
        let mut endpoint = Self::with_role(socket, config, Role::Client(Box::new(state)), now, 1);
        let mut conn = Connection::new(&endpoint.config, now);
        if let Role::Client(c) = &endpoint.role {
            let timeout = Duration::from_secs(u64::from(c.token.timeout_seconds.max(1)));
            conn.open(
                now,
                c.token.server_addresses.as_slice()[0],
                0,
                [0; USER_DATA_BYTES],
                Cipher::new(&c.token.client_to_server_key),
                Cipher::new(&c.token.server_to_client_key),
                timeout,
                endpoint.config.send_rate,
            );
            // Not active until the server accepts.
            conn.active = false;
        }
        endpoint.slots.push(conn);
        Ok(endpoint)
    }

    /// The configuration.
    pub fn config(&self) -> &EndpointConfig {
        &self.config
    }

    /// The socket.
    pub fn socket(&self) -> &S {
        &self.socket
    }

    /// The local address.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Endpoint-wide counters.
    pub fn endpoint_stats(&self) -> EndpointStats {
        self.stats
    }

    /// Server: replaces the addresses tokens must name (after NAT traversal found the
    /// external ones).
    pub fn set_public_addresses(&mut self, addrs: &[SocketAddr]) {
        if let Role::Server(s) = &mut self.role {
            s.public_addresses.clear();
            s.public_addresses.extend_from_slice(addrs);
        }
    }

    /// Whether this is a client whose connection attempt failed or ended.
    pub fn is_client_failed(&self) -> bool {
        matches!(&self.role, Role::Client(c) if c.phase == Phase::Failed)
    }

    fn unix_now(&self, now: Instant) -> u64 {
        self.base_unix + now.saturating_duration_since(self.base_instant).as_secs()
    }

    fn conn(&self, id: ConnectionId) -> Option<&Connection> {
        self.slots
            .get(usize::from(id.index))
            .map(|c| &**c)
            .filter(|c| c.active && c.generation == id.generation)
    }

    fn conn_mut(&mut self, id: ConnectionId) -> Option<&mut Connection> {
        self.slots
            .get_mut(usize::from(id.index))
            .map(|c| &mut **c)
            .filter(|c| c.active && c.generation == id.generation)
    }

    /// The peer's address.
    pub fn peer_addr(&self, id: ConnectionId) -> Option<SocketAddr> {
        self.conn(id).map(|c| c.addr)
    }

    /// Server: the client id from the connect token.
    pub fn client_id(&self, id: ConnectionId) -> Option<u64> {
        self.conn(id).map(|c| c.client_id)
    }

    /// Server: the user data from the connect token.
    pub fn user_data(&self, id: ConnectionId) -> Option<&[u8; USER_DATA_BYTES]> {
        self.conn(id).map(|c| &c.user_data)
    }

    /// Sends a datagram of another protocol (STUN, hole punching) on this socket.
    pub fn send_raw(&mut self, now: Instant, to: SocketAddr, data: &[u8]) -> io::Result<()> {
        self.socket.send_to(now, to, data)
    }

    /// Takes the oldest foreign datagram received: copies it into `out`, returns its
    /// length and sender.
    pub fn poll_foreign(&mut self, out: &mut [u8]) -> Option<(usize, SocketAddr)> {
        let (at, len, from) = self.foreign.meta.pop_front()?;
        let n = len.min(out.len());
        out[..n].copy_from_slice(&self.foreign.data[at..at + n]);
        Some((n, from))
    }

    fn push_foreign(&mut self, len: usize, from: SocketAddr) {
        if self.foreign.capacity == 0 || self.foreign.meta.len() == self.foreign.capacity {
            self.stats.foreign_dropped += 1;
            return;
        }
        let len = len.min(FOREIGN_BYTES);
        let at = self.foreign.next * FOREIGN_BYTES;
        self.foreign.next = (self.foreign.next + 1) % self.foreign.capacity;
        self.foreign.data[at..at + len].copy_from_slice(&self.buf[..len]);
        self.foreign.meta.push_back((at, len, from));
    }

    // -----------------------------------------------------------------------------------
    // Update: receive, timeouts, loss detection.
    // -----------------------------------------------------------------------------------

    /// Receives everything waiting, then handles timeouts and losses. Call once per
    /// network step before reading messages.
    pub fn update(&mut self, now: Instant) {
        engine_profiler::zone!("net::update");
        loop {
            let received = match self.socket.recv_from(now, &mut self.buf) {
                Ok(Some(r)) => r,
                Ok(None) => break,
                Err(_) => {
                    self.stats.dropped += 1;
                    break;
                }
            };
            let (len, from) = received;
            if len == 0 || !packet::is_transport_packet(self.buf[0]) {
                if len > 0 {
                    self.push_foreign(len, from);
                }
                continue;
            }
            self.receive_packet(now, len, from);
        }
        self.tick(now);
    }

    fn receive_packet(&mut self, now: Instant, len: usize, from: SocketAddr) {
        let Some((kind, seq, header)) = packet::parse_header(&self.buf[..len]) else {
            self.stats.dropped += 1;
            return;
        };
        if matches!(self.role, Role::Server(_)) {
            self.server_packet(now, kind, seq, header, len, from);
        } else {
            self.client_packet(now, kind, seq, header, len, from);
        }
    }

    /// A packet of a live connection (payload or disconnect).
    fn connection_packet(&mut self, now: Instant, index: usize, kind: Kind, seq: u64, header: usize, len: usize) {
        if self.connection_packet_inner(now, index, kind, seq, header, len) {
            self.close(index, Some(DisconnectReason::ByPeer));
        }
    }

    /// Returns whether the peer disconnected.
    fn connection_packet_inner(
        &mut self,
        now: Instant,
        index: usize,
        kind: Kind,
        seq: u64,
        header: usize,
        len: usize,
    ) -> bool {
        let Self { slots, buf, events, config, stats, .. } = self;
        let conn = &mut *slots[index];
        if conn.replay.is_replay(seq) {
            stats.dropped += 1;
            return false;
        }
        let Some(cipher) = &conn.recv_cipher else { return false };
        let Some(range) = packet::open(&mut buf[..len], seq, header, config.protocol_id, cipher) else {
            stats.dropped += 1;
            return false;
        };
        let id = conn.id(index);
        match kind {
            Kind::Payload => {
                let payload = &buf[range];
                let mut r = Reader::new(payload);
                let Some(flags) = r.u8() else {
                    stats.dropped += 1;
                    return false;
                };
                let ack = if flags & 1 != 0 {
                    match read_ack(&mut r) {
                        Some(a) => Some(a),
                        None => {
                            stats.dropped += 1;
                            return false;
                        }
                    }
                } else {
                    None
                };
                let records = &payload[r.pos()..];
                match walk_records(&mut conn.channels, records, false) {
                    Ok(()) => {}
                    Err(PayloadError::Malformed) => {
                        stats.dropped += 1;
                        return false;
                    }
                    Err(PayloadError::Backpressure) => {
                        stats.backpressure += 1;
                        return false;
                    }
                }
                conn.replay.record(seq);
                if let Some(ack) = ack {
                    let Connection { acks, channels, .. } = conn;
                    acks.on_ack(&ack, now, &mut |o| apply_outcome(channels, o, events, id));
                }
                // Validated above; applying cannot fail.
                let _ = walk_records(&mut conn.channels, records, true);
                conn.acks.on_received(seq, now);
                conn.last_recv = now;
                conn.packets_received += 1;
                conn.bytes_received += len as u64;
                conn.recv_rate.add(now, len);
                false
            }
            Kind::Disconnect => {
                conn.replay.record(seq);
                true
            }
            _ => {
                stats.dropped += 1;
                false
            }
        }
    }

    fn close(&mut self, index: usize, reason: Option<DisconnectReason>) {
        let conn = &mut *self.slots[index];
        if !conn.active {
            return;
        }
        let id = conn.id(index);
        let addr = conn.addr;
        conn.close();
        self.active -= 1;
        if self.by_addr.get(&addr) == Some(&(index as u16)) {
            self.by_addr.remove(&addr);
        }
        if let Role::Client(c) = &mut self.role {
            c.phase = Phase::Failed;
        }
        if let Some(reason) = reason {
            self.events.push_back(Event::Disconnected(id, reason));
        }
    }

    fn tick(&mut self, now: Instant) {
        // Client connection state machine.
        if let Role::Client(_) = self.role {
            self.client_tick(now);
        }
        for index in 0..self.slots.len() {
            let Self { slots, events, .. } = self;
            let conn = &mut *slots[index];
            if !conn.active {
                continue;
            }
            if now.saturating_duration_since(conn.last_recv) > conn.timeout {
                self.close(index, Some(DisconnectReason::TimedOut));
                continue;
            }
            let id = conn.id(index);
            let Connection { acks, channels, .. } = conn;
            acks.detect_lost(now, &mut |o| apply_outcome(channels, o, events, id));
            let broken = conn.channels.iter().any(|c| matches!(c, ChannelState::Reliable { recv, .. } if recv.broken));
            if broken {
                self.send_disconnect(now, index);
                self.close(index, Some(DisconnectReason::ProtocolError));
            }
        }
        if let Role::Server(s) = &mut self.role {
            s.pending.retain(|p| p.expire > now);
        }
    }

    // -----------------------------------------------------------------------------------
    // Server handshake.
    // -----------------------------------------------------------------------------------

    fn server_packet(&mut self, now: Instant, kind: Kind, seq: u64, header: usize, len: usize, from: SocketAddr) {
        if let Some(&index) = self.by_addr.get(&from) {
            match kind {
                Kind::Payload | Kind::Disconnect => {
                    self.connection_packet(now, usize::from(index), kind, seq, header, len);
                }
                // Repeated requests and responses of an accepted client: answer with a
                // payload so it learns it is connected, at most once per keepalive.
                _ => {}
            }
            return;
        }
        match kind {
            Kind::Request => self.handle_request(now, len, from),
            Kind::Response => self.handle_response(now, seq, header, len, from),
            _ => self.stats.dropped += 1,
        }
    }

    fn handle_request(&mut self, now: Instant, len: usize, from: SocketAddr) {
        // Only full-size requests: the challenge answering one is smaller (no
        // amplification), and nothing at all answers a request without a valid token.
        if len != REQUEST_BYTES {
            self.stats.dropped += 1;
            return;
        }
        let unix_now = self.unix_now(now);
        let protocol_id = self.config.protocol_id;
        let Role::Server(server) = &mut self.role else { return };
        let mut r = Reader::new(&self.buf[1..len]);
        let parsed = (|| {
            let version = r.array::<8>()?;
            let pid = r.u64()?;
            let expire = r.u64()?;
            let nonce = r.array::<12>()?;
            let sealed = r.array::<PRIVATE_TOKEN_BYTES>()?;
            Some((version, pid, expire, nonce, sealed))
        })();
        let Some((version, pid, expire, nonce, mut sealed)) = parsed else {
            self.stats.dropped += 1;
            return;
        };
        if version != VERSION || pid != protocol_id || expire <= unix_now {
            self.stats.dropped += 1;
            return;
        }
        let mac: [u8; TAG_BYTES] = sealed[PRIVATE_TOKEN_BYTES - TAG_BYTES..].try_into().unwrap_or_default();
        let Some(token) = PrivateToken::open(&server.private, protocol_id, expire, &nonce, &mut sealed) else {
            self.stats.dropped += 1;
            return;
        };
        if !server.public_addresses.is_empty()
            && !token.server_addresses.as_slice().iter().any(|a| server.public_addresses.contains(a))
        {
            self.stats.dropped += 1;
            return;
        }
        // One token, one address: a token copied off the wire cannot be used elsewhere.
        match server.history.iter().find(|h| h.mac == mac && h.expire_unix > unix_now) {
            Some(h) if h.addr != from => {
                self.stats.dropped += 1;
                return;
            }
            Some(_) => {}
            None => {
                let entry = TokenUse { mac, addr: from, expire_unix: expire };
                if server.history.len() < server.history.capacity() {
                    server.history.push(entry);
                } else {
                    let i = server.history_next % server.history.len();
                    server.history[i] = entry;
                    server.history_next = i + 1;
                }
            }
        }
        // A client id already connected (from another address) is refused silently.
        if self.slots.iter().any(|c| c.active && c.client_id == token.client_id) {
            self.stats.dropped += 1;
            return;
        }
        let full = self.active >= server.max_clients;
        let pending_index = match server.pending.iter().position(|p| p.addr == from) {
            Some(i) if server.pending[i].client_id == token.client_id => {
                server.pending[i].expire = now + server.pending[i].timeout;
                i
            }
            existing => {
                let timeout = Duration::from_secs(u64::from(token.timeout_seconds.max(1)));
                let p = Pending {
                    addr: from,
                    client_id: token.client_id,
                    user_data: token.user_data,
                    timeout,
                    recv: Cipher::new(&token.client_to_server_key),
                    send: Cipher::new(&token.server_to_client_key),
                    expire: now + timeout,
                };
                match existing {
                    Some(i) => {
                        server.pending[i] = p;
                        i
                    }
                    None if server.pending.len() < server.pending.capacity() => {
                        server.pending.push(p);
                        server.pending.len() - 1
                    }
                    None => {
                        // Replace the entry closest to expiry.
                        let i = server
                            .pending
                            .iter()
                            .enumerate()
                            .min_by_key(|(_, p)| p.expire)
                            .map_or(0, |(i, _)| i);
                        server.pending[i] = p;
                        i
                    }
                }
            }
        };
        let seq = server.global_seq;
        server.global_seq += 1;
        let pending = &server.pending[pending_index];
        let h = packet::header_len(seq);
        let n = if full {
            packet::seal(&mut self.out, Kind::Denied, seq, protocol_id, &pending.send, 0)
        } else {
            let challenge_seq = server.challenge_seq;
            server.challenge_seq += 1;
            let mut sealed = [0u8; CHALLENGE_TOKEN_BYTES];
            seal_challenge(&server.challenge_key, challenge_seq, pending.client_id, &pending.user_data, &mut sealed);
            self.out[h..h + 8].copy_from_slice(&challenge_seq.to_le_bytes());
            self.out[h + 8..h + 8 + CHALLENGE_TOKEN_BYTES].copy_from_slice(&sealed);
            packet::seal(&mut self.out, Kind::Challenge, seq, protocol_id, &pending.send, 8 + CHALLENGE_TOKEN_BYTES)
        };
        let _ = self.socket.send_to(now, from, &self.out[..n]);
    }

    fn handle_response(&mut self, now: Instant, seq: u64, header: usize, len: usize, from: SocketAddr) {
        let protocol_id = self.config.protocol_id;
        let Role::Server(server) = &mut self.role else { return };
        let Some(pi) = server.pending.iter().position(|p| p.addr == from) else {
            self.stats.dropped += 1;
            return;
        };
        let Some(range) = packet::open(&mut self.buf[..len], seq, header, protocol_id, &server.pending[pi].recv) else {
            self.stats.dropped += 1;
            return;
        };
        let payload = &self.buf[range];
        if payload.len() != 8 + CHALLENGE_TOKEN_BYTES {
            self.stats.dropped += 1;
            return;
        }
        let challenge_seq = u64::from_le_bytes(payload[..8].try_into().unwrap_or_default());
        let mut sealed = [0u8; CHALLENGE_TOKEN_BYTES];
        sealed.copy_from_slice(&payload[8..]);
        let Some((client_id, _)) = open_challenge(&server.challenge_key, challenge_seq, &mut sealed) else {
            self.stats.dropped += 1;
            return;
        };
        if client_id != server.pending[pi].client_id {
            self.stats.dropped += 1;
            return;
        }
        if self.active >= server.max_clients {
            let seq = server.global_seq;
            server.global_seq += 1;
            let n = packet::seal(&mut self.out, Kind::Denied, seq, protocol_id, &server.pending[pi].send, 0);
            let _ = self.socket.send_to(now, from, &self.out[..n]);
            return;
        }
        let pending = server.pending.swap_remove(pi);
        // A free slot, or a new one (allocated once, reused afterwards).
        let index = match self.slots.iter().position(|c| !c.active) {
            Some(i) => i,
            None => {
                self.slots.push(Connection::new(&self.config, now));
                self.slots.len() - 1
            }
        };
        let conn = &mut *self.slots[index];
        conn.open(
            now,
            from,
            pending.client_id,
            pending.user_data,
            pending.send,
            pending.recv,
            pending.timeout,
            self.config.send_rate,
        );
        let id = conn.id(index);
        self.active += 1;
        self.by_addr.insert(from, index as u16);
        self.events.push_back(Event::Connected(id));
        // Tell the client right away (an empty payload).
        self.flush_connection(now, index, true);
    }

    // -----------------------------------------------------------------------------------
    // Client handshake.
    // -----------------------------------------------------------------------------------

    fn client_packet(&mut self, now: Instant, kind: Kind, seq: u64, header: usize, len: usize, from: SocketAddr) {
        let protocol_id = self.config.protocol_id;
        let Role::Client(client) = &mut self.role else { return };
        let server = client.token.server_addresses.as_slice()[client.address_index];
        if from != server || client.phase == Phase::Failed {
            self.stats.dropped += 1;
            return;
        }
        match (kind, client.phase) {
            (Kind::Payload | Kind::Disconnect, Phase::Connected) => {
                self.connection_packet(now, 0, kind, seq, header, len);
            }
            (Kind::Payload, Phase::Requesting | Phase::Responding) => {
                let conn = &mut *self.slots[0];
                conn.active = true;
                conn.addr = server;
                conn.last_recv = now;
                client.phase = Phase::Connected;
                self.active = 1;
                self.by_addr.insert(server, 0);
                let id = conn.id(0);
                self.events.push_back(Event::Connected(id));
                self.connection_packet(now, 0, kind, seq, header, len);
            }
            (Kind::Challenge, Phase::Requesting) => {
                let Some(cipher) = &self.slots[0].recv_cipher else { return };
                let Some(range) = packet::open(&mut self.buf[..len], seq, header, protocol_id, cipher) else {
                    self.stats.dropped += 1;
                    return;
                };
                let payload = &self.buf[range];
                if payload.len() != 8 + CHALLENGE_TOKEN_BYTES {
                    self.stats.dropped += 1;
                    return;
                }
                client.challenge_seq = u64::from_le_bytes(payload[..8].try_into().unwrap_or_default());
                client.challenge.copy_from_slice(&payload[8..]);
                client.phase = Phase::Responding;
                client.phase_start = now;
                self.send_response(now);
            }
            (Kind::Denied, Phase::Requesting | Phase::Responding) => {
                let Some(cipher) = &self.slots[0].recv_cipher else { return };
                if packet::open(&mut self.buf[..len], seq, header, protocol_id, cipher).is_none() {
                    self.stats.dropped += 1;
                    return;
                }
                client.phase = Phase::Failed;
                let id = self.slots[0].id(0);
                self.events.push_back(Event::Disconnected(id, DisconnectReason::Denied));
            }
            _ => self.stats.dropped += 1,
        }
    }

    fn send_request(&mut self, now: Instant) {
        let Role::Client(client) = &mut self.role else { return };
        let token = &client.token;
        let server = token.server_addresses.as_slice()[client.address_index];
        let mut w = Writer::new(&mut self.out[..REQUEST_BYTES]);
        let ok = (|| {
            w.u8(Kind::Request as u8)?;
            w.bytes(&VERSION)?;
            w.u64(token.protocol_id)?;
            w.u64(token.expire_timestamp)?;
            w.bytes(&token.nonce)?;
            w.bytes(&token.private)
        })();
        debug_assert!(ok.is_some());
        client.last_send = now;
        let _ = self.socket.send_to(now, server, &self.out[..REQUEST_BYTES]);
    }

    fn send_response(&mut self, now: Instant) {
        let protocol_id = self.config.protocol_id;
        let Role::Client(client) = &mut self.role else { return };
        let server = client.token.server_addresses.as_slice()[client.address_index];
        let conn = &mut *self.slots[0];
        let Some(cipher) = &conn.send_cipher else { return };
        let seq = conn.send_seq;
        conn.send_seq += 1;
        let h = packet::header_len(seq);
        self.out[h..h + 8].copy_from_slice(&client.challenge_seq.to_le_bytes());
        self.out[h + 8..h + 8 + CHALLENGE_TOKEN_BYTES].copy_from_slice(&client.challenge);
        let n = packet::seal(&mut self.out, Kind::Response, seq, protocol_id, cipher, 8 + CHALLENGE_TOKEN_BYTES);
        client.last_send = now;
        let _ = self.socket.send_to(now, server, &self.out[..n]);
    }

    fn client_tick(&mut self, now: Instant) {
        let unix_now = self.unix_now(now);
        let retry = self.config.connect_retry;
        let connect_timeout = self.config.connect_timeout;
        let Role::Client(client) = &mut self.role else { return };
        match client.phase {
            Phase::Requesting | Phase::Responding => {
                if client.token.expire_timestamp <= unix_now && client.phase == Phase::Requesting {
                    client.phase = Phase::Failed;
                    let id = self.slots[0].id(0);
                    self.events.push_back(Event::Disconnected(id, DisconnectReason::TokenExpired));
                    return;
                }
                let per_address = connect_timeout.min(Duration::from_secs(u64::from(client.token.timeout_seconds.max(1))));
                if now.saturating_duration_since(client.phase_start) > per_address {
                    if client.address_index + 1 < client.token.server_addresses.as_slice().len() {
                        client.address_index += 1;
                        client.phase = Phase::Requesting;
                        client.phase_start = now;
                        client.last_send = now.checked_sub(retry).unwrap_or(now);
                    } else {
                        client.phase = Phase::Failed;
                        let id = self.slots[0].id(0);
                        self.events.push_back(Event::Disconnected(id, DisconnectReason::ConnectTimedOut));
                        return;
                    }
                }
                if now.saturating_duration_since(client.last_send) >= retry {
                    if client.phase == Phase::Requesting {
                        self.send_request(now);
                    } else {
                        self.send_response(now);
                    }
                }
            }
            Phase::Connected | Phase::Failed => {}
        }
    }

    // -----------------------------------------------------------------------------------
    // Flush: build and send packets.
    // -----------------------------------------------------------------------------------

    /// Sends queued messages, acknowledgements and keepalives. Call once per network step
    /// after queuing messages.
    pub fn flush(&mut self, now: Instant) {
        engine_profiler::zone!("net::flush");
        for index in 0..self.slots.len() {
            if self.slots[index].active {
                self.flush_connection(now, index, false);
            }
        }
    }

    fn flush_connection(&mut self, now: Instant, index: usize, force: bool) {
        let Self { slots, out, socket, events, config, .. } = self;
        let conn = &mut *slots[index];
        let id = conn.id(index);
        if let Some(rate) = config.send_rate {
            let dt = now.saturating_duration_since(conn.last_refill).as_secs_f64();
            conn.tokens = (conn.tokens + dt * f64::from(rate)).min(burst(Some(rate)));
            conn.last_refill = now;
        }
        let Some(cipher) = conn.send_cipher.as_ref() else { return };
        for packet_index in 0..config.max_packets_per_flush {
            let has_budget = conn.tokens > 0.0 && conn.acks.may_send_data();
            let data = has_budget && conn.has_pending();
            let keepalive = now.saturating_duration_since(conn.last_send) >= config.keepalive_interval;
            if !(data || conn.acks.ack_pending || keepalive || (force && packet_index == 0)) {
                break;
            }
            let seq = conn.send_seq;
            let h = packet::header_len(seq);
            let end = config.max_packet_bytes - TAG_BYTES;
            debug_assert!(h <= MAX_HEADER_BYTES);
            let mut w = Writer::new(&mut out[h..end]);
            let mut entries = EntryList::new();
            let ok = match conn.acks.ack_header(now) {
                Some(ack) => write_ack(&mut w, &ack),
                None => w.u8(0),
            };
            debug_assert!(ok.is_some());
            let header_end = w.pos();
            if has_budget {
                for (ch, state) in conn.channels.iter_mut().enumerate() {
                    if let ChannelState::Reliable { send, .. } = state
                        && !send.write(ch as u8, &mut w, &mut entries)
                    {
                        break;
                    }
                }
                for (ch, state) in conn.channels.iter_mut().enumerate() {
                    if let ChannelState::Unreliable { send, .. } = state
                        && !send.write(ch as u8, &mut w, &mut entries)
                    {
                        break;
                    }
                }
            }
            let len = w.pos();
            let carries_data = len > header_end;
            let n = packet::seal(out, Kind::Payload, seq, config.protocol_id, cipher, len);
            let _ = socket.send_to(now, conn.addr, &out[..n]);
            conn.send_seq += 1;
            let Connection { acks, channels, .. } = conn;
            acks.on_sent(seq, now, n, carries_data, &entries, &mut |o| apply_outcome(channels, o, events, id));
            conn.acks.ack_sent();
            conn.last_send = now;
            conn.packets_sent += 1;
            conn.bytes_sent += n as u64;
            conn.send_rate.add(now, n);
            if config.send_rate.is_some() {
                conn.tokens -= n as f64;
            }
            if !conn.has_pending() || !conn.acks.may_send_data() {
                break;
            }
        }
        // Unreliable messages that did not go out are dropped.
        for (ch, state) in conn.channels.iter_mut().enumerate() {
            if let ChannelState::Unreliable { send, .. } = state {
                let channel = ChannelId(ch as u8);
                conn.send_dropped += send.end_flush(&mut |n| {
                    events.push_back(match n {
                        Notice::Delivered(m) => Event::Delivered(id, channel, m),
                        Notice::Lost(m) => Event::Lost(id, channel, m),
                    });
                }) as u64;
            }
        }
    }

    fn send_disconnect(&mut self, now: Instant, index: usize) {
        let Self { slots, out, socket, config, .. } = self;
        let conn = &mut *slots[index];
        let Some(cipher) = conn.send_cipher.as_ref() else { return };
        for _ in 0..DISCONNECT_REDUNDANCY {
            let seq = conn.send_seq;
            conn.send_seq += 1;
            let n = packet::seal(out, Kind::Disconnect, seq, config.protocol_id, cipher, 0);
            let _ = socket.send_to(now, conn.addr, &out[..n]);
        }
    }

    // -----------------------------------------------------------------------------------
    // Messages.
    // -----------------------------------------------------------------------------------

    /// Queues a message; it goes out on the next flush.
    pub fn send(&mut self, id: ConnectionId, channel: ChannelId, message: &[u8]) -> Result<MessageId, SendError> {
        let Self { events, slots, .. } = self;
        let conn = slots
            .get_mut(usize::from(id.index))
            .map(|c| &mut **c)
            .filter(|c| c.active && c.generation == id.generation)
            .ok_or(SendError::NotConnected)?;
        match conn.channels.get_mut(usize::from(channel.0)).ok_or(SendError::NoChannel)? {
            ChannelState::Reliable { send, .. } => send.queue(message),
            ChannelState::Unreliable { send, .. } => send.queue(message, &mut |n| {
                if let Notice::Lost(m) = n {
                    events.push_back(Event::Lost(id, channel, m));
                }
            }),
        }
    }

    /// The next received message on a channel, valid until the next call on this
    /// endpoint.
    pub fn receive(&mut self, id: ConnectionId, channel: ChannelId) -> Option<&[u8]> {
        let conn = self.conn_mut(id)?;
        match conn.channels.get_mut(usize::from(channel.0))? {
            ChannelState::Reliable { recv, .. } => recv.receive(),
            ChannelState::Unreliable { recv, .. } => recv.delivery.receive(),
        }
    }

    /// The next event.
    pub fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    /// A connection's statistics.
    pub fn stats(&self, id: ConnectionId) -> Option<ConnectionStats> {
        self.conn(id).map(Connection::stats)
    }

    /// Bytes the connection may still send now under `send_rate` (`usize::MAX` without
    /// one). Replication sizes its snapshot to this.
    pub fn send_budget(&self, id: ConnectionId) -> Option<usize> {
        let conn = self.conn(id)?;
        Some(match self.config.send_rate {
            Some(_) => conn.tokens.max(0.0) as usize,
            None => usize::MAX,
        })
    }

    /// Closes a connection, telling the peer. No event follows.
    pub fn disconnect(&mut self, now: Instant, id: ConnectionId) {
        if self.conn(id).is_some() {
            let index = usize::from(id.index);
            self.send_disconnect(now, index);
            self.close(index, None);
        }
    }

    /// Ids of live connections.
    pub fn connections(&self) -> Connections<'_> {
        Connections { slots: &self.slots, next: 0 }
    }

    /// Live connections.
    pub fn connection_count(&self) -> usize {
        self.active
    }
}

/// Iterator over live connection ids ([`Endpoint::connections`]).
#[derive(Debug)]
pub struct Connections<'a> {
    slots: &'a [Box<Connection>],
    next: usize,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection").field("addr", &self.addr).field("active", &self.active).finish_non_exhaustive()
    }
}

impl Iterator for Connections<'_> {
    type Item = ConnectionId;

    fn next(&mut self) -> Option<ConnectionId> {
        while let Some(c) = self.slots.get(self.next) {
            let i = self.next;
            self.next += 1;
            if c.active {
                return Some(c.id(i));
            }
        }
        None
    }
}

impl<S: DatagramSocket> Drop for Endpoint<S> {
    fn drop(&mut self) {
        // Best effort: tell peers right away instead of letting them time out.
        let now = Instant::now();
        for index in 0..self.slots.len() {
            if self.slots[index].active {
                self.send_disconnect(now, index);
            }
        }
    }
}

/// What the replication layer needs from a transport (ADR 0070). [`Endpoint`] implements
/// it on any [`DatagramSocket`]; other backends (Steam networking messages) can too.
pub trait Transport {
    /// Receives, handles timeouts and losses.
    fn update(&mut self, now: Instant);
    /// Sends what was queued.
    fn flush(&mut self, now: Instant);
    /// The next event.
    fn poll_event(&mut self) -> Option<Event>;
    /// Queues a message.
    fn send(&mut self, conn: ConnectionId, channel: ChannelId, message: &[u8]) -> Result<MessageId, SendError>;
    /// The next received message, valid until the next call.
    fn receive(&mut self, conn: ConnectionId, channel: ChannelId) -> Option<&[u8]>;
    /// Estimates and counters.
    fn stats(&self, conn: ConnectionId) -> Option<ConnectionStats>;
    /// Bytes that may still be sent now.
    fn send_budget(&self, conn: ConnectionId) -> Option<usize>;
    /// Closes a connection.
    fn disconnect(&mut self, now: Instant, conn: ConnectionId);
    /// Calls `f` for every live connection.
    fn for_each_connection(&self, f: &mut dyn FnMut(ConnectionId));
}

impl<S: DatagramSocket> Transport for Endpoint<S> {
    fn update(&mut self, now: Instant) {
        Endpoint::update(self, now);
    }
    fn flush(&mut self, now: Instant) {
        Endpoint::flush(self, now);
    }
    fn poll_event(&mut self) -> Option<Event> {
        Endpoint::poll_event(self)
    }
    fn send(&mut self, conn: ConnectionId, channel: ChannelId, message: &[u8]) -> Result<MessageId, SendError> {
        Endpoint::send(self, conn, channel, message)
    }
    fn receive(&mut self, conn: ConnectionId, channel: ChannelId) -> Option<&[u8]> {
        Endpoint::receive(self, conn, channel)
    }
    fn stats(&self, conn: ConnectionId) -> Option<ConnectionStats> {
        Endpoint::stats(self, conn)
    }
    fn send_budget(&self, conn: ConnectionId) -> Option<usize> {
        Endpoint::send_budget(self, conn)
    }
    fn disconnect(&mut self, now: Instant, conn: ConnectionId) {
        Endpoint::disconnect(self, now, conn);
    }
    fn for_each_connection(&self, f: &mut dyn FnMut(ConnectionId)) {
        for id in self.connections() {
            f(id);
        }
    }
}
