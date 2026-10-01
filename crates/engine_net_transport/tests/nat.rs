//! NAT traversal against mock routers (UPnP IGD over SSDP + HTTP/SOAP, PCP, NAT-PMP), STUN
//! through the endpoint, and a full co-op join through two NATs by hole punching.

#![allow(clippy::unwrap_used, clippy::expect_used)]
// Real sockets and mock routers on other threads: plain sleeps are fine here.
#![allow(clippy::disallowed_methods)]

use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, UdpSocket as StdUdp};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use engine_net_transport::memory::{MemoryNetwork, NatBehavior};
use engine_net_transport::nat::punch::{LocalRendezvous, PunchState, Puncher, Rendezvous};
use engine_net_transport::nat::{
    self, MappingStatus, Method, NatConfig, NatSession, PortMapper, pmp, stun,
};
use engine_net_transport::*;

fn addr(s: &str) -> SocketAddr {
    s.parse().unwrap()
}

fn wait_for(
    mapper: &PortMapper,
    what: impl Fn(&MappingStatus) -> bool,
    secs: u64,
) -> MappingStatus {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let s = mapper.status();
        if what(&s) || Instant::now() > deadline {
            return s;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A UPnP IGD: SSDP responder plus an HTTP server for the description and SOAP actions.
struct MockIgd {
    ssdp: SocketAddr,
    calls: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
}

impl MockIgd {
    fn start(only_permanent: bool) -> Self {
        let http = TcpListener::bind("127.0.0.1:0").unwrap();
        let http_addr = http.local_addr().unwrap();
        let ssdp = StdUdp::bind("127.0.0.1:0").unwrap();
        let ssdp_addr = ssdp.local_addr().unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        {
            let stop = stop.clone();
            ssdp.set_read_timeout(Some(Duration::from_millis(50)))
                .unwrap();
            std::thread::spawn(move || {
                let mut buf = [0u8; 2048];
                while !stop.load(Ordering::Relaxed) {
                    if let Ok((n, from)) = ssdp.recv_from(&mut buf) {
                        let req = String::from_utf8_lossy(&buf[..n]);
                        if req.starts_with("M-SEARCH") && req.contains("InternetGatewayDevice:1") {
                            let resp = format!(
                                "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=120\r\nST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\nUSN: uuid:mock::igd\r\nLOCATION: http://{http_addr}/rootDesc.xml\r\n\r\n"
                            );
                            ssdp.send_to(resp.as_bytes(), from).unwrap();
                        }
                    }
                }
            });
        }
        {
            let calls = calls.clone();
            let stop = stop.clone();
            http.set_nonblocking(true).unwrap();
            std::thread::spawn(move || {
                let mut lease_zero_only = only_permanent;
                while !stop.load(Ordering::Relaxed) {
                    let Ok((mut stream, _)) = http.accept() else {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    };
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let mut req = Vec::new();
                    let mut buf = [0u8; 4096];
                    loop {
                        let n = stream.read(&mut buf).unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        req.extend_from_slice(&buf[..n]);
                        let text = String::from_utf8_lossy(&req);
                        if let Some(head_end) = text.find("\r\n\r\n") {
                            let len = text[..head_end]
                                .lines()
                                .find_map(|l| {
                                    l.to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                        .map(|v| v.trim().parse::<usize>().unwrap())
                                })
                                .unwrap_or(0);
                            if req.len() >= head_end + 4 + len {
                                break;
                            }
                        }
                    }
                    let text = String::from_utf8_lossy(&req).into_owned();
                    let (status, body) = if text.starts_with("GET /rootDesc.xml") {
                        // Chunked, as some routers send it.
                        (
                            200,
                            String::from(
                                "<?xml version=\"1.0\"?><root xmlns=\"urn:schemas-upnp-org:device-1-0\"><device><deviceList><device><deviceList><device>\
                             <serviceList><service><serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType>\
                             <controlURL>/ctl/IPConn</controlURL></service></serviceList></device></deviceList></device></deviceList></device></root>",
                            ),
                        )
                    } else {
                        let action = text
                            .split("SOAPAction: \"")
                            .nth(1)
                            .and_then(|s| s.split('#').nth(1))
                            .and_then(|s| s.split('"').next())
                            .unwrap_or("")
                            .to_string();
                        let arg = |name: &str| {
                            text.split(&format!("<{name}>"))
                                .nth(1)
                                .and_then(|s| s.split('<').next())
                                .unwrap_or("")
                                .to_string()
                        };
                        match action.as_str() {
                            "GetExternalIPAddress" => (
                                200,
                                String::from(
                                    "<s:Envelope><s:Body><u:GetExternalIPAddressResponse><NewExternalIPAddress>203.0.113.77</NewExternalIPAddress></u:GetExternalIPAddressResponse></s:Body></s:Envelope>",
                                ),
                            ),
                            "AddPortMapping" => {
                                let lease = arg("NewLeaseDuration");
                                calls.lock().unwrap().push(format!(
                                    "add {} {} {} {}",
                                    arg("NewExternalPort"),
                                    arg("NewInternalPort"),
                                    arg("NewInternalClient"),
                                    lease
                                ));
                                if lease_zero_only && lease != "0" {
                                    (
                                        500,
                                        String::from(
                                            "<s:Envelope><s:Body><s:Fault><detail><UPnPError><errorCode>725</errorCode></UPnPError></detail></s:Fault></s:Body></s:Envelope>",
                                        ),
                                    )
                                } else {
                                    lease_zero_only = only_permanent;
                                    (
                                        200,
                                        String::from(
                                            "<s:Envelope><s:Body><u:AddPortMappingResponse/></s:Body></s:Envelope>",
                                        ),
                                    )
                                }
                            }
                            "DeletePortMapping" => {
                                calls
                                    .lock()
                                    .unwrap()
                                    .push(format!("delete {}", arg("NewExternalPort")));
                                (
                                    200,
                                    String::from(
                                        "<s:Envelope><s:Body><u:DeletePortMappingResponse/></s:Body></s:Envelope>",
                                    ),
                                )
                            }
                            _ => (500, String::from("<errorCode>401</errorCode>")),
                        }
                    };
                    let chunked = text.starts_with("GET");
                    let response = if chunked {
                        let (a, b) = body.split_at(body.len() / 2);
                        format!(
                            "HTTP/1.1 {status} OK\r\nTransfer-Encoding: chunked\r\nContent-Type: text/xml\r\n\r\n{:x}\r\n{a}\r\n{:x}\r\n{b}\r\n0\r\n\r\n",
                            a.len(),
                            b.len()
                        )
                    } else {
                        format!(
                            "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nContent-Type: text/xml\r\n\r\n{body}",
                            body.len()
                        )
                    };
                    let _ = stream.write_all(response.as_bytes());
                }
            });
        }
        Self {
            ssdp: ssdp_addr,
            calls,
            stop,
        }
    }
}

impl Drop for MockIgd {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[test]
fn upnp_maps_renews_and_removes_one_port() {
    let igd = MockIgd::start(false);
    let config = NatConfig {
        pcp: false,
        lease: Duration::from_secs(2),
        ssdp_target: igd.ssdp,
        local_ip: Some("127.0.0.1".parse().unwrap()),
        ..NatConfig::default()
    };
    let mapper = PortMapper::start(config, 27015);
    let status = wait_for(&mapper, |s| !matches!(s, MappingStatus::Pending), 5);
    let MappingStatus::Mapped(m) = status else {
        panic!("{status:?}")
    };
    assert_eq!(m.method, Method::Upnp);
    assert_eq!(m.external, addr("203.0.113.77:27015"));
    // Renewed at half the 2 s lease.
    let status = wait_for(
        &mapper,
        |s| matches!(s, MappingStatus::Mapped(m) if m.renewals >= 1),
        5,
    );
    assert!(
        matches!(status, MappingStatus::Mapped(m) if m.renewals >= 1),
        "{status:?}"
    );
    mapper.stop();
    let calls = igd.calls.lock().unwrap().clone();
    assert_eq!(calls[0], "add 27015 27015 127.0.0.1 2");
    assert_eq!(calls[1], "add 27015 27015 127.0.0.1 2");
    assert_eq!(calls.last().unwrap(), "delete 27015");
    assert_eq!(calls.iter().filter(|c| c.starts_with("delete")).count(), 1);
}

#[test]
fn upnp_falls_back_to_a_permanent_mapping_and_still_removes_it() {
    let igd = MockIgd::start(true);
    let config = NatConfig {
        pcp: false,
        ssdp_target: igd.ssdp,
        local_ip: Some("127.0.0.1".parse().unwrap()),
        ..NatConfig::default()
    };
    let mapper = PortMapper::start(config, 30000);
    let status = wait_for(&mapper, |s| !matches!(s, MappingStatus::Pending), 5);
    assert!(
        matches!(status, MappingStatus::Mapped(m) if m.lease.is_zero()),
        "{status:?}"
    );
    drop(mapper);
    let calls = igd.calls.lock().unwrap().clone();
    assert_eq!(
        calls,
        vec![
            "add 30000 30000 127.0.0.1 3600",
            "add 30000 30000 127.0.0.1 0",
            "delete 30000"
        ]
    );
}

/// A PCP server, or a NAT-PMP-only one (answers PCP with "unsupported version").
fn mock_pmp(pcp: bool) -> (SocketAddr, Arc<Mutex<Vec<String>>>, Arc<AtomicBool>) {
    let socket = StdUdp::bind("127.0.0.1:0").unwrap();
    let at = socket.local_addr().unwrap();
    socket
        .set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let (c, s) = (calls.clone(), stop.clone());
    std::thread::spawn(move || {
        let mut buf = [0u8; 1100];
        let mut nonces: Vec<[u8; 12]> = Vec::new();
        while !s.load(Ordering::Relaxed) {
            let Ok((n, from)) = socket.recv_from(&mut buf) else {
                continue;
            };
            let req = &buf[..n];
            if req[0] == 2 {
                if !pcp {
                    socket.send_to(&[0, 0x81, 0, 1, 0, 0, 0, 0], from).unwrap();
                    continue;
                }
                let lifetime = u32::from_be_bytes(req[4..8].try_into().unwrap());
                let nonce: [u8; 12] = req[24..36].try_into().unwrap();
                if !nonces.contains(&nonce) {
                    nonces.push(nonce);
                }
                c.lock().unwrap().push(format!(
                    "pcp {} {} nonce#{}",
                    u16::from_be_bytes([req[40], req[41]]),
                    lifetime,
                    nonces.iter().position(|x| *x == nonce).unwrap()
                ));
                let mut r = [0u8; 60];
                r[0] = 2;
                r[1] = 0x81;
                r[4..8].copy_from_slice(&lifetime.to_be_bytes());
                r[24..60].copy_from_slice(&req[24..60]);
                r[42..44].copy_from_slice(&40_123u16.to_be_bytes());
                r[44..60].copy_from_slice(
                    &"::ffff:203.0.113.88"
                        .parse::<std::net::Ipv6Addr>()
                        .unwrap()
                        .octets(),
                );
                socket.send_to(&r, from).unwrap();
            } else if req[0] == 0 && req[1] == 0 {
                socket
                    .send_to(&[0, 0x80, 0, 0, 0, 0, 0, 1, 203, 0, 113, 99], from)
                    .unwrap();
            } else if req[0] == 0 && req[1] == 1 {
                let internal = u16::from_be_bytes([req[4], req[5]]);
                let lifetime = u32::from_be_bytes(req[8..12].try_into().unwrap());
                c.lock().unwrap().push(format!("pmp {internal} {lifetime}"));
                let mut r = [0u8; 16];
                r[1] = 0x81;
                r[8..10].copy_from_slice(&internal.to_be_bytes());
                r[10..12]
                    .copy_from_slice(&(if lifetime == 0 { 0u16 } else { 40_124 }).to_be_bytes());
                r[12..16].copy_from_slice(&lifetime.to_be_bytes());
                socket.send_to(&r, from).unwrap();
            }
        }
    });
    (at, calls, stop)
}

#[test]
fn pcp_maps_renews_with_the_same_nonce_and_removes() {
    let (gw, calls, stop) = mock_pmp(true);
    let config = NatConfig {
        upnp: false,
        lease: Duration::from_secs(2),
        gateway: Some(gw),
        local_ip: Some("127.0.0.1".parse().unwrap()),
        ..NatConfig::default()
    };
    let mapper = PortMapper::start(config, 27016);
    let status = wait_for(
        &mapper,
        |s| matches!(s, MappingStatus::Mapped(m) if m.renewals >= 1),
        5,
    );
    let MappingStatus::Mapped(m) = status else {
        panic!("{status:?}")
    };
    assert_eq!(
        (m.method, m.external),
        (Method::Pcp, addr("203.0.113.88:40123"))
    );
    mapper.stop();
    stop.store(true, Ordering::Relaxed);
    let calls = calls.lock().unwrap().clone();
    assert_eq!(calls[0], "pcp 27016 2 nonce#0");
    assert_eq!(calls[1], "pcp 27016 2 nonce#0");
    assert_eq!(calls.last().unwrap(), "pcp 27016 0 nonce#0");
}

#[test]
fn nat_pmp_only_gateways_are_used_after_pcp_is_refused() {
    let (gw, calls, stop) = mock_pmp(false);
    let local: IpAddr = "127.0.0.1".parse().unwrap();
    let m = pmp::map(
        gw,
        local,
        27017,
        Duration::from_secs(7200),
        pmp::Retry::default(),
        None,
    )
    .unwrap();
    assert_eq!(m.protocol, pmp::PmpProtocol::NatPmp);
    assert_eq!(m.external, addr("203.0.113.99:40124"));
    pmp::unmap(gw, &m, pmp::Retry::default()).unwrap();
    stop.store(true, Ordering::Relaxed);
    assert_eq!(
        *calls.lock().unwrap(),
        vec!["pmp 27017 7200", "pmp 27017 0"]
    );
}

#[test]
fn nothing_answering_fails_fast() {
    // A port with no server: PCP gets "port unreachable" or silence.
    let silent = StdUdp::bind("127.0.0.1:0").unwrap();
    let config = NatConfig {
        upnp: true,
        discovery_timeout: Duration::from_millis(300),
        ssdp_target: silent.local_addr().unwrap(),
        gateway: Some(silent.local_addr().unwrap()),
        local_ip: Some("127.0.0.1".parse().unwrap()),
        retry: pmp::Retry {
            initial: Duration::from_millis(50),
            attempts: 2,
        },
        ..NatConfig::default()
    };
    let start = Instant::now();
    let mapper = PortMapper::start(config, 1234);
    let status = wait_for(&mapper, |s| !matches!(s, MappingStatus::Pending), 5);
    assert!(matches!(status, MappingStatus::Failed(_)), "{status:?}");
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "{:?}",
        start.elapsed()
    );
}

const PROTOCOL: u64 = 0x0f1e_0a77;

/// STUN through the endpoint's socket: the probe sees the NAT's mapping of the game
/// socket; two servers expose a symmetric NAT.
#[test]
fn stun_through_the_endpoint() {
    for behavior in [NatBehavior::PortRestricted, NatBehavior::Symmetric] {
        let net = MemoryNetwork::new(4);
        let home = net.add_nat("198.51.100.20".parse().unwrap(), behavior);
        let mut servers = [
            net.bind(addr("203.0.113.1:3478")),
            net.bind(addr("203.0.113.2:3478")),
        ];
        let t0 = Instant::now();
        let mut host = Endpoint::server(
            net.bind_behind_nat(addr("192.168.0.5:27015"), home),
            EndpointConfig::new(PROTOCOL),
            ServerConfig {
                private_key: Key::random(),
                max_clients: 2,
                public_addresses: vec![],
            },
            t0,
        )
        .unwrap();
        let mut session =
            NatSession::new(&[addr("203.0.113.1:3478"), addr("203.0.113.2:3478")], t0);
        let mut buf = [0u8; 256];
        let mut out = [0u8; 256];
        for ms in (0..1000).step_by(10) {
            let now = t0 + Duration::from_millis(ms);
            for s in &mut servers {
                while let Some((n, from)) = s.recv_from(now, &mut buf).unwrap() {
                    let m = stun::answer_binding(&buf[..n], from, &mut out).unwrap();
                    s.send_to(now, from, &out[..m]).unwrap();
                }
            }
            host.update(now);
            session.service(&mut host, now);
            if session.stun_done(now) {
                break;
            }
        }
        let r = session.stun_result().unwrap();
        assert_eq!(r.answers, 2);
        assert_eq!(
            r.mapped.unwrap().ip(),
            "198.51.100.20".parse::<IpAddr>().unwrap()
        );
        assert_eq!(r.mapping_varies, behavior == NatBehavior::Symmetric);
        let report = nat::decide(addr("192.168.0.5:27015"), None, Some(&r), false);
        let expected = if behavior == NatBehavior::Symmetric {
            Method::LanOnly
        } else {
            Method::HolePunch
        };
        assert_eq!(report.method, expected);
    }
}

/// The whole co-op join: host and player each behind a home NAT (port-restricted, the
/// common kind), no port mapping available. STUN finds the host's mapping, the
/// rendezvous carries candidates, both punch, the player connects through the hole.
#[test]
fn coop_join_through_two_nats_by_hole_punching() {
    assert!(coop_join(true), "the player did not get through");
    // Control: without the host punching toward the player, its NAT drops the requests.
    assert!(
        !coop_join(false),
        "joined without punching: the test proves nothing"
    );
}

fn coop_join(host_punches: bool) -> bool {
    let net = MemoryNetwork::new(8);
    let host_nat = net.add_nat("198.51.100.1".parse().unwrap(), NatBehavior::PortRestricted);
    let player_nat = net.add_nat("198.51.100.2".parse().unwrap(), NatBehavior::PortRestricted);
    let mut stun_server = net.bind(addr("203.0.113.1:3478"));
    let t0 = Instant::now();
    let key = Key::random();
    let host_lan = addr("192.168.1.10:27015");
    let mut host = Endpoint::server(
        net.bind_behind_nat(host_lan, host_nat),
        EndpointConfig::new(PROTOCOL),
        ServerConfig {
            private_key: key.clone(),
            max_clients: 4,
            public_addresses: vec![],
        },
        t0,
    )
    .unwrap();
    let mut host_nat_session = NatSession::new(&[addr("203.0.113.1:3478")], t0);
    let mut buf = [0u8; 256];
    let mut out = [0u8; 256];
    let mut serve_stun =
        |now: Instant, stun_server: &mut engine_net_transport::memory::MemorySocket| {
            while let Some((n, from)) = stun_server.recv_from(now, &mut buf).unwrap() {
                if let Some(m) = stun::answer_binding(&buf[..n], from, &mut out) {
                    stun_server.send_to(now, from, &out[..m]).unwrap();
                }
            }
        };
    // 1. The host learns its public address.
    let mut now = t0;
    while !host_nat_session.stun_done(now) {
        now += Duration::from_millis(10);
        serve_stun(now, &mut stun_server);
        host.update(now);
        host_nat_session.service(&mut host, now);
    }
    let report = nat::decide(
        host_lan,
        None,
        host_nat_session.stun_result().as_ref(),
        false,
    );
    assert_eq!(report.method, Method::HolePunch);
    host.set_public_addresses(&report.addresses);
    // 2. The host issues the player's token (it is the one admitting players) and posts
    //    its candidates on the rendezvous.
    let token = ConnectToken::generate(
        &TokenParams {
            protocol_id: PROTOCOL,
            client_id: 2,
            expire_seconds: 60,
            timeout_seconds: 10,
            server_addresses: &report.addresses,
            user_data: [0; USER_DATA_BYTES],
        },
        &key,
        unix_now(),
    )
    .unwrap();
    let session_id = 77;
    let mut host_board = LocalRendezvous::new_host();
    let mut player_board = host_board.for_peer();
    host_board.publish(session_id, &AddressList::new(&report.addresses).unwrap());
    // 3. The player learns its own mapping, posts it, and starts connecting.
    let mut player = Endpoint::client(
        net.bind_behind_nat(addr("192.168.7.20:50000"), player_nat),
        EndpointConfig::new(PROTOCOL),
        token.clone(),
        now,
    )
    .unwrap();
    let mut player_nat_session = NatSession::new(&[addr("203.0.113.1:3478")], now);
    let mut posted = false;
    let mut connected = None;
    for _ in 0..800 {
        now += Duration::from_millis(10);
        serve_stun(now, &mut stun_server);
        player.update(now);
        host.update(now);
        player_nat_session.service(&mut player, now);
        host_nat_session.service(&mut host, now);
        if !posted && player_nat_session.stun_done(now) {
            let mine = player_nat_session.stun_result().unwrap().mapped.unwrap();
            player_board.publish(
                session_id,
                &AddressList::new(&[mine, player.local_addr().unwrap()]).unwrap(),
            );
            let theirs = player_board.peer_candidates(session_id).unwrap();
            player_nat_session.punches.push(Puncher::new(
                &token.punch_key(),
                session_id,
                true,
                theirs,
                now,
                Duration::from_secs(5),
            ));
            posted = true;
        }
        if host_punches
            && host_nat_session.punches.is_empty()
            && let Some(theirs) = host_board.peer_candidates(session_id)
        {
            host_nat_session.punches.push(Puncher::new(
                &token.punch_key(),
                session_id,
                false,
                theirs,
                now,
                Duration::from_secs(5),
            ));
        }
        while let Some(e) = player.poll_event() {
            if let Event::Connected(id) = e {
                connected = Some(id);
            }
        }
        while host.poll_event().is_some() {}
        player.flush(now);
        host.flush(now);
        if connected.is_some() {
            break;
        }
    }
    if connected.is_none() {
        return false;
    }
    assert_eq!(host.connection_count(), 1);
    let peer = host.peer_addr(host.connections().next().unwrap()).unwrap();
    assert_eq!(peer.ip(), "198.51.100.2".parse::<IpAddr>().unwrap());
    assert!(matches!(
        player_nat_session.punches[0].state(),
        PunchState::Open(_) | PunchState::Probing
    ));
    true
}
