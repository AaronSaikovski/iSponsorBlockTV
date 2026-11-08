//! Native DIAL discovery. Protocol behavior follows the project's dial_client.py.
//!
//! DIAL specification notice retained from that implementation:
//! Redistribution and use of the DIAL DIscovery And Launch protocol
//! specification (the “DIAL Specification”), with or without modification,
//! are permitted provided that the following conditions are met:
//! Redistributions of the DIAL Specification must retain the above copyright
//! notice, this list of conditions and the following disclaimer.
//! Redistributions of implementations of the DIAL Specification in source code
//! form must retain the above copyright notice, this list of conditions and the
//! following disclaimer. Redistributions of implementations of the DIAL
//! Specification in binary form must include the above copyright notice. The
//! DIAL mark, the NETFLIX mark and the names of contributors to the DIAL
//! Specification may not be used to endorse or promote specifications, software,
//! products, or any other materials derived from the DIAL Specification without
//! specific prior written permission. The DIAL mark is owned by Netflix and
//! information on licensing the DIAL mark is available at www.dial-multiscreen.org.
//!
//! Reference SSDP implementation notice:
//! MIT License — Copyright (c) 2018 Johannes Hoppe
//!
//! Permission is hereby granted, free of charge, to any person obtaining a copy
//! of this software and associated documentation files (the "Software"), to deal
//! in the Software without restriction, including without limitation the rights
//! to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
//! copies of the Software, and to permit persons to whom the Software is
//! furnished to do so, subject to the following conditions:
//!
//! The above copyright notice and this permission notice shall be included in all
//! copies or substantial portions of the Software.
//!
//! THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
//! IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
//! FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
//! AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
//! LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
//! OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
//! SOFTWARE.

use super::{
    BackendEvent, Candidate as BackendCandidate, DiscoveredDevice, DiscoveryMode, ProbeContext,
    RETRY_INTERVAL, TransportPolicy,
};
use quick_xml::{events::Event, name::ResolveResult, reader::NsReader};
use rand::{Rng, rngs::OsRng};
use reqwest::{
    Method,
    header::{LOCATION, ORIGIN},
};
use std::{
    collections::HashSet,
    net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4},
    time::Duration,
};
use tokio::{
    net::UdpSocket,
    sync::mpsc,
    task::JoinSet,
    time::{Instant, sleep_until},
};
use tokio_util::sync::CancellationToken;
use url::{Host, Url};

const MAX_DATAGRAM: usize = 16 * 1024;
const SEARCH_TARGET: &str = "urn:dial-multiscreen-org:service:dial:1";
const SEARCH: &[u8] = b"M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 3\r\nST: urn:dial-multiscreen-org:service:dial:1\r\n\r\n";
const MULTICAST: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(239, 255, 255, 250), 1900);

pub(crate) struct Candidate {
    location: Url,
    sender: IpAddr,
    usn: Option<String>,
}

impl Candidate {
    pub(crate) fn keys(&self) -> Vec<String> {
        let mut keys = vec![format!("dial:url:{}", self.location)];
        if let Some(usn) = &self.usn {
            keys.push(format!("dial:uuid:{usn}"));
        }
        keys
    }
}

fn unicast(ip: IpAddr, policy: TransportPolicy) -> bool {
    !ip.is_unspecified()
        && !ip.is_multicast()
        && (!ip.is_loopback() || policy.allow_loopback)
        && match ip {
            IpAddr::V4(ip) => !ip.is_broadcast(),
            IpAddr::V6(ip) => ip.to_ipv4_mapped().is_none(),
        }
}

fn admitted_url(value: &str, sender: IpAddr, policy: TransportPolicy) -> Result<Url, &'static str> {
    let url = Url::parse(value).map_err(|_| "Invalid DIAL URL")?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err("Unsafe DIAL URL rejected");
    }
    let address = match url.host() {
        Some(Host::Ipv4(ip)) => IpAddr::V4(ip),
        Some(Host::Ipv6(ip)) => IpAddr::V6(ip),
        _ => return Err("DIAL URL must use the responder's literal IP address"),
    };
    if address != sender || !unicast(address, policy) {
        return Err("DIAL URL does not match an admitted responder");
    }
    Ok(url)
}

