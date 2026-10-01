//! PCP (RFC 6887) and NAT-PMP (RFC 6886): ask the gateway on UDP port 5351 for a port
//! mapping. PCP first (it also opens IPv6 firewall pinholes); a NAT-PMP-only gateway
//! answers a PCP request with version 0 "unsupported version", and we fall back.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use super::NatError;
use crate::crypto::random_bytes;

/// The PCP / NAT-PMP server port on the gateway.
pub const SERVER_PORT: u16 = 5351;
const PCP_VERSION: u8 = 2;
const PCP_OPCODE_MAP: u8 = 1;
const UDP: u8 = 17;
const NATPMP_OP_ADDRESS: u8 = 0;
const NATPMP_OP_MAP_UDP: u8 = 1;
/// Result code both protocols use for "unsupported version".
const UNSUPPORTED_VERSION: u8 = 1;

/// Which protocol made a mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PmpProtocol {
    /// Port Control Protocol, RFC 6887.
    Pcp,
    /// NAT Port Mapping Protocol, RFC 6886.
    NatPmp,
}

/// A mapping the gateway granted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PmpMapping {
    /// The protocol.
    pub protocol: PmpProtocol,
    /// The address peers reach us at.
    pub external: SocketAddr,
    /// How long the gateway keeps it; renew at half.
    pub lifetime: Duration,
    /// The local port.
    pub internal_port: u16,
    nonce: [u8; 12],
    local: IpAddr,
}

impl PmpMapping {
    /// The local address the mapping points to.
    pub fn local_ip(&self) -> IpAddr {
        self.local
    }
}

/// An IPv4 address as PCP carries it (IPv4-mapped IPv6).
fn pcp_addr(ip: IpAddr) -> [u8; 16] {
    match ip {
        IpAddr::V4(v4) => v4.to_ipv6_mapped().octets(),
        IpAddr::V6(v6) => v6.octets(),
    }
}

fn from_pcp_addr(b: [u8; 16]) -> IpAddr {
    let v6 = Ipv6Addr::from(b);
    match v6.to_ipv4_mapped() {
        Some(v4) => IpAddr::V4(v4),
        None => IpAddr::V6(v6),
    }
}

/// A PCP MAP request (60 bytes).
pub fn pcp_map_request(
    local: IpAddr,
    nonce: &[u8; 12],
    internal_port: u16,
    suggested_port: u16,
    lifetime: u32,
) -> [u8; 60] {
    let mut r = [0u8; 60];
    r[0] = PCP_VERSION;
    r[1] = PCP_OPCODE_MAP;
    r[4..8].copy_from_slice(&lifetime.to_be_bytes());
    r[8..24].copy_from_slice(&pcp_addr(local));
    r[24..36].copy_from_slice(nonce);
    r[36] = UDP;
    r[40..42].copy_from_slice(&internal_port.to_be_bytes());
    r[42..44].copy_from_slice(&suggested_port.to_be_bytes());
    // Suggested external address: all zeros of the local family (no preference).
    let any = match local {
        IpAddr::V4(_) => pcp_addr(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
        IpAddr::V6(_) => [0u8; 16],
    };
    r[44..60].copy_from_slice(&any);
    r
}

/// What a gateway answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PmpResponse {
    /// PCP MAP response.
    Pcp {
        /// Result code (0 success).
        result: u8,
        /// Granted lifetime, seconds.
        lifetime: u32,
        /// The request's nonce.
        nonce: [u8; 12],
        /// The internal port.
        internal_port: u16,
        /// The assigned external address and port.
        external: SocketAddr,
    },
    /// NAT-PMP external address response.
    Address {
        /// Result code (0 success).
        result: u16,
        /// The gateway's external address.
        ip: Ipv4Addr,
    },
    /// NAT-PMP mapping response.
    Map {
        /// Result code (0 success).
        result: u16,
        /// The internal port.
        internal_port: u16,
        /// The assigned external port.
        external_port: u16,
        /// Granted lifetime, seconds.
        lifetime: u32,
    },
    /// A version-0 answer with "unsupported version" (a NAT-PMP-only gateway refusing PCP).
    UnsupportedVersion,
}

