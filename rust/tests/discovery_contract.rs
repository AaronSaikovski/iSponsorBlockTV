//! Actual UDP/TCP loopback contracts. The fixture-only constructor neither
//! enumerates LAN interfaces nor enables mDNS, and cannot be selected by CLI.
use isponsorblocktv::{
    api::{ApiClient, ApiOptions},
    discovery::{Discovery, DiscoveryEvent, DiscoveryMode},
    http::{Endpoints, HttpClients},
};
use std::{
    collections::BTreeMap,
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    sync::mpsc,
    task::{JoinHandle, JoinSet},
    time::{Instant, timeout},
};
use tokio_util::sync::CancellationToken;
use url::Url;

struct Request {
    method: String,
    target: String,
    headers: BTreeMap<String, String>,
    body: String,
    received_at: Instant,
}
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}
type Handler = Arc<dyn Fn(&Request, &str) -> Option<Reply> + Send + Sync>;

fn reply(status: u16, body: &str) -> Option<Reply> {
    Some(Reply {
        status,
        headers: vec![],
        body: body.into(),
    })
}
fn standard(request: &Request, base: &str, screen: bool) -> Option<Reply> {
    match (request.method.as_str(), request.target.as_str()) {
        ("GET", path) if path.starts_with("/desc") => Some(Reply {
            status: 200, headers: vec![("Application-URL".into(), format!("{base}apps/"))],
            body: "<r:root xmlns:r='urn:schemas-upnp-org:device-1-0'><r:device><r:friendlyName>TV &amp; [literal] 😀</r:friendlyName></r:device></r:root>".into(),
        }),
        ("GET", "/apps/YouTube") if screen => reply(200, "<service xmlns='urn:dial-multiscreen-org:schemas:dial'><additionalData><screenId>fixture-screen</screenId></additionalData></service>"),
        ("GET", "/apps/YouTube") => reply(200, "<service><additionalData/></service>"),
        ("POST", "/apps/YouTube") => Some(Reply { status: 201, headers: vec![("Location".into(), "/apps/YouTube/run".into())], body: String::new() }),
        ("POST", "/pairing/get_screen") => reply(200, r#"{"screen":{"screenId":"fixture-paired-screen","name":"Cloud name"}}"#),
        _ => reply(404, ""),
    }
}

struct Fixture {
    base: String,
    udp: UdpSocket,
    requests: mpsc::Receiver<Request>,
    cancel: CancellationToken,
    server: Option<JoinHandle<()>>,
}

impl Fixture {
    async fn start(handler: Handler) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let udp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let server_base = base.clone();
        let (tx, requests) = mpsc::channel(1024);
        let server = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    biased;
                    _ = token.cancelled() => break,
                    result = connections.join_next(), if !connections.is_empty() => { result.unwrap().unwrap(); }
                    result = listener.accept() => {
                        let (socket, _) = result.unwrap();
                        let handler = handler.clone();
                        let base = server_base.clone();
                        let tx = tx.clone();
                        connections.spawn(async move {
                            let (mut socket, request) = read_request(socket).await;
                            let response = handler(&request, &base);
                            if tx.send(request).await.is_err() { return; }
                            let Some(response) = response else { std::future::pending::<()>().await; return; };
                            let mut wire = format!("HTTP/1.1 {} Fixture\r\nConnection: close\r\nContent-Length: {}\r\n", response.status, response.body.len());
                            for (key, value) in response.headers { wire.push_str(&format!("{key}: {value}\r\n")); }
                            wire.push_str("\r\n"); wire.push_str(&response.body);
                            let _ = socket.write_all(wire.as_bytes()).await;
                        });
                    }
                }
            }
            connections.abort_all();
            while connections.join_next().await.is_some() {}
        });
        Self {
            base,
            udp,
            requests,
            cancel,
            server: Some(server),
        }
    }

    fn discovery(&self) -> Discovery {
        let base = Url::parse(&self.base).unwrap();
        let endpoints = Endpoints {
            lounge: base.clone(),
            youtube: base.clone(),
            sponsorblock: base,
        };
        let api = ApiClient::new(ApiOptions::default(), endpoints).unwrap();
        let http = HttpClients::new(false, false).unwrap();
        let SocketAddr::V4(target) = self.udp.local_addr().unwrap() else {
            panic!("IPv4 fixture")
        };
        Discovery::loopback(api, http, target)
    }

    async fn search(&self) -> SocketAddr {
        let mut buffer = [0; 4096];
        let (size, peer) = timeout(Duration::from_secs(3), self.udp.recv_from(&mut buffer))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buffer[..size], b"M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 3\r\nST: urn:dial-multiscreen-org:service:dial:1\r\n\r\n");
        assert_eq!(peer.ip(), Ipv4Addr::LOCALHOST);
        assert_ne!(peer.port(), 1900);
        peer
    }

    async fn announce(&self, peer: SocketAddr, path: &str, uuid: &str) {
        let wire = format!(
            "HTTP/1.1 200 OK\r\nLOCATION: {}{}\r\nST: urn:dial-multiscreen-org:service:dial:1\r\nUSN: uuid:{uuid}::urn:dial-multiscreen-org:service:dial:1\r\n\r\n",
            self.base, path
        );
        self.udp.send_to(wire.as_bytes(), peer).await.unwrap();
    }

    async fn request(&mut self) -> Request {
        timeout(Duration::from_secs(3), self.requests.recv())
            .await
            .unwrap()
            .unwrap()
    }

    async fn finish(mut self) {
        self.cancel.cancel();
        self.server.take().unwrap().await.unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

async fn read_request(mut socket: TcpStream) -> (TcpStream, Request) {
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut buffer = [0; 2048];
        let length = socket.read(&mut buffer).await.unwrap();
        assert_ne!(length, 0, "request truncated");
        bytes.extend_from_slice(&buffer[..length]);
        if let Some(end) = bytes.windows(4).position(|value| value == b"\r\n\r\n") {
            break end + 4;
        }
        assert!(bytes.len() < 16384);
    };
    let text = std::str::from_utf8(&bytes[..header_end]).unwrap();
    let mut lines = text.split("\r\n");
    let mut first = lines.next().unwrap().split_ascii_whitespace();
    let method = first.next().unwrap().to_owned();
    let target = first.next().unwrap().to_owned();
    let headers: BTreeMap<_, _> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.to_ascii_lowercase(), value.trim().into()))
        .collect();
    let length = headers
        .get("content-length")
        .map_or(0, |value: &String| value.parse::<usize>().unwrap());
    assert!(length < 16384);
    while bytes.len() < header_end + length {
        let mut buffer = [0; 2048];
        let count = socket.read(&mut buffer).await.unwrap();
        assert_ne!(count, 0);
        bytes.extend_from_slice(&buffer[..count]);
    }
    let body = String::from_utf8(bytes[header_end..header_end + length].to_vec()).unwrap();
    (
        socket,
        Request {
            method,
            target,
            headers,
            body,
            received_at: Instant::now(),
        },
    )
}