fn announcement(bytes: &[u8], sender: IpAddr, policy: TransportPolicy) -> Option<Candidate> {
    if bytes.len() > MAX_DATAGRAM || !unicast(sender, policy) {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    let text = text.strip_suffix("\r\n\r\n")?;
    let mut lines = text.split("\r\n");
    let mut status = lines.next()?.splitn(3, ' ');
    if !matches!(status.next(), Some("HTTP/1.1" | "HTTP/1.0")) || status.next() != Some("200") {
        return None;
    }
    if status.next()?.chars().any(|c| c.is_control() && c != '\t') {
        return None;
    }
    let mut location = None;
    let mut target = None;
    let mut usn = None;
    for line in lines {
        let (name, value) = line.split_once(':')?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&c))
            || value.chars().any(|c| c.is_control() && c != '\t')
        {
            return None;
        }
        let value = value.trim();
        if name.eq_ignore_ascii_case("location") {
            if location.replace(value).is_some() {
                return None;
            }
        } else if name.eq_ignore_ascii_case("st") {
            if target.replace(value).is_some() {
                return None;
            }
        } else if name.eq_ignore_ascii_case("usn") && usn.replace(value).is_some() {
            return None;
        }
    }
    if !target?.eq_ignore_ascii_case(SEARCH_TARGET) {
        return None;
    }
    let location = admitted_url(location?, sender, policy).ok()?;
    let usn = usn
        .and_then(|value| value.split("::").next())
        .filter(|value| {
            value.len() > 5
                && value.len() <= 133
                && value
                    .get(..5)
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case("uuid:"))
        })
        .map(str::to_ascii_lowercase);
    Some(Candidate {
        location,
        sender,
        usn,
    })
}

async fn send_event(
    tx: &mpsc::Sender<BackendEvent>,
    cancel: &CancellationToken,
    end: Option<Instant>,
    event: BackendEvent,
) -> bool {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => false,
        _ = super::deadline(end) => false,
        result = tx.send(event) => result.is_ok(),
    }
}

pub(crate) async fn collect(
    tx: mpsc::Sender<BackendEvent>,
    cancel: CancellationToken,
    admission_end: Option<Instant>,
    fixture_target: Option<SocketAddrV4>,
    policy: TransportPolicy,
) {
    let interfaces: Vec<Ipv4Addr> = if fixture_target.is_some() {
        vec![Ipv4Addr::LOCALHOST]
    } else {
        match if_addrs::get_if_addrs() {
            Ok(interfaces) => interfaces
                .iter()
                .filter(|interface| interface.is_oper_up() && !interface.is_loopback())
                .filter_map(|interface| match interface.ip() {
                    IpAddr::V4(ip) if unicast(IpAddr::V4(ip), policy) => Some(ip),
                    _ => None,
                })
                .collect::<HashSet<_>>()
                .into_iter()
                .collect(),
            Err(_) => {
                send_event(
                    &tx,
                    &cancel,
                    admission_end,
                    BackendEvent::Warning {
                        backend: "DIAL",
                        message: "Unable to enumerate LAN interfaces",
                    },
                )
                .await;
                return;
            }
        }
    };
    if interfaces.is_empty() {
        send_event(
            &tx,
            &cancel,
            admission_end,
            BackendEvent::Warning {
                backend: "DIAL",
                message: "No active non-loopback IPv4 interface is available for DIAL discovery",
            },
        )
        .await;
        return;
    }
    let mut sockets = JoinSet::new();
    for address in interfaces {
        let tx = tx.clone();
        let cancel = cancel.clone();
        sockets.spawn(async move {
            let result = socket_loop(
                address,
                fixture_target.unwrap_or(MULTICAST),
                &tx,
                &cancel,
                admission_end,
                policy,
            )
            .await;
            if let Err(message) = result {
                send_event(
                    &tx,
                    &cancel,
                    admission_end,
                    BackendEvent::Warning {
                        backend: "DIAL",
                        message,
                    },
                )
                .await;
            }
        });
    }
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            _ = super::deadline(admission_end) => break,
            result = sockets.join_next() => match result {
                None => break,
                Some(Ok(())) => {},
                Some(Err(_)) => { send_event(&tx, &cancel, admission_end, BackendEvent::Warning { backend: "DIAL", message: "DIAL interface worker ended unexpectedly" }).await; }
            }
        }
    }
    sockets.abort_all();
    while sockets.join_next().await.is_some() {}
}

