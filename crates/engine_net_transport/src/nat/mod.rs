//! NAT traversal for hosts of co-op games (listen servers), ADR 0070. The ladder:
//!
//! 1. **UPnP IGD** ([`upnp`]): ask the home router to forward one UDP port.
//! 2. **PCP, then NAT-PMP** ([`pmp`]): the same through the gateway's port-control
//!    service (Apple routers, many newer ones; PCP also opens IPv6 firewall pinholes).
//! 3. **STUN + hole punching** ([`stun`], [`punch`]): learn the router's mapping of the
//!    game socket and open it toward each joining player through a rendezvous.
//! 4. **Relay** ([`relay`]): forward through a server (our reference relay, Steam
//!    Datagram Relay supplied by the game, TURN).
//!
//! Exactly one UDP port is mapped per session (the game socket's), the lease is renewed
//! at half its life, and the mapping is removed when the [`PortMapper`] is stopped or
//! dropped. A crash leaves it until the lease runs out (one hour by default), which is why
//! permanent mappings are only used when the router accepts nothing else.
//!
//! No server is contacted unless the game configures it: there are no default STUN
//! servers, relays or rendezvous services (no telemetry, CLAUDE.md principles).

pub mod gateway;
pub mod pmp;
pub mod punch;
pub mod relay;
pub mod stun;
pub mod upnp;

use std::net::{IpAddr, SocketAddr};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::endpoint::Endpoint;
use crate::socket::DatagramSocket;

/// Why a traversal step failed.
#[derive(Debug, thiserror::Error)]
pub enum NatError {
    /// A socket or HTTP error.
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    /// Nothing answered in time.
    #[error("no answer")]
    NoAnswer,
    /// The gateway refused with this protocol error code (UPnP errorCode, PCP or NAT-PMP
    /// result code).
    #[error("refused with code {0}")]
    Refused(u16),
    /// An HTTP status other than 200 without a UPnP error.
    #[error("HTTP status {0}")]
    Http(u16),
    /// The device has no WAN connection service.
    #[error("unsupported device")]
    Unsupported,
    /// An answer that does not parse.
    #[error("malformed answer")]
    Malformed,
}

/// Which rung of the ladder made the host reachable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    /// The host has a public address; no NAT.
    Public,
    /// A UPnP IGD port mapping.
    Upnp,
    /// A PCP port mapping.
    Pcp,
    /// A NAT-PMP port mapping.
    NatPmp,
    /// No mapping, but the NAT maps endpoint-independently: joining players punch holes.
    HolePunch,
    /// Players go through a relay.
    Relay,
    /// Nothing works: only players on the same LAN can join.
    LanOnly,
}

/// Whether `ip` is reachable from the internet: not private, carrier-grade NAT shared
/// space (100.64.0.0/10, RFC 6598), loopback, link-local, unique-local, multicast or
/// unspecified.
pub fn is_global(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || (o[0] == 100 && (o[1] & 0xc0) == 64)
                || o[0] == 0
                || o[0] >= 224)
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00
                || (s[0] & 0xffc0) == 0xfe80)
        }
    }
}

/// A port mapping the [`PortMapper`] holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortMapping {
    /// [`Method::Upnp`], [`Method::Pcp`] or [`Method::NatPmp`].
    pub method: Method,
    /// The address players reach the host at.
    pub external: SocketAddr,
    /// The lease (zero: permanent, the router refused leases).
    pub lease: Duration,
    /// Renewals so far.
    pub renewals: u32,
}

/// Where the port mapper is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MappingStatus {
    /// Still asking.
    Pending,
    /// Mapped.
    Mapped(PortMapping),
    /// Neither UPnP nor PCP/NAT-PMP worked (the reasons, for the log).
    Failed(String),
    /// Stopped; the mapping was removed.
    Stopped,
}