#[tokio::test]
async fn active_streams_direct_screen_without_launch_and_preserves_first_name() {
    let mut fixture = Fixture::start(Arc::new(|request, base| standard(request, base, true))).await;
    let mut session = fixture.discovery().start(DiscoveryMode::ActiveOnce);
    let peer = fixture.search().await;
    for _ in 0..8 {
        fixture.announce(peer, "desc", "same-device").await;
    }
    let device = timeout(Duration::from_secs(3), session.next())
        .await
        .unwrap()
        .unwrap();
    let DiscoveryEvent::Device(device) = device else {
        panic!("expected streaming device");
    };
    assert_eq!(device.screen_id, "fixture-screen");
    assert_eq!(device.name, "TV & [literal] 😀");
    assert_eq!(fixture.request().await.target, "/desc");
    let status = fixture.request().await;
    assert_eq!(status.target, "/apps/YouTube");
    assert_eq!(
        status.headers.get("origin").map(String::as_str),
        Some("https://www.youtube.com")
    );
    session.stop().await;
    assert!(
        fixture.requests.try_recv().is_err(),
        "duplicates must not create probes or launch"
    );
    fixture.finish().await;
}

#[tokio::test]
async fn active_launches_once_per_application_and_pairs_exact_random_code() {
    let mut fixture =
        Fixture::start(Arc::new(|request, base| standard(request, base, false))).await;
    let mut session = fixture.discovery().start(DiscoveryMode::ActiveOnce);
    let peer = fixture.search().await;
    fixture.announce(peer, "desc", "first").await;
    fixture.announce(peer, "desc-alias", "second").await;
    for _ in 0..5 {
        fixture.announce(peer, "desc", "first").await;
    }
    let DiscoveryEvent::Device(device) = timeout(Duration::from_secs(3), session.next())
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("expected paired device");
    };
    assert_eq!(device.screen_id, "fixture-paired-screen");
    // Wait for natural completion so an incorrectly duplicated second launch
    // cannot hide behind cancellation immediately after the first result.
    assert!(matches!(
        timeout(Duration::from_secs(6), session.next())
            .await
            .unwrap(),
        Some(DiscoveryEvent::Complete)
    ));
    let mut launch_code = None;
    let mut pairing_code = None;
    let mut launches = 0;
    let mut pairings = 0;
    while let Ok(request) = fixture.requests.try_recv() {
        assert_ne!(
            request.target, "/apps/YouTube/run",
            "launch Location must not be followed"
        );
        if request.method == "POST" && request.target == "/apps/YouTube" {
            launches += 1;
            assert_eq!(
                request.headers.get("origin").map(String::as_str),
                Some("https://www.youtube.com")
            );
            assert_eq!(
                request.headers.get("content-type").map(String::as_str),
                Some("application/x-www-form-urlencoded")
            );
            let fields: BTreeMap<_, _> = url::form_urlencoded::parse(request.body.as_bytes())
                .into_owned()
                .collect();
            let code = fields["pairingCode"].clone();
            assert_eq!(code.len(), 12);
            assert!(code.bytes().all(|byte| byte.is_ascii_digit()));
            assert_eq!(fields["theme"], "cl");
            launch_code = Some(code);
        } else if request.target == "/pairing/get_screen" {
            pairings += 1;
            let fields: BTreeMap<_, _> = url::form_urlencoded::parse(request.body.as_bytes())
                .into_owned()
                .collect();
            pairing_code = fields.get("pairing_code").cloned();
        }
    }
    assert_eq!((launches, pairings), (1, 1));
    assert_eq!(launch_code, pairing_code);
    session.stop().await;
    fixture.finish().await;
}

