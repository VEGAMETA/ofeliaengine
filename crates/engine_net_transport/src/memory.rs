//! An in-memory network for tests and for developing replication: latency, jitter, loss,
//! duplication, reordering, and NATs, all on a virtual clock and deterministic for a seed.
//!
//! ```
//! use engine_net_transport::memory::{LinkConditions, MemoryNetwork};
//! use engine_net_transport::DatagramSocket;
//! use std::time::{Duration, Instant};
//!
//! let net = MemoryNetwork::new(1);
//! net.set_default_conditions(LinkConditions::new(Duration::from_millis(40), Duration::from_millis(5), 0.1));
//! let mut a = net.bind("10.0.0.1:1000".parse().unwrap());
//! let mut b = net.bind("10.0.0.2:1000".parse().unwrap());
//! let t0 = Instant::now();
//! a.send_to(t0, b.local_addr().unwrap(), b"hi").unwrap();
//! let mut buf = [0u8; 16];
//! assert!(b.recv_from(t0, &mut buf).unwrap().is_none()); // still on the wire
//! ```

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::socket::DatagramSocket;

/// What a link does to datagrams.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LinkConditions {
    /// One-way delay.
    pub latency: Duration,
    /// Extra delay, uniform in `0..jitter`, independent per datagram (reorders too).
    pub jitter: Duration,
    /// Probability a datagram is lost.
    pub loss: f32,
    /// Probability a datagram arrives twice.
    pub duplicate: f32,
    /// Probability a datagram is held back by another `latency` (arrives after later ones).
    pub reorder: f32,
}

impl LinkConditions {
    /// No delay, no loss.
    pub const PERFECT: Self = Self {
        latency: Duration::ZERO,
        jitter: Duration::ZERO,
        loss: 0.0,
        duplicate: 0.0,
        reorder: 0.0,
    };

    /// Latency, jitter and loss; no duplication or forced reordering.
    pub const fn new(latency: Duration, jitter: Duration, loss: f32) -> Self {
        Self {
            latency,
            jitter,
            loss,
            duplicate: 0.0,
            reorder: 0.0,
        }
    }
}

/// How a simulated NAT maps and filters (RFC 4787 terms).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NatBehavior {
    /// Endpoint-independent mapping and filtering ("full cone"): anyone may send to a
    /// mapping once it exists.
    FullCone,
    /// Endpoint-independent mapping, address-and-port-dependent filtering ("port
    /// restricted cone", most home routers): inbound only from where the host sent to.
    PortRestricted,
    /// Address-and-port-dependent mapping ("symmetric", many carrier-grade NATs): a new
    /// external port per destination. Hole punching fails; relays are needed.
    Symmetric,
}

/// A NAT added with [`MemoryNetwork::add_nat`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NatId(usize);

struct Mapping {
    internal: SocketAddr,
    /// The destination this mapping is for (symmetric NATs only).
    remote: Option<SocketAddr>,
    external_port: u16,
}

struct Nat {
    public_ip: IpAddr,
    behavior: NatBehavior,
    next_port: u16,
    mappings: Vec<Mapping>,
    /// (external port, remote) pairs the host has sent to.
    permissions: Vec<(u16, SocketAddr)>,
}

struct Datagram {
    deliver_at: Instant,
    order: u64,
    from: SocketAddr,
    data: Vec<u8>,
}

/// Traffic counters of the whole network.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NetworkStats {
    /// Datagrams sent.
    pub sent: u64,
    /// Datagrams lost by link conditions.
    pub lost: u64,
    /// Datagrams dropped by NAT filtering or for want of a receiver.
    pub filtered: u64,
    /// Datagrams delivered.
    pub delivered: u64,
}

struct Inner {
    inboxes: HashMap<SocketAddr, Vec<Datagram>>,
    pool: Vec<Vec<u8>>,
    rng: u64,
    order: u64,
    default: LinkConditions,
    links: HashMap<(SocketAddr, SocketAddr), LinkConditions>,
    nats: Vec<Nat>,
    behind: HashMap<SocketAddr, usize>,
    stats: NetworkStats,
}

impl Inner {
    fn random(&mut self) -> f32 {
        // xorshift64*
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        (self.rng.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 40) as f32 / (1u64 << 24) as f32
    }