/// Settings of the port mapper.
#[derive(Clone, Debug)]
pub struct NatConfig {
    /// Try UPnP IGD.
    pub upnp: bool,
    /// Try PCP and NAT-PMP.
    pub pcp: bool,
    /// The lease asked for, renewed at half of it.
    pub lease: Duration,
    /// The mapping's description in the router's UI (UPnP).
    pub description: String,
    /// How long SSDP discovery may take.
    pub discovery_timeout: Duration,
    /// Where SSDP searches go ([`upnp::SSDP_ADDR`]; tests use a local responder).
    pub ssdp_target: SocketAddr,
    /// The PCP / NAT-PMP server (`None`: the default gateway, port 5351).
    pub gateway: Option<SocketAddr>,
    /// The local address mappings point to (`None`: the default route's source).
    pub local_ip: Option<IpAddr>,
    /// Retries of PCP / NAT-PMP requests.
    pub retry: pmp::Retry,
}

impl Default for NatConfig {
    fn default() -> Self {
        Self {
            upnp: true,
            pcp: true,
            lease: Duration::from_secs(3600),
            description: String::from("ofeliaengine game"),
            discovery_timeout: Duration::from_secs(2),
            ssdp_target: upnp::SSDP_ADDR,
            gateway: None,
            local_ip: None,
            retry: pmp::Retry::default(),
        }
    }
}

enum Held {
    Upnp(upnp::Gateway, u16),
    Pmp(SocketAddr, pmp::PmpMapping),
}

/// Maps the game socket's port on the router from a background thread: UPnP first, then
/// PCP / NAT-PMP; renews the lease; removes the mapping on [`PortMapper::stop`] or drop.
pub struct PortMapper {
    status: Arc<Mutex<MappingStatus>>,
    stop: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for PortMapper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PortMapper").field("status", &self.status()).finish()
    }
}

impl PortMapper {
    /// Starts mapping UDP `internal_port` (the game socket's port).
    pub fn start(config: NatConfig, internal_port: u16) -> Self {
        let status = Arc::new(Mutex::new(MappingStatus::Pending));
        let (stop, stopped) = mpsc::channel();
        let shared = status.clone();
        let thread = std::thread::Builder::new()
            .name(String::from("net::port-mapper"))
            .spawn(move || run_mapper(&config, internal_port, &shared, &stopped))
            .ok();
        Self { status, stop: Some(stop), thread }
    }

    /// The current status.
    pub fn status(&self) -> MappingStatus {
        self.status.lock().map_or(MappingStatus::Pending, |s| s.clone())
    }