#[tokio::test]
async fn passive_never_launches_and_stop_joins_socket_and_pending_requests() {
    let mut fixture =
        Fixture::start(Arc::new(|request, base| standard(request, base, false))).await;
    let session = fixture.discovery().start(DiscoveryMode::PassiveContinuous);
    let peer = fixture.search().await;
    fixture.announce(peer, "desc", "one").await;
    assert_eq!(fixture.request().await.method, "GET");
    assert_eq!(fixture.request().await.method, "GET");
    session.stop().await;
    assert!(fixture.requests.try_recv().is_err());
    fixture.announce(peer, "desc", "late").await;
    assert!(
        timeout(Duration::from_millis(50), fixture.requests.recv())
            .await
            .is_err()
    );
    fixture.finish().await;
}

#[tokio::test]
async fn stop_cancels_inflight_http_without_waiting_for_http_timeout() {
    let mut fixture = Fixture::start(Arc::new(|_, _| None)).await;
    let session = fixture.discovery().start(DiscoveryMode::ActiveOnce);
    let peer = fixture.search().await;
    fixture.announce(peer, "desc", "one").await;
    assert_eq!(fixture.request().await.target, "/desc");
    timeout(Duration::from_secs(1), session.stop())
        .await
        .unwrap();
    fixture.finish().await;
}

#[tokio::test]
async fn failed_launch_and_redirects_never_pair_or_follow_location() {
    for status in [302, 400, 500] {
        let mut fixture = Fixture::start(Arc::new(move |request, base| {
            if request.method == "POST" && request.target == "/apps/YouTube" {
                Some(Reply {
                    status,
                    headers: vec![("Location".into(), "/must-not-follow".into())],
                    body: String::new(),
                })
            } else {
                standard(request, base, false)
            }
        }))
        .await;
        let mut session = fixture.discovery().start(DiscoveryMode::ActiveOnce);
        let peer = fixture.search().await;
        fixture.announce(peer, "desc", "one").await;
        assert!(matches!(
            timeout(Duration::from_secs(3), session.next())
                .await
                .unwrap(),
            Some(DiscoveryEvent::BackendWarning { .. })
        ));
        session.stop().await;
        let mut count = 0;
        while let Ok(request) = fixture.requests.try_recv() {
            assert_ne!(request.target, "/must-not-follow");
            assert_ne!(request.target, "/pairing/get_screen");
            count += 1;
        }
        assert_eq!(count, 3);
        fixture.finish().await;
    }
}