    /// The source address the world sees, creating a NAT mapping if needed.
    fn translate_out(&mut self, from: SocketAddr, to: SocketAddr) -> SocketAddr {
        let Some(&n) = self.behind.get(&from) else { return from };
        let nat = &mut self.nats[n];
        let remote = (nat.behavior == NatBehavior::Symmetric).then_some(to);
        let port = match nat.mappings.iter().find(|m| m.internal == from && m.remote == remote) {
            Some(m) => m.external_port,
            None => {
                let port = nat.next_port;
                nat.next_port = nat.next_port.wrapping_add(1).max(1024);
                nat.mappings.push(Mapping {
                    internal: from,
                    remote,
                    external_port: port,
                });
                port
            }
        };
        if !nat.permissions.contains(&(port, to)) {
            nat.permissions.push((port, to));
        }
        SocketAddr::new(nat.public_ip, port)
    }

    /// The internal address a datagram to `to` from `from` reaches, if the NAT lets it in.
    fn translate_in(&self, from: SocketAddr, to: SocketAddr) -> Option<SocketAddr> {
        let Some(nat) = self.nats.iter().find(|n| n.public_ip == to.ip()) else {
            return Some(to);
        };
        let mapping = nat.mappings.iter().find(|m| m.external_port == to.port())?;
        let allowed = match nat.behavior {
            NatBehavior::FullCone => true,
            NatBehavior::PortRestricted | NatBehavior::Symmetric => {
                nat.permissions.contains(&(to.port(), from))
            }
        };
        allowed.then_some(mapping.internal)
    }
}

/// The shared network. Clones refer to the same network.
#[derive(Clone)]
pub struct MemoryNetwork {
    inner: Arc<Mutex<Inner>>,
}

impl std::fmt::Debug for MemoryNetwork {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MemoryNetwork")
    }
}

impl MemoryNetwork {
    /// An empty network whose random choices follow `seed`.
    pub fn new(seed: u64) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                inboxes: HashMap::new(),
                pool: Vec::new(),
                rng: seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1,
                order: 0,
                default: LinkConditions::PERFECT,
                links: HashMap::new(),
                nats: Vec::new(),
                behind: HashMap::new(),
                stats: NetworkStats::default(),
            })),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A panic while holding the lock is a test failure already; keep going.
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Conditions for every link without its own.
    pub fn set_default_conditions(&self, conditions: LinkConditions) {
        self.lock().default = conditions;
    }

    /// Conditions from `from` to `to` (one direction; internal addresses).
    pub fn set_link_conditions(&self, from: SocketAddr, to: SocketAddr, conditions: LinkConditions) {
        self.lock().links.insert((from, to), conditions);
    }

    /// A socket at `addr` with a public address.
    pub fn bind(&self, addr: SocketAddr) -> MemorySocket {
        self.lock().inboxes.insert(addr, Vec::with_capacity(4096));
        MemorySocket {
            net: self.clone(),
            addr,
        }
    }

    /// Adds a NAT with the public address `public_ip`.
    pub fn add_nat(&self, public_ip: IpAddr, behavior: NatBehavior) -> NatId {
        let mut inner = self.lock();
        inner.nats.push(Nat {
            public_ip,
            behavior,
            next_port: 40_000,
            mappings: Vec::new(),
            permissions: Vec::new(),
        });
        NatId(inner.nats.len() - 1)
    }

    /// A socket at the private address `addr` behind `nat`.
    pub fn bind_behind_nat(&self, addr: SocketAddr, nat: NatId) -> MemorySocket {
        self.lock().behind.insert(addr, nat.0);
        self.bind(addr)
    }

    /// Traffic counters.
    pub fn stats(&self) -> NetworkStats {
        self.lock().stats
    }

    fn send(&self, now: Instant, from: SocketAddr, to: SocketAddr, data: &[u8]) {
        let mut inner = self.lock();
        inner.stats.sent += 1;
        let conditions = inner.links.get(&(from, to)).copied().unwrap_or(inner.default);
        let source = inner.translate_out(from, to);
        if inner.random() < conditions.loss {
            inner.stats.lost += 1;
            return;
        }
        let Some(dest) = inner.translate_in(source, to) else {
            inner.stats.filtered += 1;
            return;
        };
        if !inner.inboxes.contains_key(&dest) {
            inner.stats.filtered += 1;
            return;
        }
        let copies = if inner.random() < conditions.duplicate { 2 } else { 1 };
        for _ in 0..copies {
            let mut delay = conditions.latency + conditions.jitter.mul_f32(inner.random());
            if inner.random() < conditions.reorder {
                delay += conditions.latency.max(Duration::from_millis(1));
            }
            let mut buf = inner.pool.pop().unwrap_or_default();
            buf.clear();
            buf.extend_from_slice(data);
            inner.order += 1;
            let order = inner.order;
            if let Some(inbox) = inner.inboxes.get_mut(&dest) {
                inbox.push(Datagram {
                    deliver_at: now + delay,
                    order,
                    from: source,
                    data: buf,
                });
            }
        }
    }

    fn recv(&self, now: Instant, at: SocketAddr, out: &mut [u8]) -> Option<(usize, SocketAddr)> {
        let mut inner = self.lock();
        let inner = &mut *inner;
        let inbox = inner.inboxes.get_mut(&at)?;
        let (i, _) = inbox
            .iter()
            .enumerate()
            .filter(|(_, d)| d.deliver_at <= now)
            .min_by_key(|(_, d)| (d.deliver_at, d.order))?;
        let d = inbox.swap_remove(i);
        let n = d.data.len().min(out.len());
        out[..n].copy_from_slice(&d.data[..n]);
        inner.stats.delivered += 1;
        let from = d.from;
        inner.pool.push(d.data);
        Some((n, from))
    }
}

