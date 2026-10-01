//! UPnP Internet Gateway Device port mapping: SSDP discovery, the device description,
//! and the WANIPConnection / WANPPPConnection SOAP actions (GetExternalIPAddress,
//! AddPortMapping, GetSpecificPortMappingEntry, DeletePortMapping).
//!
//! Written here instead of using `igd-next` (MIT): it pulls in an HTTP client, an XML
//! tree, URL parsing and `rand` (or tokio and hyper for async) for what is three HTTP
//! requests with fixed XML (ADR 0070). Blocking, with timeouts; runs on the port mapper's
//! thread, never on the game's.

use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, UdpSocket};
use std::time::{Duration, Instant};

use super::NatError;

/// The SSDP multicast address.
pub const SSDP_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::new(239, 255, 255, 250)), 1900);
/// Largest HTTP response read (descriptions are a few KiB).
const MAX_RESPONSE: usize = 256 * 1024;
const SEARCH_TARGETS: [&str; 2] = [
    "urn:schemas-upnp-org:device:InternetGatewayDevice:1",
    "urn:schemas-upnp-org:device:InternetGatewayDevice:2",
];
const SERVICES: [&str; 3] = [
    "urn:schemas-upnp-org:service:WANIPConnection:2",
    "urn:schemas-upnp-org:service:WANIPConnection:1",
    "urn:schemas-upnp-org:service:WANPPPConnection:1",
];
/// UPnP error: the external port is mapped to another client.
pub const ERROR_CONFLICT: u16 = 718;
/// UPnP error: the gateway only supports permanent (lease 0) mappings.
pub const ERROR_ONLY_PERMANENT: u16 = 725;

/// An `http://host:port/path` URL.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Url {
    addr: SocketAddr,
    host: String,
    path: String,
}

fn parse_url(url: &str) -> Option<Url> {
    let rest = url.strip_prefix("http://").or_else(|| url.strip_prefix("HTTP://"))?;
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let addr: SocketAddr = if host.contains(':') && !host.starts_with('[') {
        host.parse().ok()?
    } else if host.starts_with('[') {
        host.parse().ok().or_else(|| format!("{host}:80").parse().ok())?
    } else {
        format!("{host}:80").parse().ok()?
    };
    Some(Url {
        addr,
        host: host.to_string(),
        path: path.to_string(),
    })
}

fn join_url(base: &Url, reference: &str) -> Option<Url> {
    if reference.starts_with("http://") || reference.starts_with("HTTP://") {
        return parse_url(reference);
    }
    let path = if reference.starts_with('/') {
        reference.to_string()
    } else {
        let dir = base.path.rfind('/').map_or("/", |i| &base.path[..=i]);
        format!("{dir}{reference}")
    };
    Some(Url { path, ..base.clone() })
}

/// One HTTP/1.1 request with `Connection: close`; returns status and body.
fn http(url: &Url, method: &str, headers: &str, body: &str, timeout: Duration) -> Result<(u16, String), NatError> {
    let mut stream = TcpStream::connect_timeout(&url.addr, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let request = format!(
        "{method} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n{body}",
        url.path,
        url.host,
        body.len()
    );
    stream.write_all(request.as_bytes())?;
    let mut response = Vec::new();
    let deadline = Instant::now() + timeout;
    let mut chunk = [0u8; 4096];
    loop {
        if Instant::now() > deadline {
            return Err(NatError::NoAnswer);
        }
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                response.extend_from_slice(&chunk[..n]);
                if response.len() > MAX_RESPONSE {
                    return Err(NatError::Malformed);
                }
                // Stop at Content-Length when the server keeps the connection open.
                if let Some(total) = complete_length(&response)
                    && response.len() >= total
                {
                    break;
                }
            }
            Err(e) => return Err(e.into()),
        }
    }
    parse_http(&response)
}

/// Total bytes of a response with Content-Length, once its head is complete.
fn complete_length(response: &[u8]) -> Option<usize> {
    let head_end = response.windows(4).position(|w| w == b"\r\n\r\n")? + 4;
    let head = std::str::from_utf8(&response[..head_end]).ok()?;
    let len = header(head, "content-length")?.trim().parse::<usize>().ok()?;
    Some(head_end + len)
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|line| {
        let (k, v) = line.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then_some(v.trim())
    })
}

fn parse_http(response: &[u8]) -> Result<(u16, String), NatError> {
    let head_end = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or(NatError::Malformed)?
        + 4;
    let head = std::str::from_utf8(&response[..head_end]).map_err(|_| NatError::Malformed)?;
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or(NatError::Malformed)?;
    let mut body = response[head_end..].to_vec();
    if header(head, "transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        body = dechunk(&body).ok_or(NatError::Malformed)?;
    } else if let Some(len) = header(head, "content-length").and_then(|v| v.parse::<usize>().ok()) {
        body.truncate(len);
    }
    Ok((status, String::from_utf8_lossy(&body).into_owned()))
}