#[tokio::test]
async fn metadata_redirect_and_mismatched_application_address_are_rejected() {
    for bad_url in [
        "http://127.0.0.2/apps",
        "http://localhost/apps",
        "http://user@127.0.0.1/apps",
        "http://127.0.0.1/apps#fragment",
        "redirect",
    ] {
        let mut fixture = Fixture::start(Arc::new(move |_, _| {
            Some(Reply {
                status: if bad_url == "redirect" { 302 } else { 200 },
                headers: vec![
                    ("Application-URL".into(), bad_url.into()),
                    ("Location".into(), "/must-not-follow".into()),
                ],
                body: "<root><device><friendlyName>Fixture</friendlyName></device></root>".into(),
            })
        }))
        .await;
        let mut session = fixture.discovery().start(DiscoveryMode::ActiveOnce);
        let peer = fixture.search().await;
        fixture.announce(peer, "desc", "one").await;
        assert!(matches!(
            timeout(Duration::from_secs(3), session.next())
                .await
                .unwrap(),
            Some(DiscoveryEvent::BackendWarning { .. })
        ));
        session.stop().await;
        assert_eq!(fixture.request().await.target, "/desc");
        assert!(fixture.requests.try_recv().is_err());
        fixture.finish().await;
    }
}

#[tokio::test]
async fn candidate_overflow_is_visible_probe_pool_is_eight_and_active_deadline_is_fixed() {
    let mut fixture = Fixture::start(Arc::new(|_, _| None)).await;
    let mut session = fixture.discovery().start(DiscoveryMode::ActiveOnce);
    let peer = fixture.search().await;
    for id in 0..257 {
        fixture
            .announce(peer, &format!("desc-{id}"), &format!("fixture-{id}"))
            .await;
        tokio::task::yield_now().await;
    }
    let event = timeout(Duration::from_secs(3), session.next())
        .await
        .unwrap()
        .unwrap();
    let DiscoveryEvent::BackendWarning { message, .. } = event else {
        panic!("expected overflow warning");
    };
    assert!(message.contains("256"));
    for _ in 0..8 {
        fixture.request().await;
    }
    assert!(
        fixture.requests.try_recv().is_err(),
        "more than eight probes dispatched"
    );
    tokio::time::pause();
    let now = Instant::now();
    tokio::time::advance(Duration::from_secs(50)).await;
    while let Some(event) = session.next().await {
        if matches!(event, DiscoveryEvent::Complete) {
            break;
        }
    }
    assert_eq!(Instant::now() - now, Duration::from_secs(50));
    session.stop().await;
    fixture.finish().await;
}

#[tokio::test]
async fn ten_pairing_attempts_are_two_seconds_apart_and_stop_has_no_extra_attempt() {
    let mut fixture = Fixture::start(Arc::new(|request, base| {
        if request.target == "/pairing/get_screen" {
            reply(404, "")
        } else {
            standard(request, base, false)
        }
    }))
    .await;
    let mut session = fixture.discovery().start(DiscoveryMode::ActiveOnce);
    let peer = fixture.search().await;
    fixture.announce(peer, "desc", "one").await;
    for _ in 0..3 {
        fixture.request().await;
    }
    let first = fixture.request().await;
    assert_eq!(first.target, "/pairing/get_screen");
    let mut previous = first.received_at;
    // Real loopback IO remains live while the bounded pairing cadence advances.
    // Avoid an auto-advancing paused clock racing TCP readiness.
    for _ in 1..10 {
        let request = fixture.request().await;
        assert_eq!(request.target, "/pairing/get_screen");
        assert_eq!(request.body, first.body);
        assert!(request.received_at - previous >= Duration::from_secs(2));
        previous = request.received_at;
    }
    assert!(previous - first.received_at >= Duration::from_secs(18));
    assert!(matches!(
        timeout(Duration::from_secs(3), session.next())
            .await
            .unwrap(),
        Some(DiscoveryEvent::Complete)
    ));
    session.stop().await;
    assert!(fixture.requests.try_recv().is_err());
    fixture.finish().await;
}

#[tokio::test]
async fn passive_reprobes_failed_endpoint_no_faster_than_fifteen_seconds() {
    let mut fixture =
        Fixture::start(Arc::new(|request, base| standard(request, base, false))).await;
    let session = fixture.discovery().start(DiscoveryMode::PassiveContinuous);
    let peer = fixture.search().await;
    fixture.announce(peer, "desc", "one").await;
    fixture.request().await;
    fixture.request().await;
    fixture.announce(peer, "desc", "one").await;
    assert!(
        timeout(Duration::from_millis(50), fixture.requests.recv())
            .await
            .is_err()
    );
    // Advance only while no HTTP request is pending, then resume real IO time.
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(15)).await;
    tokio::time::resume();
    let peer_again = fixture.search().await;
    assert_eq!(
        peer, peer_again,
        "passive search retains the interface socket"
    );
    fixture.announce(peer, "desc", "one").await;
    assert_eq!(fixture.request().await.target, "/desc");
    assert_eq!(fixture.request().await.target, "/apps/YouTube");
    session.stop().await;
    assert!(fixture.requests.try_recv().is_err());
    fixture.finish().await;
}