/// Parses a PCP or NAT-PMP response.
pub fn parse_response(b: &[u8]) -> Option<PmpResponse> {
    match *b.first()? {
        PCP_VERSION => {
            if b.len() < 60 || b[1] != (0x80 | PCP_OPCODE_MAP) {
                return None;
            }
            Some(PmpResponse::Pcp {
                result: b[3],
                lifetime: u32::from_be_bytes(b[4..8].try_into().ok()?),
                nonce: b[24..36].try_into().ok()?,
                internal_port: u16::from_be_bytes([b[40], b[41]]),
                external: SocketAddr::new(
                    from_pcp_addr(b[44..60].try_into().ok()?),
                    u16::from_be_bytes([b[42], b[43]]),
                ),
            })
        }
        0 => {
            let op = *b.get(1)?;
            let result = u16::from_be_bytes([*b.get(2)?, *b.get(3)?]);
            match op {
                0x80 if b.len() >= 12 => Some(PmpResponse::Address {
                    result,
                    ip: Ipv4Addr::new(b[8], b[9], b[10], b[11]),
                }),
                0x81 if b.len() >= 16 => Some(PmpResponse::Map {
                    result,
                    internal_port: u16::from_be_bytes([b[8], b[9]]),
                    external_port: u16::from_be_bytes([b[10], b[11]]),
                    lifetime: u32::from_be_bytes(b[12..16].try_into().ok()?),
                }),
                // A NAT-PMP server answering an unknown version echoes the opcode with
                // the response bit and result 1.
                _ if result == u16::from(UNSUPPORTED_VERSION) => {
                    Some(PmpResponse::UnsupportedVersion)
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// How hard to try.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retry {
    /// First wait for an answer; doubles per attempt (RFC 6886: 250 ms).
    pub initial: Duration,
    /// Attempts per request (RFC 6886: 9, i.e. up to 64 s; games want 3).
    pub attempts: u32,
}

impl Default for Retry {
    fn default() -> Self {
        Self {
            initial: Duration::from_millis(250),
            attempts: 3,
        }
    }
}

fn exchange(
    socket: &UdpSocket,
    gateway: SocketAddr,
    request: &[u8],
    retry: Retry,
    accept: &mut dyn FnMut(PmpResponse) -> bool,
) -> Result<PmpResponse, NatError> {
    let mut wait = retry.initial;
    let mut buf = [0u8; 1100];
    for _ in 0..retry.attempts {
        socket.send_to(request, gateway)?;
        let deadline = Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            socket.set_read_timeout(Some(left))?;
            match socket.recv_from(&mut buf) {
                Ok((n, from)) => {
                    if from.ip() != gateway.ip() || from.port() != gateway.port() {
                        continue;
                    }
                    if let Some(r) = parse_response(&buf[..n])
                        && accept(r)
                    {
                        return Ok(r);
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    break;
                }
                // An ICMP "port unreachable": nothing listens on the gateway.
                Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {
                    return Err(NatError::NoAnswer);
                }
                Err(e) => return Err(e.into()),
            }
        }
        wait *= 2;
    }
    Err(NatError::NoAnswer)
}

fn socket_for(local: IpAddr) -> Result<UdpSocket, NatError> {
    Ok(UdpSocket::bind(SocketAddr::new(local, 0))?)
}

/// Maps UDP `internal_port` on the gateway, PCP first, then NAT-PMP. `previous` renews an
/// existing mapping (same protocol, nonce and port).
pub fn map(
    gateway: SocketAddr,
    local: IpAddr,
    internal_port: u16,
    lifetime: Duration,
    retry: Retry,
    previous: Option<&PmpMapping>,
) -> Result<PmpMapping, NatError> {
    let socket = socket_for(local)?;
    let seconds = lifetime.as_secs().clamp(1, u64::from(u32::MAX)) as u32;
    let suggested = previous.map_or(internal_port, |p| p.external.port());
    if previous.is_none_or(|p| p.protocol == PmpProtocol::Pcp) {
        let mut nonce = [0u8; 12];
        match previous {
            Some(p) => nonce = p.nonce,
            None => random_bytes(&mut nonce),
        }
        let request = pcp_map_request(local, &nonce, internal_port, suggested, seconds);
        match exchange(&socket, gateway, &request, retry, &mut |r| match r {
            PmpResponse::Pcp { nonce: n, .. } => n == nonce,
            PmpResponse::UnsupportedVersion => true,
            _ => false,
        }) {
            Ok(PmpResponse::Pcp {
                result: 0,
                lifetime,
                external,
                ..
            }) => {
                return Ok(PmpMapping {
                    protocol: PmpProtocol::Pcp,
                    external,
                    lifetime: Duration::from_secs(u64::from(lifetime)),
                    internal_port,
                    nonce,
                    local,
                });
            }
            Ok(PmpResponse::Pcp { result, .. }) => {
                return Err(NatError::Refused(u16::from(result)));
            }
            // Unsupported version: a NAT-PMP-only gateway (RFC 6887 §9), NAT-PMP below.
            Ok(_) => {}
            // Silence: neither protocol (NAT-PMP servers answer unknown versions).
            Err(e) => return Err(e),
        }
    }
    // NAT-PMP: the external address, then the mapping.
    let address = exchange(&socket, gateway, &[0, NATPMP_OP_ADDRESS], retry, &mut |r| {
        matches!(r, PmpResponse::Address { .. })
    })?;
    let PmpResponse::Address { result: 0, ip } = address else {
        return Err(NatError::Refused(match address {
            PmpResponse::Address { result, .. } => result,
            _ => 0,
        }));
    };
    let mut request = [0u8; 12];
    request[1] = NATPMP_OP_MAP_UDP;
    request[4..6].copy_from_slice(&internal_port.to_be_bytes());
    request[6..8].copy_from_slice(&suggested.to_be_bytes());
    request[8..12].copy_from_slice(&seconds.to_be_bytes());
    match exchange(
        &socket,
        gateway,
        &request,
        retry,
        &mut |r| matches!(r, PmpResponse::Map { internal_port: p, .. } if p == internal_port),
    )? {
        PmpResponse::Map {
            result: 0,
            external_port,
            lifetime,
            ..
        } => Ok(PmpMapping {
            protocol: PmpProtocol::NatPmp,
            external: SocketAddr::new(IpAddr::V4(ip), external_port),
            lifetime: Duration::from_secs(u64::from(lifetime)),
            internal_port,
            nonce: [0; 12],
            local,
        }),
        PmpResponse::Map { result, .. } => Err(NatError::Refused(result)),
        _ => Err(NatError::Malformed),
    }
}

/// Removes a mapping (lifetime 0).
pub fn unmap(gateway: SocketAddr, mapping: &PmpMapping, retry: Retry) -> Result<(), NatError> {
    let socket = socket_for(mapping.local)?;
    match mapping.protocol {
        PmpProtocol::Pcp => {
            let request =
                pcp_map_request(mapping.local, &mapping.nonce, mapping.internal_port, 0, 0);
            exchange(
                &socket,
                gateway,
                &request,
                retry,
                &mut |r| matches!(r, PmpResponse::Pcp { nonce, .. } if nonce == mapping.nonce),
            )?;
        }
        PmpProtocol::NatPmp => {
            let mut request = [0u8; 12];
            request[1] = NATPMP_OP_MAP_UDP;
            request[4..6].copy_from_slice(&mapping.internal_port.to_be_bytes());
            exchange(
                &socket,
                gateway,
                &request,
                retry,
                &mut |r| matches!(r, PmpResponse::Map { internal_port, .. } if internal_port == mapping.internal_port),
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn pcp_request_layout() {
        let r = pcp_map_request("192.168.1.5".parse().unwrap(), &[7; 12], 27015, 27015, 7200);
        assert_eq!(r[0], 2);
        assert_eq!(r[1], 1);
        assert_eq!(u32::from_be_bytes(r[4..8].try_into().unwrap()), 7200);
        assert_eq!(
            &r[8..24],
            &"::ffff:192.168.1.5".parse::<Ipv6Addr>().unwrap().octets()
        );
        assert_eq!(&r[24..36], &[7; 12]);
        assert_eq!(r[36], 17);
        assert_eq!(u16::from_be_bytes([r[40], r[41]]), 27015);
        assert_eq!(
            &r[44..60],
            &"::ffff:0.0.0.0".parse::<Ipv6Addr>().unwrap().octets()
        );
    }

    #[test]
    fn natpmp_responses_parse() {
        let mut b = [0u8; 16];
        b[1] = 0x81;
        b[8..10].copy_from_slice(&27015u16.to_be_bytes());
        b[10..12].copy_from_slice(&40000u16.to_be_bytes());
        b[12..16].copy_from_slice(&3600u32.to_be_bytes());
        assert_eq!(
            parse_response(&b),
            Some(PmpResponse::Map {
                result: 0,
                internal_port: 27015,
                external_port: 40000,
                lifetime: 3600
            })
        );
        assert_eq!(
            parse_response(&[0, 0x81, 0, 1]),
            Some(PmpResponse::UnsupportedVersion)
        );
    }
}