    /// Removes the mapping and waits for the thread (at most a few seconds).
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        drop(self.stop.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for PortMapper {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn set(status: &Mutex<MappingStatus>, value: MappingStatus) {
    if let Ok(mut s) = status.lock() {
        *s = value;
    }
}

fn try_upnp(config: &NatConfig, local: IpAddr, port: u16) -> Result<(upnp::Gateway, PortMapping), NatError> {
    let gw = upnp::discover(config.ssdp_target, local, config.discovery_timeout)?;
    let external_ip = gw.external_ip()?;
    let mut lease = config.lease;
    // The game's own port first, then a few others if another host holds it.
    let mut candidates = [port; 4];
    for c in candidates.iter_mut().skip(1) {
        *c = 49_152 + (crate::crypto::random_u64() % 16_000) as u16;
    }
    let mut last = NatError::NoAnswer;
    for external in candidates {
        let mut result = gw.add_mapping(external, port, lease, &config.description);
        if matches!(result, Err(NatError::Refused(upnp::ERROR_ONLY_PERMANENT))) && !lease.is_zero() {
            lease = Duration::ZERO;
            result = gw.add_mapping(external, port, lease, &config.description);
        }
        match result {
            Ok(()) => {
                let mapping = PortMapping {
                    method: Method::Upnp,
                    external: SocketAddr::new(external_ip, external),
                    lease,
                    renewals: 0,
                };
                return Ok((gw, mapping));
            }
            Err(NatError::Refused(upnp::ERROR_CONFLICT)) => last = NatError::Refused(upnp::ERROR_CONFLICT),
            Err(e) => return Err(e),
        }
    }
    Err(last)
}

fn run_mapper(config: &NatConfig, port: u16, status: &Mutex<MappingStatus>, stopped: &mpsc::Receiver<()>) {
    let route = gateway::default_route(false).ok();
    let local = config.local_ip.or(route.map(|r| r.local));
    let gw = config.gateway.or(route.map(|r| SocketAddr::new(r.gateway, pmp::SERVER_PORT)));
    let mut reasons = Vec::new();
    let mut held = None;
    match local {
        Some(local) => {
            if config.upnp {
                match try_upnp(config, local, port) {
                    Ok((g, m)) => held = Some((Held::Upnp(g, m.external.port()), m)),
                    Err(e) => reasons.push(format!("UPnP: {e}")),
                }
            }
            if held.is_none() && config.pcp {
                match gw {
                    Some(gw) => match pmp::map(gw, local, port, config.lease, config.retry, None) {
                        Ok(m) => {
                            let mapping = PortMapping {
                                method: match m.protocol {
                                    pmp::PmpProtocol::Pcp => Method::Pcp,
                                    pmp::PmpProtocol::NatPmp => Method::NatPmp,
                                },
                                external: m.external,
                                lease: m.lifetime,
                                renewals: 0,
                            };
                            held = Some((Held::Pmp(gw, m), mapping));
                        }
                        Err(e) => reasons.push(format!("PCP/NAT-PMP: {e}")),
                    },
                    None => reasons.push(String::from("PCP/NAT-PMP: no default gateway")),
                }
            }
        }
        None => reasons.push(String::from("no local address (no default route)")),
    }
    let Some((mut held, mut mapping)) = held else {
        set(status, MappingStatus::Failed(reasons.join("; ")));
        let _ = stopped.recv();
        return;
    };
    set(status, MappingStatus::Mapped(mapping));
    loop {
        let wait = if mapping.lease.is_zero() { Duration::from_secs(3600) } else { mapping.lease / 2 };
        match stopped.recv_timeout(wait) {
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if mapping.lease.is_zero() {
                    continue;
                }
                let renewed = match &mut held {
                    Held::Upnp(g, external) => g
                        .add_mapping(*external, port, mapping.lease, &config.description)
                        .map(|()| mapping.lease),
                    Held::Pmp(gw, m) => pmp::map(*gw, m.local_ip(), port, config.lease, config.retry, Some(m)).map(|n| {
                        *m = n;
                        n.lifetime
                    }),
                };
                if let Ok(lease) = renewed {
                    mapping.lease = lease;
                    mapping.renewals += 1;
                    set(status, MappingStatus::Mapped(mapping));
                }
            }
            // Stop requested, or the mapper was dropped.
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    match &held {
        Held::Upnp(g, external) => {
            let _ = g.delete_mapping(*external);
        }
        Held::Pmp(gw, m) => {
            let _ = pmp::unmap(*gw, m, config.retry);
        }
    }
    set(status, MappingStatus::Stopped);
}

/// What the host should advertise, and how it got there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NatReport {
    /// The rung that worked.
    pub method: Method,
    /// Addresses for connect tokens and for [`Endpoint::set_public_addresses`], best first
    /// (the external one, then the LAN one for players on the same network).
    pub addresses: Vec<SocketAddr>,
}

/// Picks the rung from what the port mapper and STUN found. `local` is the game
/// socket's LAN address.
///
/// A mapping whose external address is not global (a router behind another NAT, or
/// behind carrier-grade NAT) or disagrees with what STUN saw is useless and skipped.
pub fn decide(local: SocketAddr, mapping: Option<&PortMapping>, stun: Option<&stun::StunResult>, relay: bool) -> NatReport {
    let mut addresses = Vec::new();
    let stun_mapped = stun.and_then(|s| s.mapped);
    let method = if let Some(mapped) = stun_mapped.filter(|m| *m == local && is_global(m.ip())) {
        addresses.push(mapped);
        Method::Public
    } else if let Some(m) =
        mapping.filter(|m| is_global(m.external.ip()) && stun_mapped.is_none_or(|s| s.ip() == m.external.ip()))
    {
        addresses.push(m.external);
        m.method
    } else if let Some(s) = stun.filter(|s| s.mapped.is_some() && !s.mapping_varies) {
        addresses.extend(s.mapped);
        Method::HolePunch
    } else if relay {
        Method::Relay
    } else {
        Method::LanOnly
    };
    if !addresses.contains(&local) && !local.ip().is_unspecified() {
        addresses.push(local);
    }
    NatReport { method, addresses }
}

/// Pumps a STUN probe and hole punches on the game socket: sends what they have due and
/// hands them the foreign datagrams the endpoint received. Call once per network step
/// after [`Endpoint::update`].
#[derive(Debug, Default)]
pub struct NatSession {
    /// The STUN probe, if one runs.
    pub stun: Option<stun::StunProbe>,
    /// Hole punches in progress or done.
    pub punches: Vec<punch::Puncher>,
}

impl NatSession {
    /// A session probing `stun_servers` (none: no STUN).
    pub fn new(stun_servers: &[SocketAddr], now: Instant) -> Self {
        Self {
            stun: (!stun_servers.is_empty()).then(|| stun::StunProbe::new(stun_servers, now)),
            punches: Vec::new(),
        }
    }

    /// One step.
    pub fn service<S: DatagramSocket>(&mut self, endpoint: &mut Endpoint<S>, now: Instant) {
        let mut buf = [0u8; 1500];
        while let Some((n, from)) = endpoint.poll_foreign(&mut buf) {
            let data = &buf[..n];
            if stun::is_stun(data) {
                if let Some(p) = &mut self.stun {
                    p.handle(from, data);
                }
            } else if data.first() == Some(&punch::PUNCH_PREFIX) {
                for p in &mut self.punches {
                    if p.handle(now, from, data) {
                        break;
                    }
                }
            }
        }
        if let Some(p) = &mut self.stun {
            let mut out = [0u8; stun::REQUEST_BYTES];
            while let Some(to) = p.poll_transmit(now, &mut out) {
                let _ = endpoint.send_raw(now, to, &out);
            }
        }
        let mut out = [0u8; punch::PUNCH_BYTES];
        for p in &mut self.punches {
            while let Some(to) = p.poll_transmit(now, &mut out) {
                let _ = endpoint.send_raw(now, to, &out);
            }
        }
    }

    /// Whether the STUN probe finished (or none runs).
    pub fn stun_done(&self, now: Instant) -> bool {
        self.stun.as_ref().is_none_or(|p| p.is_done(now))
    }

    /// The STUN findings.
    pub fn stun_result(&self) -> Option<stun::StunResult> {
        self.stun.as_ref().map(stun::StunProbe::result)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn a(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn global_addresses() {
        for ip in [
            "10.1.2.3", "192.168.0.1", "172.16.5.4", "100.64.0.1", "100.127.255.255", "127.0.0.1", "169.254.1.1", "0.0.0.0",
            "fd00::1", "fe80::1", "::1",
        ] {
            assert!(!is_global(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["203.0.113.4", "8.8.8.8", "100.128.0.1", "2001:db8::1"] {
            assert!(is_global(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn the_ladder_picks_the_best_rung() {
        let local = a("192.168.1.10:27015");
        let upnp = PortMapping {
            method: Method::Upnp,
            external: a("203.0.113.4:27015"),
            lease: Duration::from_secs(3600),
            renewals: 0,
        };
        let cgnat = PortMapping { external: a("100.70.1.2:27015"), ..upnp };
        let stun_ok = stun::StunResult { mapped: Some(a("203.0.113.4:40001")), answers: 2, mapping_varies: false };
        let stun_sym = stun::StunResult { mapping_varies: true, ..stun_ok };
        let stun_other = stun::StunResult { mapped: Some(a("198.51.100.9:40001")), ..stun_ok };
        assert_eq!(decide(local, Some(&upnp), Some(&stun_ok), false).method, Method::Upnp);
        assert_eq!(decide(local, Some(&upnp), Some(&stun_ok), false).addresses, vec![a("203.0.113.4:27015"), local]);
        // A mapping on a router behind carrier-grade NAT is useless.
        assert_eq!(decide(local, Some(&cgnat), Some(&stun_other), true).method, Method::HolePunch);
        // The mapped address disagrees with STUN (double NAT with a public inner WAN).
        assert_eq!(decide(local, Some(&upnp), Some(&stun_other), false).method, Method::HolePunch);
        assert_eq!(decide(local, None, Some(&stun_sym), true).method, Method::Relay);
        assert_eq!(decide(local, None, Some(&stun_sym), false).method, Method::LanOnly);
        let public = a("203.0.113.4:27015");
        let stun_public = stun::StunResult { mapped: Some(public), ..stun_ok };
        assert_eq!(decide(public, None, Some(&stun_public), false).method, Method::Public);
        assert_eq!(decide(local, None, None, false).method, Method::LanOnly);
    }
}
