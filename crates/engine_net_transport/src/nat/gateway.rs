//! The default gateway (where NAT-PMP and PCP live) and the local address on its link
//! (what a port mapping points to), from the routing table (IP Helper `GetBestRoute2`).
//! A routing lookup only: no packet is sent.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// The route to the internet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Route {
    /// The next hop: the home router.
    pub gateway: IpAddr,
    /// This machine's address on the gateway's link.
    pub local: IpAddr,
}

/// The default route for IPv4 (`v6` false) or IPv6.
pub fn default_route(v6: bool) -> io::Result<Route> {
    // A public address for the lookup (documentation ranges would not route).
    let dest: IpAddr = if v6 {
        IpAddr::V6(Ipv6Addr::new(0x2001, 0x4860, 0x4860, 0, 0, 0, 0, 0x8888))
    } else {
        IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))
    };
    sys::best_route(dest)
}

#[cfg(windows)]
mod sys {
    use std::io;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    use windows_sys::Win32::NetworkManagement::IpHelper::{GetBestRoute2, MIB_IPFORWARD_ROW2};
    use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6, SOCKADDR_INET};

    fn to_sockaddr(ip: IpAddr) -> SOCKADDR_INET {
        let mut s = SOCKADDR_INET::default();
        match ip {
            IpAddr::V4(v4) => {
                s.Ipv4.sin_family = AF_INET;
                s.Ipv4.sin_addr.S_un.S_addr = u32::from_ne_bytes(v4.octets());
            }
            IpAddr::V6(v6) => {
                s.Ipv6.sin6_family = AF_INET6;
                s.Ipv6.sin6_addr.u.Byte = v6.octets();
            }
        }
        s
    }

    fn from_sockaddr(s: &SOCKADDR_INET) -> Option<IpAddr> {
        // SAFETY: the family field is shared by every variant; the variant read matches it.
        unsafe {
            match s.si_family {
                AF_INET => Some(IpAddr::V4(Ipv4Addr::from(
                    s.Ipv4.sin_addr.S_un.S_addr.to_ne_bytes(),
                ))),
                AF_INET6 => Some(IpAddr::V6(Ipv6Addr::from(s.Ipv6.sin6_addr.u.Byte))),
                _ => None,
            }
        }
    }

    pub(super) fn best_route(dest: IpAddr) -> io::Result<super::Route> {
        let dest = to_sockaddr(dest);
        let mut row = MIB_IPFORWARD_ROW2::default();
        let mut source = SOCKADDR_INET::default();
        // SAFETY: valid pointers for the call; a null LUID and index 0 let the stack pick
        // the interface.
        let r = unsafe {
            GetBestRoute2(
                std::ptr::null(),
                0,
                std::ptr::null(),
                &dest,
                0,
                &mut row,
                &mut source,
            )
        };
        if r != 0 {
            return Err(io::Error::from_raw_os_error(r as i32));
        }
        let gateway =
            from_sockaddr(&row.NextHop).ok_or_else(|| io::Error::other("no gateway family"))?;
        let local = from_sockaddr(&source).ok_or_else(|| io::Error::other("no source family"))?;
        if gateway.is_unspecified() {
            // On-link route (no router): nothing to map.
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no default gateway",
            ));
        }
        Ok(super::Route { gateway, local })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_route_is_plausible_when_present() {
        // Machines without a network (CI sandboxes) have no route; only check the shape.
        if let Ok(route) = default_route(false) {
            assert!(route.gateway.is_ipv4() && route.local.is_ipv4());
            assert!(!route.local.is_unspecified());
        }
    }
}