fn dechunk(mut data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = data.windows(2).position(|w| w == b"\r\n")?;
        let size_text = std::str::from_utf8(&data[..line_end]).ok()?;
        let size = usize::from_str_radix(size_text.split(';').next()?.trim(), 16).ok()?;
        data = &data[line_end + 2..];
        if size == 0 {
            return Some(out);
        }
        out.extend_from_slice(data.get(..size)?);
        data = data.get(size + 2..)?;
    }
}

/// The text of the first `<tag>` (any namespace prefix), unescaped.
fn xml_text<'a>(xml: &'a str, tag: &str) -> Option<String> {
    let mut at = 0;
    while let Some(open) = xml[at..].find('<') {
        let start = at + open + 1;
        let end = start + xml[start..].find('>')?;
        let name = &xml[start..end];
        let local = name.split_whitespace().next()?.rsplit(':').next()?;
        if local == tag && !name.starts_with('/') && !name.ends_with('/') {
            let content_start = end + 1;
            let close = xml[content_start..].find("</")? + content_start;
            return Some(unescape(xml[content_start..close].trim()));
        }
        at = end + 1;
    }
    None
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Finds the control URL and service type of a WAN connection service in a device
/// description.
fn find_service(description: &str, base: &Url) -> Option<(Url, String)> {
    let url_base = xml_text(description, "URLBase").and_then(|u| parse_url(&u));
    let base = url_base.as_ref().unwrap_or(base);
    for wanted in SERVICES {
        for block in description.split("</service>") {
            let Some(start) = block.rfind("<service>") else { continue };
            let block = &block[start..];
            if xml_text(block, "serviceType").as_deref() == Some(wanted) {
                let control = xml_text(block, "controlURL")?;
                return Some((join_url(base, &control)?, wanted.to_string()));
            }
        }
    }
    None
}

/// An Internet Gateway Device's WAN connection service.
#[derive(Clone, Debug)]
pub struct Gateway {
    control: Url,
    service: String,
    /// This machine's address toward the gateway (what mappings point to).
    pub local: IpAddr,
    /// The device description's URL.
    pub location: String,
    timeout: Duration,
}

/// Searches for an IGD with SSDP (`target` is [`SSDP_ADDR`] on a real network), from
/// the local address `local`, for up to `timeout`. The first device with a WAN
/// connection service wins.
pub fn discover(target: SocketAddr, local: IpAddr, timeout: Duration) -> Result<Gateway, NatError> {
    let socket = UdpSocket::bind(SocketAddr::new(local, 0))?;
    if target.ip().is_multicast() {
        let _ = socket.set_multicast_ttl_v4(2);
    }
    let deadline = Instant::now() + timeout;
    let mut buf = [0u8; 2048];
    let mut tried: Vec<String> = Vec::new();
    let mut sends = 0;
    let mut next_send = Instant::now();
    let mut last_error = NatError::NoAnswer;
    while Instant::now() < deadline {
        if Instant::now() >= next_send && sends < 3 {
            for st in SEARCH_TARGETS {
                let msg = format!(
                    "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: {st}\r\n\r\n"
                );
                let _ = socket.send_to(msg.as_bytes(), target);
            }
            sends += 1;
            next_send = Instant::now() + Duration::from_millis(500);
        }
        let wait = deadline
            .min(next_send)
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(1));
        socket.set_read_timeout(Some(wait))?;
        let (n, _) = match socket.recv_from(&mut buf) {
            Ok(r) => r,
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => continue,
            Err(e) => return Err(e.into()),
        };
        let Ok(text) = std::str::from_utf8(&buf[..n]) else { continue };
        let Some(location) = header(text, "location") else { continue };
        if tried.iter().any(|t| t == location) {
            continue;
        }
        tried.push(location.to_string());
        let Some(url) = parse_url(location) else { continue };
        let left = deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(500));
        match http(&url, "GET", "", "", left.min(Duration::from_secs(3))) {
            Ok((200, description)) => {
                if let Some((control, service)) = find_service(&description, &url) {
                    // The address the gateway sees us at: the source of a connection to it.
                    let local = local_toward(control.addr).unwrap_or(local);
                    return Ok(Gateway {
                        control,
                        service,
                        local,
                        location: location.to_string(),
                        timeout: Duration::from_secs(3),
                    });
                }
                last_error = NatError::Unsupported;
            }
            Ok((status, _)) => last_error = NatError::Http(status),
            Err(e) => last_error = e,
        }
    }
    Err(last_error)
}

