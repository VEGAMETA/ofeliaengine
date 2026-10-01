//! The network transport (ADR 0070): encrypted, connection-oriented UDP for the
//! authoritative client/server model of ADR 0019, and NAT traversal for co-op hosts.
//!
//! - **Connections** start from a [`ConnectToken`] (netcode's design): whoever admits
//!   players seals the token with a key the server knows; nothing answers a request
//!   without a valid token, and no answer is larger than the request.
//! - **Packets** are AES-256-GCM sealed (Windows CNG), numbered, replay-checked.
//! - **Channels** per connection: unreliable (snapshots), unreliable-sequenced, and
//!   reliable-ordered (events, chat); messages up to a configured size, fragmented and
//!   reassembled; optional delivery notification for unreliable messages.
//! - **Estimates** per connection: round trip, jitter, loss, rates; a send budget.
//! - **Sockets**: [`UdpSocket`] (Windows), [`memory::MemoryNetwork`] (latency, jitter,
//!   loss, reordering, NATs, on a virtual clock), or anything implementing
//!   [`DatagramSocket`].
//! - **NAT traversal** ([`nat`]): UPnP IGD → PCP / NAT-PMP → STUN and hole punching →
//!   relay, one UDP port per session, leases renewed, removed on exit.
//!
//! ```
//! use engine_net_transport::memory::MemoryNetwork;
//! use engine_net_transport::*;
//! use std::time::{Duration, Instant};
//!
//! let net = MemoryNetwork::new(1);
//! let server_addr = "10.0.0.1:27015".parse().unwrap();
//! let key = Key::random();
//! let config = EndpointConfig::new(0x0f1e_0001);
//! let t0 = Instant::now();
//! let mut server = Endpoint::server(
//!     net.bind(server_addr),
//!     config.clone(),
//!     ServerConfig { private_key: key.clone(), max_clients: 4, public_addresses: vec![server_addr] },
//!     t0,
//! )
//! .unwrap();
//! let token = ConnectToken::generate(
//!     &TokenParams {
//!         protocol_id: 0x0f1e_0001,
//!         client_id: 1,
//!         expire_seconds: 30,
//!         timeout_seconds: 10,
//!         server_addresses: &[server_addr],
//!         user_data: [0; USER_DATA_BYTES],
//!     },
//!     &key,
//!     unix_now(),
//! )
//! .unwrap();
//! let mut client = Endpoint::client(net.bind("10.0.0.2:5000".parse().unwrap()), config, token, t0).unwrap();
//!
//! // The game's network step: update, read, queue, flush.
//! let mut connected = None;
//! for step in 0..20 {
//!     let now = t0 + Duration::from_millis(16 * step);
//!     client.update(now);
//!     server.update(now);
//!     while let Some(event) = client.poll_event() {
//!         if let Event::Connected(id) = event {
//!             connected = Some(id);
//!             client.send(id, ChannelId(0), b"hello").unwrap();
//!         }
//!     }
//!     while let Some(_event) = server.poll_event() {}
//!     client.flush(now);
//!     server.flush(now);
//! }
//! let id = server.connections().next().unwrap();
//! server.update(t0 + Duration::from_secs(1));
//! assert_eq!(server.receive(id, ChannelId(0)), Some(&b"hello"[..]));
//! assert!(connected.is_some());
//! ```

#![deny(unsafe_op_in_unsafe_fn)]

mod ack;
mod bytes;
mod channel;
pub mod crypto;
mod endpoint;
pub mod memory;
pub mod nat;
mod packet;
mod socket;
mod token;

pub use channel::{ChannelConfig, ChannelKind, FRAGMENT_BYTES, MAX_CHANNELS, MessageId, SendError};
pub use crypto::Key;
pub use endpoint::{
    ChannelId, ConfigError, ConnectionId, ConnectionStats, Connections, DisconnectReason, Endpoint,
    EndpointConfig, EndpointStats, Event, ServerConfig, Transport,
};
pub use packet::is_transport_packet;
pub use socket::{DatagramSocket, UdpSocket};
pub use token::{
    AddressList, CONNECT_TOKEN_BYTES, ConnectToken, MAX_SERVER_ADDRESSES, TokenError, TokenParams,
    USER_DATA_BYTES, VERSION, unix_now,
};
