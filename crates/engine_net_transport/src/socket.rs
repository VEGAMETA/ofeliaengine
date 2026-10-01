//! Datagram sockets the endpoint runs on: real UDP, the in-memory simulator, or anything
//! a game supplies (a relay, Steam Datagram Relay through `ISteamNetworkingFakeUDPPort`).

use std::io;
use std::net::{SocketAddr, UdpSocket as StdUdpSocket};
use std::time::{Duration, Instant};

use crate::bytes::normalize_addr;

/// A non-blocking datagram socket.
///
/// `now` lets simulated sockets run on a virtual clock; real sockets ignore it. Addresses
/// are plain `SocketAddr`s: a backend without IP addresses (a relay, Steam's networking
/// identities) maps its peers to stable fake addresses, as Steamworks' FakeIP does.
pub trait DatagramSocket {
    /// Sends one datagram. A full send buffer may drop it silently (UDP semantics).
    fn send_to(&mut self, now: Instant, to: SocketAddr, data: &[u8]) -> io::Result<()>;
    /// Receives one datagram if one is waiting; never blocks.
    fn recv_from(&mut self, now: Instant, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>>;
    /// The address the socket is bound to.
    fn local_addr(&self) -> io::Result<SocketAddr>;
}

/// A UDP socket set up for games on Windows:
/// - non-blocking;
/// - `SIO_UDP_CONNRESET` off: otherwise an ICMP "port unreachable" from one departed peer
///   makes the next `recvfrom` fail with `WSAECONNRESET`, for every peer of a server;
/// - 1 MiB send and receive buffers (a server's burst of snapshots, a client's frame hitch);
/// - an IPv6 wildcard address binds dual-stack (IPv4 peers arrive as IPv4).
#[derive(Debug)]
pub struct UdpSocket {
    socket: StdUdpSocket,
}

impl UdpSocket {
    /// Binds to `addr` (`0.0.0.0:0` for any port, `[::]:port` for dual-stack).
    pub fn bind(addr: SocketAddr) -> io::Result<Self> {
        let socket = if addr.is_ipv6() && addr.ip().is_unspecified() {
            sys::bind_dual_stack(addr.port())?
        } else {
            StdUdpSocket::bind(addr)?
        };
        socket.set_nonblocking(true)?;
        sys::configure(&socket)?;
        Ok(Self { socket })
    }

    /// The underlying socket.
    pub fn as_std(&self) -> &StdUdpSocket {
        &self.socket
    }