#[tokio::test]
async fn dropping_session_cancels_inflight_request_and_future_admissions() {
    let mut fixture = Fixture::start(Arc::new(|_, _| None)).await;
    let session = fixture.discovery().start(DiscoveryMode::PassiveContinuous);
    let peer = fixture.search().await;
    fixture.announce(peer, "desc", "one").await;
    fixture.request().await;
    drop(session);
    // Cancellation runs on the next scheduler turn; stop() is the stronger
    // explicit join contract exercised separately above.
    tokio::task::yield_now().await;
    fixture.announce(peer, "desc-late", "two").await;
    assert!(
        timeout(Duration::from_millis(50), fixture.requests.recv())
            .await
            .is_err()
    );
    fixture.finish().await;
}

#[tokio::test]
async fn independent_endpoints_returning_same_screen_emit_only_one_device() {
    let mut fixture = Fixture::start(Arc::new(|request, base| standard(request, base, true))).await;
    let mut session = fixture.discovery().start(DiscoveryMode::ActiveOnce);
    let peer = fixture.search().await;
    fixture.announce(peer, "desc-a", "a").await;
    fixture.announce(peer, "desc-b", "b").await;
    assert!(matches!(
        timeout(Duration::from_secs(3), session.next())
            .await
            .unwrap(),
        Some(DiscoveryEvent::Device(_))
    ));
    assert!(matches!(
        timeout(Duration::from_secs(6), session.next())
            .await
            .unwrap(),
        Some(DiscoveryEvent::Complete)
    ));
    session.stop().await;
    let mut count = 0;
    while let Ok(request) = fixture.requests.try_recv() {
        assert_eq!(request.method, "GET");
        count += 1;
    }
    assert_eq!(count, 4);
    fixture.finish().await;
}

#[tokio::test]
async fn cancelling_during_pairing_stops_future_attempts() {
    let mut fixture = Fixture::start(Arc::new(|request, base| {
        if request.target == "/pairing/get_screen" {
            reply(404, "")
        } else {
            standard(request, base, false)
        }
    }))
    .await;
    let session = fixture.discovery().start(DiscoveryMode::ActiveOnce);
    let peer = fixture.search().await;
    fixture.announce(peer, "desc", "one").await;
    for _ in 0..3 {
        fixture.request().await;
    }
    assert_eq!(fixture.request().await.target, "/pairing/get_screen");
    timeout(Duration::from_secs(1), session.stop())
        .await
        .unwrap();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(30)).await;
    tokio::time::resume();
    assert!(fixture.requests.try_recv().is_err());
    fixture.finish().await;
}

#[tokio::test]
async fn malformed_application_xml_cannot_trigger_launch() {
    for xml in [
        "<!DOCTYPE service [<!ENTITY x SYSTEM 'http://127.0.0.1/private'>]><service>&x;</service>",
        "<service><additionalData><screenId>partial",
        "<service xmlns:d='urn:dial'><d:additionalData>&custom;</d:additionalData></service>",
    ] {
        let mut fixture = Fixture::start(Arc::new(move |request, base| {
            if request.target == "/apps/YouTube" {
                reply(200, xml)
            } else {
                standard(request, base, false)
            }
        }))
        .await;
        let mut session = fixture.discovery().start(DiscoveryMode::ActiveOnce);
        let peer = fixture.search().await;
        fixture.announce(peer, "desc", "one").await;
        assert!(matches!(
            timeout(Duration::from_secs(3), session.next())
                .await
                .unwrap(),
            Some(DiscoveryEvent::BackendWarning { .. })
        ));
        session.stop().await;
        let mut count = 0;
        while let Ok(request) = fixture.requests.try_recv() {
            assert_eq!(request.method, "GET");
            count += 1;
        }
        assert_eq!(count, 2);
        fixture.finish().await;
    }
}

