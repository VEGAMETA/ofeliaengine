//! Manual check of the NAT ladder against the real router: map one UDP port with UPnP,
//! verify it, remove it, verify it is gone; then the same with PCP / NAT-PMP. Leaves no
//! mapping behind (it removes what it made even when a verification fails).
//!
//! `cargo run -p engine_net_transport --example nat_probe`

#![allow(clippy::print_stdout, clippy::unwrap_used)]

use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use engine_net_transport::nat::{gateway, is_global, pmp, upnp};

fn main() {
    let socket = UdpSocket::bind("0.0.0.0:0").unwrap();
    let port = socket.local_addr().unwrap().port();
    let route = gateway::default_route(false);
    println!("game socket port {port}; default route {route:?}");
    let Ok(route) = route else { return };

    // UPnP IGD.
    let start = Instant::now();
    match upnp::discover(upnp::SSDP_ADDR, route.local, Duration::from_secs(3)) {
        Ok(gw) => {
            println!("UPnP: found {} in {:?} (local {})", gw.location, start.elapsed(), gw.local);
            let ip = gw.external_ip();
            println!("UPnP: external address {ip:?} (global: {})", ip.as_ref().is_ok_and(|ip| is_global(*ip)));
            let t = Instant::now();
            let added = gw.add_mapping(port, port, Duration::from_secs(120), "ofeliaengine nat_probe");
            println!("UPnP: AddPortMapping {port} -> {port} lease 120 s: {added:?} in {:?}", t.elapsed());
            if added.is_ok() {
                println!("UPnP: GetSpecificPortMappingEntry: {:?}", gw.mapping(port));
                let t = Instant::now();
                println!("UPnP: DeletePortMapping: {:?} in {:?}", gw.delete_mapping(port), t.elapsed());
                println!("UPnP: after delete, entry: {:?} (an error = gone)", gw.mapping(port));
                // The whole table, to be sure nothing of ours is left (other programs'
                // entries are counted, not printed).
                let (mut ours, mut total) = (0, 0);
                while total < 1024 && let Ok(entry) = gw.mapping_at(total) {
                    total += 1;
                    if entry.4.contains("ofeliaengine") {
                        ours += 1;
                    }
                }
                println!("UPnP: table has {total} entries, {ours} of ours");
            }
        }
        Err(e) => println!("UPnP: no gateway ({e}) after {:?}", start.elapsed()),
    }

    // PCP, then NAT-PMP.
    let gw = SocketAddr::new(route.gateway, pmp::SERVER_PORT);
    let start = Instant::now();
    match pmp::map(gw, route.local, port, Duration::from_secs(120), pmp::Retry::default(), None) {
        Ok(m) => {
            println!("{:?}: mapped {port} -> {} for {:?} in {:?}", m.protocol, m.external, m.lifetime, start.elapsed());
            println!("{:?}: unmap {:?}", m.protocol, pmp::unmap(gw, &m, pmp::Retry::default()));
        }
        Err(e) => println!("PCP/NAT-PMP: {e} after {:?}", start.elapsed()),
    }
}