    /// Waits until a datagram is readable or `timeout` passes (a dedicated server's idle
    /// wait between ticks, or an I/O thread). Returns whether one is readable.
    pub fn wait_readable(&self, timeout: Duration) -> io::Result<bool> {
        sys::wait_readable(&self.socket, timeout)
    }
}

impl DatagramSocket for UdpSocket {
    fn send_to(&mut self, _now: Instant, to: SocketAddr, data: &[u8]) -> io::Result<()> {
        let to = match (to, self.socket.local_addr()) {
            // A dual-stack socket sends to IPv4 peers through mapped addresses.
            (SocketAddr::V4(v4), Ok(SocketAddr::V6(_))) => {
                SocketAddr::new(v4.ip().to_ipv6_mapped().into(), v4.port())
            }
            _ => to,
        };
        match self.socket.send_to(data, to) {
            Ok(_) => Ok(()),
            // A full buffer drops the datagram, as the network would.
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn recv_from(&mut self, _now: Instant, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        loop {
            match self.socket.recv_from(buf) {
                Ok((n, from)) => return Ok(Some((n, normalize_addr(from)))),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                // Leftovers of ICMP errors and oversized foreign datagrams (WSAEMSGSIZE):
                // skip them, the next datagram is unaffected.
                Err(e)
                    if e.kind() == io::ErrorKind::ConnectionReset
                        || e.raw_os_error() == Some(sys::WSAEMSGSIZE) =>
                {
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr().map(normalize_addr)
    }
}

#[cfg(windows)]
mod sys {
    use std::io;
    use std::net::UdpSocket;
    use std::os::windows::io::{AsRawSocket, FromRawSocket};
    use std::time::Duration;

    use windows_sys::Win32::Networking::WinSock::{
        AF_INET6, IN6_ADDR, INVALID_SOCKET, IPPROTO_IPV6, IPPROTO_UDP, IPV6_V6ONLY, POLLRDNORM,
        SIO_UDP_CONNRESET, SOCK_DGRAM, SOCKADDR, SOCKADDR_IN6, SOCKET, SOL_SOCKET, SO_RCVBUF,
        SO_SNDBUF, WSAIoctl, WSAPOLLFD, WSAPoll, bind, closesocket, setsockopt, socket,
    };

    pub(super) const WSAEMSGSIZE: i32 = 10040;
    const BUFFER_BYTES: i32 = 1 << 20;

    fn last_error() -> io::Error {
        io::Error::last_os_error()
    }

    pub(super) fn configure(socket: &UdpSocket) -> io::Result<()> {
        let s = socket.as_raw_socket() as SOCKET;
        let off: u32 = 0;
        let mut returned = 0u32;
        // SAFETY: a valid socket; the input is a 4-byte BOOL as SIO_UDP_CONNRESET expects;
        // synchronous call (no overlapped structure).
        let r = unsafe {
            WSAIoctl(
                s,
                SIO_UDP_CONNRESET,
                std::ptr::from_ref(&off).cast(),
                4,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
                None,
            )
        };
        if r != 0 {
            return Err(last_error());
        }
        for option in [SO_RCVBUF, SO_SNDBUF] {
            // SAFETY: a valid socket and a 4-byte int option value.
            let r = unsafe {
                setsockopt(s, SOL_SOCKET, option, std::ptr::from_ref(&BUFFER_BYTES).cast(), 4)
            };
            if r != 0 {
                return Err(last_error());
            }
        }
        Ok(())
    }

    pub(super) fn bind_dual_stack(port: u16) -> io::Result<UdpSocket> {
        // std's UdpSocket has no way to clear IPV6_V6ONLY before binding: create the
        // socket here (Winsock is initialized by std's networking on first use).
        let _ = UdpSocket::bind("127.0.0.1:0");
        // SAFETY: plain socket creation.
        let s = unsafe { socket(i32::from(AF_INET6), SOCK_DGRAM, IPPROTO_UDP) };
        if s == INVALID_SOCKET {
            return Err(last_error());
        }
        // SAFETY: `s` is a socket we own; on every error path it is closed once.
        unsafe {
            let off: i32 = 0;
            if setsockopt(s, IPPROTO_IPV6, IPV6_V6ONLY, std::ptr::from_ref(&off).cast(), 4) != 0 {
                let e = last_error();
                closesocket(s);
                return Err(e);
            }
            let addr = SOCKADDR_IN6 {
                sin6_family: AF_INET6,
                sin6_port: port.to_be(),
                sin6_flowinfo: 0,
                sin6_addr: IN6_ADDR::default(),
                Anonymous: Default::default(),
            };
            if bind(s, std::ptr::from_ref(&addr).cast::<SOCKADDR>(), size_of::<SOCKADDR_IN6>() as i32) != 0 {
                let e = last_error();
                closesocket(s);
                return Err(e);
            }
            Ok(UdpSocket::from_raw_socket(s as u64))
        }
    }

    pub(super) fn wait_readable(socket: &UdpSocket, timeout: Duration) -> io::Result<bool> {
        let mut fd = WSAPOLLFD {
            fd: socket.as_raw_socket() as SOCKET,
            events: POLLRDNORM,
            revents: 0,
        };
        let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        // SAFETY: one valid poll descriptor.
        let r = unsafe { WSAPoll(&mut fd, 1, ms) };
        if r < 0 {
            return Err(last_error());
        }
        Ok(r > 0)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn loopback_datagrams_and_dual_stack() {
        let now = Instant::now();
        let mut a = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let mut b = UdpSocket::bind("[::]:0".parse().unwrap()).unwrap();
        let b_port = b.local_addr().unwrap().port();
        let to_b: SocketAddr = format!("127.0.0.1:{b_port}").parse().unwrap();
        a.send_to(now, to_b, b"over ipv4").unwrap();
        assert!(b.wait_readable(Duration::from_secs(2)).unwrap());
        let mut buf = [0u8; 64];
        let (n, from) = b.recv_from(now, &mut buf).unwrap().unwrap();
        assert_eq!(&buf[..n], b"over ipv4");
        assert_eq!(from, a.local_addr().unwrap());
        // Reply through the dual-stack socket to the IPv4 peer.
        b.send_to(now, from, b"back").unwrap();
        assert!(a.wait_readable(Duration::from_secs(2)).unwrap());
        let (n, _) = a.recv_from(now, &mut buf).unwrap().unwrap();
        assert_eq!(&buf[..n], b"back");
        assert!(a.recv_from(now, &mut buf).unwrap().is_none());
    }

    #[test]
    fn unreachable_peers_do_not_break_receiving() {
        let now = Instant::now();
        let mut a = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let gone = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let gone_addr = gone.local_addr().unwrap();
        drop(gone);
        // ICMP port unreachable comes back; without SIO_UDP_CONNRESET off the next
        // receive would fail.
        a.send_to(now, gone_addr, b"x").unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let mut b = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        b.send_to(now, a.local_addr().unwrap(), b"still here").unwrap();
        assert!(a.wait_readable(Duration::from_secs(2)).unwrap());
        let mut buf = [0u8; 64];
        let (n, _) = a.recv_from(now, &mut buf).unwrap().unwrap();
        assert_eq!(&buf[..n], b"still here");
    }
}