/// The local address the stack uses to reach `addr`.
fn local_toward(addr: SocketAddr) -> Option<IpAddr> {
    let probe = UdpSocket::bind(SocketAddr::new(
        if addr.is_ipv4() { IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED) } else { IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED) },
        0,
    ))
    .ok()?;
    probe.connect(addr).ok()?;
    let ip = probe.local_addr().ok()?.ip();
    (!ip.is_unspecified()).then_some(ip)
}

impl Gateway {
    fn soap(&self, action: &str, args: &str) -> Result<String, NatError> {
        let body = format!(
            "<?xml version=\"1.0\"?>\r\n<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
             s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body>\
             <u:{action} xmlns:u=\"{}\">{args}</u:{action}></s:Body></s:Envelope>\r\n",
            self.service
        );
        let headers = format!(
            "Content-Type: text/xml; charset=\"utf-8\"\r\nSOAPAction: \"{}#{action}\"\r\n",
            self.service
        );
        let (status, response) = http(&self.control, "POST", &headers, &body, self.timeout)?;
        if status == 200 {
            return Ok(response);
        }
        match xml_text(&response, "errorCode").and_then(|c| c.parse::<u16>().ok()) {
            Some(code) => Err(NatError::Refused(code)),
            None => Err(NatError::Http(status)),
        }
    }

    /// The gateway's external (WAN) address.
    pub fn external_ip(&self) -> Result<IpAddr, NatError> {
        let response = self.soap("GetExternalIPAddress", "")?;
        xml_text(&response, "NewExternalIPAddress")
            .and_then(|ip| ip.parse().ok())
            .ok_or(NatError::Malformed)
    }

    /// Maps external UDP `external_port` to `internal_port` on this machine for `lease`
    /// (0: permanent, for gateways that refuse leases).
    pub fn add_mapping(&self, external_port: u16, internal_port: u16, lease: Duration, description: &str) -> Result<(), NatError> {
        let args = format!(
            "<NewRemoteHost></NewRemoteHost><NewExternalPort>{external_port}</NewExternalPort>\
             <NewProtocol>UDP</NewProtocol><NewInternalPort>{internal_port}</NewInternalPort>\
             <NewInternalClient>{}</NewInternalClient><NewEnabled>1</NewEnabled>\
             <NewPortMappingDescription>{}</NewPortMappingDescription>\
             <NewLeaseDuration>{}</NewLeaseDuration>",
            self.local,
            escape(description),
            lease.as_secs()
        );
        self.soap("AddPortMapping", &args).map(|_| ())
    }

    /// The internal client and port an external UDP port maps to.
    pub fn mapping(&self, external_port: u16) -> Result<(IpAddr, u16), NatError> {
        let args = format!(
            "<NewRemoteHost></NewRemoteHost><NewExternalPort>{external_port}</NewExternalPort><NewProtocol>UDP</NewProtocol>"
        );
        let response = self.soap("GetSpecificPortMappingEntry", &args)?;
        let client = xml_text(&response, "NewInternalClient").and_then(|c| c.parse().ok());
        let port = xml_text(&response, "NewInternalPort").and_then(|p| p.parse().ok());
        client.zip(port).ok_or(NatError::Malformed)
    }

    /// Removes the mapping of external UDP `external_port`.
    pub fn delete_mapping(&self, external_port: u16) -> Result<(), NatError> {
        let args = format!(
            "<NewRemoteHost></NewRemoteHost><NewExternalPort>{external_port}</NewExternalPort><NewProtocol>UDP</NewProtocol>"
        );
        self.soap("DeletePortMapping", &args).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn urls_and_xml() {
        let base = parse_url("http://192.168.1.1:5000/rootDesc.xml").unwrap();
        assert_eq!(base.addr, "192.168.1.1:5000".parse().unwrap());
        assert_eq!(join_url(&base, "/ctl/IPConn").unwrap().path, "/ctl/IPConn");
        assert_eq!(join_url(&base, "ctl/IPConn").unwrap().path, "/ctl/IPConn");
        assert_eq!(parse_url("http://10.0.0.1/x").unwrap().addr, "10.0.0.1:80".parse().unwrap());
        let xml = "<root><device><serviceList>\
            <service><serviceType>urn:schemas-upnp-org:service:Layer3Forwarding:1</serviceType><controlURL>/l3f</controlURL></service>\
            <service><serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType><controlURL>/ctl/IPConn</controlURL></service>\
            </serviceList></device></root>";
        let (url, service) = find_service(xml, &base).unwrap();
        assert_eq!(url.path, "/ctl/IPConn");
        assert_eq!(service, "urn:schemas-upnp-org:service:WANIPConnection:1");
        assert_eq!(
            xml_text("<s:Body><u:R><NewExternalIPAddress>203.0.113.4</NewExternalIPAddress></u:R>", "NewExternalIPAddress").as_deref(),
            Some("203.0.113.4")
        );
        assert_eq!(dechunk(b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n").unwrap(), b"Wikipedia");
    }
}