async fn socket_loop(
    address: Ipv4Addr,
    target: SocketAddrV4,
    tx: &mpsc::Sender<BackendEvent>,
    cancel: &CancellationToken,
    end: Option<Instant>,
    policy: TransportPolicy,
) -> Result<(), &'static str> {
    let socket = UdpSocket::bind(SocketAddrV4::new(address, 0))
        .await
        .map_err(|_| "Unable to bind DIAL discovery socket")?;
    socket2::SockRef::from(&socket)
        .set_multicast_if_v4(&address)
        .map_err(|_| "Unable to select DIAL multicast interface")?;
    let mut next_search = Instant::now();
    let mut buffer = vec![0; MAX_DATAGRAM + 1];
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(()),
            _ = super::deadline(end) => return Ok(()),
            _ = sleep_until(next_search) => {
                socket.send_to(SEARCH, target).await.map_err(|_| "Unable to send DIAL search")?;
                next_search = if let Some(end) = end { end } else { Instant::now() + RETRY_INTERVAL };
            }
            result = socket.recv_from(&mut buffer) => {
                let (length, sender) = result.map_err(|_| "Unable to receive DIAL announcement")?;
                let observed_at = Instant::now();
                if !matches!(sender, SocketAddr::V4(_)) { continue; }
                if let Some(candidate) = announcement(&buffer[..length], sender.ip(), policy)
                    && !send_event(tx, cancel, end, BackendEvent::Candidate { candidate: BackendCandidate::Dial(candidate), observed_at }).await { return Ok(()); }
            }
        }
    }
}

/// Resolve namespaces before matching local paths, reject undefined prefixes,
/// DTDs and custom entities, and validate the entire bounded document before
/// returning a field. Device names are literal text, never terminal markup.
fn xml_field(bytes: &[u8], path: &[&[u8]]) -> Result<Option<String>, &'static str> {
    if bytes.len() > crate::http::MAX_BODY_BYTES {
        return Err("DIAL XML exceeds body limit");
    }
    let mut reader = NsReader::from_reader(bytes);
    let mut stack: Vec<Vec<u8>> = Vec::new();
    let mut roots = 0;
    let mut value = String::new();
    let mut found = false;
    loop {
        let (namespace, event) = reader
            .read_resolved_event()
            .map_err(|_| "Malformed DIAL XML")?;
        if matches!(namespace, ResolveResult::Unknown(_)) {
            return Err("Undefined XML namespace prefix");
        }
        let empty = matches!(&event, Event::Empty(_));
        match event {
            Event::Start(element) | Event::Empty(element) => {
                if stack.is_empty() {
                    roots += 1;
                    if roots != 1 {
                        return Err("Multiple XML roots");
                    }
                }
                if stack.len() >= 64 {
                    return Err("DIAL XML nesting exceeds limit");
                }
                for attribute in element.attributes() {
                    let attribute = attribute.map_err(|_| "Malformed XML attribute")?;
                    if matches!(
                        reader.resolve_attribute(attribute.key).0,
                        ResolveResult::Unknown(_)
                    ) {
                        return Err("Undefined XML attribute namespace prefix");
                    }
                    attribute
                        .decode_and_unescape_value(reader.decoder())
                        .map_err(|_| "Invalid XML attribute entity")?;
                }
                if !empty {
                    stack.push(element.local_name().as_ref().to_vec());
                }
            }
            Event::End(_) => {
                if stack.pop().is_none() {
                    return Err("Malformed DIAL XML nesting");
                }
            }
            Event::DocType(_) => return Err("DIAL XML document types are forbidden"),
            Event::Text(text) => {
                let text = text.xml_content().map_err(|_| "Invalid DIAL XML text")?;
                if stack.is_empty() && !text.trim().is_empty() {
                    return Err("Text outside XML root");
                }
                if stack.iter().map(Vec::as_slice).eq(path.iter().copied()) {
                    value.push_str(&text);
                    found = true;
                }
            }
            Event::CData(text) => {
                if stack.is_empty() {
                    return Err("Text outside XML root");
                }
                if stack.iter().map(Vec::as_slice).eq(path.iter().copied()) {
                    value.push_str(&text.decode().map_err(|_| "Invalid DIAL XML text")?);
                    found = true;
                }
            }
            Event::GeneralRef(reference) => {
                let character = match reference
                    .resolve_char_ref()
                    .map_err(|_| "Invalid XML character reference")?
                {
                    Some(character) => character,
                    None => match reference.as_ref() {
                        b"amp" => '&',
                        b"lt" => '<',
                        b"gt" => '>',
                        b"apos" => '\'',
                        b"quot" => '"',
                        _ => return Err("Custom XML entities are forbidden"),
                    },
                };
                if stack.is_empty() {
                    return Err("Text outside XML root");
                }
                if stack.iter().map(Vec::as_slice).eq(path.iter().copied()) {
                    value.push(character);
                    found = true;
                }
            }
            Event::Eof => {
                if roots != 1 || !stack.is_empty() {
                    return Err("Incomplete DIAL XML document");
                }
                return Ok(found
                    .then(|| value.trim().to_owned())
                    .filter(|value| !value.is_empty()));
            }
            _ => {}
        }
    }
}