#[tokio::test]
async fn oversized_dial_identities_are_rejected_without_launch_and_scan_continues() {
    use isponsorblocktv::discovery::MAX_SCREEN_ID_BYTES;
    use std::sync::atomic::{AtomicUsize, Ordering};
    for screen in [
        "x".repeat(MAX_SCREEN_ID_BYTES + 1),
        format!("<![CDATA[{}]]>", "x".repeat(MAX_SCREEN_ID_BYTES + 1)),
        "&#x1f600;".repeat(MAX_SCREEN_ID_BYTES / 4 + 1),
        format!("{}<![CDATA[x]]>", "x".repeat(MAX_SCREEN_ID_BYTES)),
    ] {
        let requests = AtomicUsize::new(0);
        let mut fixture = Fixture::start(Arc::new(move |request, base| {
            if request.method == "GET" && request.target == "/apps/YouTube" {
                let screen = if requests.fetch_add(1, Ordering::SeqCst) == 0 {
                    screen.clone()
                } else {
                    "x".repeat(MAX_SCREEN_ID_BYTES)
                };
                return reply(200, &format!(
                    "<service><additionalData><screenId>{screen}</screenId></additionalData></service>"
                ));
            }
            standard(request, base, false)
        })).await;
        let mut session = fixture.discovery().start(DiscoveryMode::ActiveOnce);
        let peer = fixture.search().await;
        fixture.announce(peer, "desc-overlong", "overlong").await;
        let event = timeout(Duration::from_secs(3), session.next())
            .await
            .unwrap();
        assert!(
            matches!(event, Some(DiscoveryEvent::BackendWarning { backend: "DIAL", message })
            if message.contains("retention limit"))
        );
        fixture.announce(peer, "desc-valid", "valid").await;
        let event = timeout(Duration::from_secs(3), session.next())
            .await
            .unwrap();
        let Some(DiscoveryEvent::Device(device)) = event else {
            panic!("expected bounded identity after rejection, got {event:?}");
        };
        assert_eq!(device.screen_id, "x".repeat(MAX_SCREEN_ID_BYTES));
        session.stop().await;
        while let Ok(request) = fixture.requests.try_recv() {
            assert_eq!(request.method, "GET");
        }
        fixture.finish().await;
    }
}

#[tokio::test]
async fn oversized_paired_dial_identity_is_not_emitted_or_retried() {
    use isponsorblocktv::discovery::MAX_SCREEN_ID_BYTES;
    let mut fixture = Fixture::start(Arc::new(|request, base| {
        if request.target == "/pairing/get_screen" {
            return reply(
                200,
                &serde_json::json!({
                    "screen": {"screenId": "x".repeat(MAX_SCREEN_ID_BYTES + 1), "name": "TV"}
                })
                .to_string(),
            );
        }
        standard(request, base, false)
    }))
    .await;
    let mut session = fixture.discovery().start(DiscoveryMode::ActiveOnce);
    let peer = fixture.search().await;
    fixture.announce(peer, "desc", "paired").await;
    let event = timeout(Duration::from_secs(3), session.next())
        .await
        .unwrap();
    assert!(
        matches!(event, Some(DiscoveryEvent::BackendWarning { backend: "DIAL", message })
        if message.contains("256 bytes"))
    );
    session.stop().await;
    let mut pairings = 0;
    while let Ok(request) = fixture.requests.try_recv() {
        pairings += usize::from(request.target == "/pairing/get_screen");
    }
    assert_eq!(pairings, 1);
    fixture.finish().await;
}

#[tokio::test]
async fn oversized_device_name_ends_session_before_emitting_device() {
    use isponsorblocktv::discovery::MAX_DISCOVERY_RESULT_BYTES;
    let fixture = Fixture::start(Arc::new(|request, base| {
        let mut response = standard(request, base, true)?;
        if request.target.starts_with("/desc") {
            response.body = format!(
                "<root><device><friendlyName>{}</friendlyName></device></root>",
                "n".repeat(MAX_DISCOVERY_RESULT_BYTES)
            );
        }
        Some(response)
    }))
    .await;
    let mut session = fixture.discovery().start(DiscoveryMode::PassiveContinuous);
    let peer = fixture.search().await;
    fixture.announce(peer, "desc", "oversized-name").await;
    let event = timeout(Duration::from_secs(3), session.next())
        .await
        .unwrap();
    assert!(
        matches!(event, Some(DiscoveryEvent::BackendWarning { backend: "Discovery", message })
        if message.contains("retention limit") && message.contains("session ended"))
    );
    assert!(matches!(
        session.next().await,
        Some(DiscoveryEvent::Complete)
    ));
    assert!(session.next().await.is_none());
    session.stop().await;
    fixture.finish().await;
}