/// A socket on a [`MemoryNetwork`].
#[derive(Debug)]
pub struct MemorySocket {
    net: MemoryNetwork,
    addr: SocketAddr,
}

impl DatagramSocket for MemorySocket {
    fn send_to(&mut self, now: Instant, to: SocketAddr, data: &[u8]) -> io::Result<()> {
        self.net.send(now, self.addr, to, data);
        Ok(())
    }

    fn recv_from(&mut self, now: Instant, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        Ok(self.net.recv(now, self.addr, buf))
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.addr)
    }
}

impl Drop for MemorySocket {
    fn drop(&mut self) {
        let mut inner = self.net.lock();
        if let Some(inbox) = inner.inboxes.remove(&self.addr) {
            inner.pool.extend(inbox.into_iter().map(|d| d.data));
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn latency_loss_and_order() {
        let net = MemoryNetwork::new(7);
        net.set_default_conditions(LinkConditions::new(Duration::from_millis(50), Duration::ZERO, 0.25));
        let mut a = net.bind(addr("10.0.0.1:1"));
        let mut b = net.bind(addr("10.0.0.2:1"));
        let t0 = Instant::now();
        for i in 0..1000u32 {
            a.send_to(t0, addr("10.0.0.2:1"), &i.to_le_bytes()).unwrap();
        }
        let mut buf = [0u8; 8];
        assert!(b.recv_from(t0 + Duration::from_millis(49), &mut buf).unwrap().is_none());
        let mut got = Vec::new();
        while let Some((n, from)) = b.recv_from(t0 + Duration::from_millis(50), &mut buf).unwrap() {
            assert_eq!((n, from), (4, addr("10.0.0.1:1")));
            got.push(u32::from_le_bytes(buf[..4].try_into().unwrap()));
        }
        assert!((700..800).contains(&got.len()), "{}", got.len());
        assert!(got.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn nats_map_and_filter() {
        let net = MemoryNetwork::new(1);
        let home = net.add_nat("198.51.100.1".parse().unwrap(), NatBehavior::PortRestricted);
        let mut host = net.bind_behind_nat(addr("192.168.0.2:7777"), home);
        let mut server = net.bind(addr("203.0.113.5:3478"));
        let mut stranger = net.bind(addr("203.0.113.9:1"));
        let t0 = Instant::now();
        let mut buf = [0u8; 8];
        host.send_to(t0, addr("203.0.113.5:3478"), b"x").unwrap();
        let (_, mapped) = server.recv_from(t0, &mut buf).unwrap().unwrap();
        assert_eq!(mapped.ip(), "198.51.100.1".parse::<IpAddr>().unwrap());
        // The server may answer; a stranger may not until the host sends to it.
        server.send_to(t0, mapped, b"y").unwrap();
        stranger.send_to(t0, mapped, b"z").unwrap();
        assert_eq!(host.recv_from(t0, &mut buf).unwrap().unwrap().1, addr("203.0.113.5:3478"));
        assert!(host.recv_from(t0, &mut buf).unwrap().is_none());
        host.send_to(t0, addr("203.0.113.9:1"), b"p").unwrap();
        stranger.recv_from(t0, &mut buf).unwrap().unwrap();
        stranger.send_to(t0, mapped, b"z").unwrap();
        assert_eq!(host.recv_from(t0, &mut buf).unwrap().unwrap().1, addr("203.0.113.9:1"));
    }
}