pub(crate) async fn probe(
    candidate: Candidate,
    context: &ProbeContext,
) -> Result<Option<DiscoveredDevice>, &'static str> {
    let http = &context.http.dial;
    let response = http
        .execute(http.request(Method::GET, candidate.location))
        .await
        .map_err(|_| "DIAL device metadata request failed")?;
    if !response.status.is_success() {
        return Err("DIAL metadata returned an unsuccessful status (redirects are forbidden)");
    }
    let name = xml_field(&response.body, &[b"root", b"device", b"friendlyName"])?
        .unwrap_or_else(|| "Unnamed device".into());
    let application = response
        .headers
        .get("application-url")
        .and_then(|value| value.to_str().ok())
        .ok_or("DIAL metadata has no valid Application-URL")?;
    let mut application = admitted_url(application, candidate.sender, context.policy)?;
    if application.query().is_some() {
        return Err("DIAL application URL must not contain a query");
    }
    if !application.path().ends_with('/') {
        application.set_path(&format!("{}/", application.path()));
    }
    let youtube = application
        .join("YouTube")
        .map_err(|_| "Invalid DIAL application URL")?;
    let response = http
        .execute(
            http.request(Method::GET, youtube.clone())
                .header(ORIGIN, "https://www.youtube.com"),
        )
        .await
        .map_err(|_| "DIAL YouTube status request failed")?;
    if response.status.is_redirection() {
        return Err("DIAL application redirects are forbidden");
    }
    if response.status.is_success() {
        if let Some(screen_id) = xml_field(
            &response.body,
            &[b"service", b"additionalData", b"screenId"],
        )? {
            return Ok(Some(DiscoveredDevice { screen_id, name }));
        }
    } else if response.status != reqwest::StatusCode::NOT_FOUND {
        return Err("DIAL YouTube status request returned an unsuccessful status");
    }
    if context.mode == DiscoveryMode::PassiveContinuous {
        return Ok(None);
    }
    {
        let mut claimed = context
            .claimed_endpoints
            .lock()
            .map_err(|_| "DIAL endpoint admission unavailable")?;
        if !claimed.insert(youtube.as_str().to_owned()) {
            return Ok(None);
        }
    }
    let code: String = (0..12)
        .map(|_| char::from(b'0' + OsRng.gen_range(0..10)))
        .collect();
    let response = http
        .execute(
            http.request(Method::POST, youtube.clone())
                .header(ORIGIN, "https://www.youtube.com")
                .form(&[("pairingCode", code.as_str()), ("theme", "cl")]),
        )
        .await
        .map_err(|_| "DIAL launch request failed")?;
    if !response.status.is_success() {
        return Err("DIAL launch was rejected (redirects are forbidden)");
    }
    let location = response
        .headers
        .get(LOCATION)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .ok_or("DIAL launch did not provide a Location header")?;
    let location = youtube
        .join(location)
        .map_err(|_| "DIAL launch returned an invalid Location")?;
    admitted_url(location.as_str(), candidate.sender, context.policy)?;
    // Location is validated but deliberately never followed.
    for attempt in 0..10 {
        if attempt != 0 {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        match context.api.pair_with_code(&code).await {
            Ok(Some(paired)) => {
                return Ok(Some(DiscoveredDevice {
                    screen_id: paired.screen_id,
                    name,
                }));
            }
            Ok(None) => {}
            Err(_) => {
                tracing::debug!("DIAL pairing attempt failed; retrying within probe deadline")
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    const POLICY: TransportPolicy = TransportPolicy {
        allow_loopback: false,
    };
    fn sender() -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(192, 168, 1, 4))
    }

    #[test]
    fn urls_pin_literal_sender_and_reject_unsafe_targets() {
        assert!(admitted_url("http://192.168.1.4:123/apps", sender(), POLICY).is_ok());
        assert!(admitted_url("https://192.168.1.4/apps", sender(), POLICY).is_ok());
        for value in [
            "http://192.168.1.5/apps",
            "http://device.local/apps",
            "http://user@192.168.1.4/apps",
            "http://192.168.1.4/apps#x",
            "ftp://192.168.1.4/apps",
            "http://127.0.0.1/apps",
        ] {
            assert!(admitted_url(value, sender(), POLICY).is_err(), "{value}");
        }
        for value in ["127.0.0.1", "0.0.0.0", "239.1.1.1", "255.255.255.255"] {
            let ip: IpAddr = value.parse().unwrap();
            assert!(admitted_url(&format!("http://{value}/"), ip, POLICY).is_err());
        }
    }

    #[test]
    fn ssdp_exact_framing_and_case_insensitive_headers() {
        assert_eq!(SEARCH, b"M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 3\r\nST: urn:dial-multiscreen-org:service:dial:1\r\n\r\n");
        let valid = b"HTTP/1.1 200 OK\r\nlOcAtIoN: http://192.168.1.4:456/desc\r\nsT: urn:dial-multiscreen-org:service:dial:1\r\nUSN: uuid:one::urn:dial-multiscreen-org:service:dial:1\r\n\r\n";
        let parsed = announcement(valid, sender(), POLICY).unwrap();
        assert_eq!(parsed.location.port(), Some(456));
        assert_eq!(parsed.usn.as_deref(), Some("uuid:one"));
        for invalid in [
            String::from_utf8_lossy(valid).replace("200", "404"),
            String::from_utf8_lossy(valid).replace("\r\n", "\n"),
            String::from_utf8_lossy(valid).replace("dial:1", "dial:2"),
            String::from_utf8_lossy(valid).replace("192.168.1.4", "127.0.0.1"),
        ] {
            assert!(announcement(invalid.as_bytes(), sender(), POLICY).is_none());
        }
        assert!(announcement(&vec![b'x'; MAX_DATAGRAM + 1], sender(), POLICY).is_none());
    }

    #[test]
    fn xml_namespaces_text_entities_and_security_limits() {
        let path: &[&[u8]] = &[b"service", b"additionalData", b"screenId"];
        for xml in [
            "<service xmlns='urn:dial'><additionalData><screenId>a&amp;b&#x1f600;</screenId></additionalData></service>",
            "<d:service xmlns:d='urn:dial'><d:additionalData><d:screenId>a&amp;b😀</d:screenId></d:additionalData></d:service>",
        ] {
            assert_eq!(
                xml_field(xml.as_bytes(), path).unwrap().as_deref(),
                Some("a&b😀")
            );
        }
        assert_eq!(
            xml_field(b"<service><additionalData/></service>", path).unwrap(),
            None
        );
        for invalid in [
            "<!DOCTYPE service [<!ENTITY e 'secret'>]><service/>",
            "<service>&custom;</service>",
            "<d:service/>",
            "<service></wrong>",
            "<service>",
            "<service/><service/>",
            "outside<service/>",
        ] {
            assert!(xml_field(invalid.as_bytes(), path).is_err(), "{invalid}");
        }
        let deep = format!("{}{}", "<a>".repeat(65), "</a>".repeat(65));
        assert!(xml_field(deep.as_bytes(), path).is_err());
        assert!(xml_field(&vec![b'x'; crate::http::MAX_BODY_BYTES + 1], path).is_err());
    }
}
