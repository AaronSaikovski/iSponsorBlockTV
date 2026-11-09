//! Native loopback Lounge fixtures; never contact public services or paired TVs.
use std::{collections::BTreeMap, time::Duration};

use isponsorblocktv::{
    http::HttpClients,
    lounge::{
        Command, FrameDecoder, LoungeError, LoungeEvent, LoungeTransport, SessionIds,
        session::CONNECT_PARAMS,
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    task::JoinHandle,
};
use url::Url;

type Fields = BTreeMap<String, String>;
struct Request {
    method: String,
    target: String,
    headers: Fields,
    body: String,
}
impl Request {
    fn form(&self) -> Fields {
        url::form_urlencoded::parse(self.body.as_bytes())
            .into_owned()
            .collect()
    }
    fn query(&self) -> Fields {
        Url::parse(&format!("http://fixture{}", self.target))
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect()
    }
}

async fn read_request(socket: &mut TcpStream) -> Request {
    let mut bytes = Vec::new();
    let end = loop {
        let mut chunk = [0; 2048];
        let n = socket.read(&mut chunk).await.unwrap();
        assert_ne!(n, 0);
        bytes.extend_from_slice(&chunk[..n]);
        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            break end + 4;
        }
        assert!(bytes.len() < 16384);
    };
    let text = std::str::from_utf8(&bytes[..end]).unwrap();
    let mut lines = text.split("\r\n");
    let mut first = lines.next().unwrap().split_whitespace();
    let method = first.next().unwrap().to_owned();
    let target = first.next().unwrap().to_owned();
    let headers: Fields = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let length = headers
        .get("content-length")
        .map_or(0, |v| v.parse::<usize>().unwrap());
    while bytes.len() < end + length {
        let mut chunk = [0; 2048];
        let n = socket.read(&mut chunk).await.unwrap();
        assert_ne!(n, 0);
        bytes.extend_from_slice(&chunk[..n]);
    }
    Request {
        method,
        target,
        headers,
        body: String::from_utf8(bytes[end..end + length].to_vec()).unwrap(),
    }
}

fn frame(payload: &str) -> String {
    format!("{}\n{payload}", payload.encode_utf16().count())
}
fn ids(aid: u64) -> SessionIds {
    SessionIds {
        sid: "session-id".into(),
        gsession: "global-id".into(),
        aid,
    }
}
fn transport(base: Url) -> LoungeTransport {
    LoungeTransport::new(
        HttpClients::new(false, false).unwrap(),
        base,
        "screen-id".into(),
        "Native 🦀".into(),
    )
    .unwrap()
}

struct Fixture {
    base: Url,
    requests: mpsc::Receiver<Request>,
    server: JoinHandle<()>,
}
impl Fixture {
    async fn start(replies: Vec<(u16, String)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = Url::parse(&format!(
            "http://{}/api/lounge",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let (sender, requests) = mpsc::channel(64);
        let server = tokio::spawn(async move {
            for (status, body) in replies {
                let (mut socket, _) = listener.accept().await.unwrap();
                sender.send(read_request(&mut socket).await).await.unwrap();
                let response = format!(
                    "HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        Self {
            base,
            requests,
            server,
        }
    }
    fn transport(&self) -> LoungeTransport {
        transport(self.base.clone())
    }
    async fn next(&mut self) -> Request {
        tokio::time::timeout(Duration::from_secs(5), self.requests.recv())
            .await
            .unwrap()
            .unwrap()
    }
    async fn finish(mut self) {
        tokio::time::timeout(Duration::from_secs(5), &mut self.server)
            .await
            .unwrap()
            .unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

#[tokio::test]
async fn auth_requires_matching_screen_and_availability_does_not_require_echo() {
    let mut fixture = Fixture::start(vec![
        (200, r#"{"screens":[{"screenId":"other","loungeToken":"wrong"},{"screenId":"screen-id","loungeToken":"token"}]}"#.into()),
        (200, r#"{"screens":[{"status":"online"}]}"#.into()),
        (200, r#"{"screens":[]}"#.into()),
        (200, "{}".into()),
        (200, r#"{"screens":[{"screenId":"other","loungeToken":"wrong"}]}"#.into()),
        (200, r#"{"screens":[{"screenId":"screen-id","loungeToken":""}]}"#.into()),
    ]).await;
    let client = fixture.transport();
    assert_eq!(client.clone().refresh_token().await.unwrap(), "token");
    let request = fixture.next().await;
    assert_eq!(request.method, "POST");
    assert_eq!(request.target, "/api/lounge/pairing/get_lounge_token_batch");
    assert_eq!(
        request.form(),
        Fields::from([("screen_ids".into(), "screen-id".into())])
    );
    assert!(client.clone().availability("token".into()).await.unwrap());
    let request = fixture.next().await;
    assert_eq!(
        request.target,
        "/api/lounge/pairing/get_screen_availability"
    );
    assert_eq!(
        request.form(),
        Fields::from([("lounge_token".into(), "token".into())])
    );
    assert!(!client.clone().availability("token".into()).await.unwrap());
    assert!(!client.clone().availability("token".into()).await.unwrap());
    assert!(matches!(
        client.clone().refresh_token().await,
        Err(LoungeError::MalformedResponse)
    ));
    assert!(matches!(
        client.refresh_token().await,
        Err(LoungeError::MalformedResponse)
    ));
    fixture.finish().await;
}

#[tokio::test]
async fn exact_handshake_opaque_constant_and_separate_session_frames() {
    let mut fixture = Fixture::start(vec![(
        200,
        frame(r#"[[0,["c","session-id"]]]"#) + &frame(r#"[[1,["S","global-id"]],[2,["noop"]]]"#),
    )])
    .await;
    let envelopes = fixture.transport().connect("token".into()).await.unwrap();
    assert_eq!(envelopes.len(), 3);
    assert_eq!(
        envelopes[0].event,
        LoungeEvent::Connected {
            sid: "session-id".into()
        }
    );
    assert_eq!(
        envelopes[1].event,
        LoungeEvent::Session {
            gsession: "global-id".into()
        }
    );
    assert_eq!(envelopes[2].aid, 2);
    let request = fixture.next().await;
    assert_eq!(request.method, "POST");
    assert_eq!(request.target, "/api/lounge/bc/bind");
    assert_eq!(
        request.headers["content-type"],
        "application/x-www-form-urlencoded"
    );
    let expected: Fields = [
        ("id", "screen-id"),
        ("mdx-version", "3"),
        ("TYPE", "xmlhttp"),
        ("theme", "cl"),
        ("sessionSource", "MDX_SESSION_SOURCE_UNKNOWN"),
        (
            "connectParams",
            r#"{"setStatesParams": "{"playbackSpeed":0}"}"#,
        ),
        ("RID", "1"),
        ("CVER", "1"),
        ("capabilities", "que,dsdtr,atp,vsp"),
        ("ui", "false"),
        ("app", "ytios-phone-20.15.1"),
        ("pairing_type", "manual"),
        ("VER", "8"),
        ("loungeIdToken", "token"),
        ("device", "REMOTE_CONTROL"),
        ("name", "Native 🦀"),
    ]
    .into_iter()
    .map(|(k, v)| (k.into(), v.into()))
    .collect();
    assert_eq!(request.form(), expected);
    assert!(serde_json::from_str::<serde_json::Value>(CONNECT_PARAMS).is_err());
    fixture.finish().await;
}

fn assert_common(query: &Fields, aid: u64) {
    assert_eq!(query["name"], "Native 🦀");
    assert_eq!(query["loungeIdToken"], "token");
    assert_eq!(query["SID"], "session-id");
    assert_eq!(query["gsessionid"], "global-id");
    assert_eq!(query["AID"], aid.to_string());
    assert_eq!(query["device"], "REMOTE_CONTROL");
    assert_eq!(query["app"], "ytios-phone-20.15.1");
    assert_eq!(query["VER"], "8");
    assert_eq!(query["v"], "2");
}

#[tokio::test]
async fn failed_command_is_not_replayed_and_actor_snapshots_control_counters() {
    let mut fixture = Fixture::start(vec![
        (500, "failure-secret-body".into()),
        (200, "".into()),
        (200, "".into()),
    ])
    .await;
    let client = fixture.transport();
    let result = client
        .clone()
        .command("token".into(), ids(8), 1, Command::SeekTo { seconds: 12.5 })
        .await;
    assert!(matches!(result, Err(LoungeError::Status(500))));
    let first = fixture.next().await;
    assert_eq!(first.method, "POST");
    assert_common(&first.query(), 8);
    assert_eq!(first.query()["RID"], "2");
    assert_eq!(first.query().len(), 10);
    assert_eq!(
        first.form(),
        Fields::from([
            ("count".into(), "1".into()),
            ("ofs".into(), "1".into()),
            ("req0__sc".into(), "seekTo".into()),
            ("req0_newTime".into(), "12.5".into()),
        ])
    );
    // Allocation is deliberately external, so even a failed request consumed 1.
    client
        .clone()
        .command("token".into(), ids(9), 2, Command::GetNowPlaying)
        .await
        .unwrap();
    let second = fixture.next().await;
    assert_common(&second.query(), 9);
    assert_eq!(second.query()["RID"], "3");
    assert_eq!(second.form()["ofs"], "2");
    assert_eq!(second.form()["req0__sc"], "getNowPlaying");
    client.disconnect("token".into(), ids(10), 3).await.unwrap();
    let terminate = fixture.next().await;
    assert_common(&terminate.query(), 10);
    assert_eq!(terminate.query()["RID"], "3");
    assert_eq!(terminate.query()["CVER"], "1");
    assert_eq!(terminate.query()["auth_failure_option"], "send_error");
    assert_eq!(
        terminate.form(),
        Fields::from([
            ("ui".into(), "".into()),
            ("TYPE".into(), "terminate".into()),
            (
                "clientDisconnectReason".into(),
                "MDX_SESSION_DISCONNECT_REASON_DISCONNECTED_BY_USER".into()
            ),
        ])
    );
    fixture.finish().await;
}

#[tokio::test]
async fn missing_session_and_invalid_counter_do_not_send_http() {
    let fixture = Fixture::start(vec![]).await;
    let client = fixture.transport();
    for bad in [
        SessionIds {
            sid: String::new(),
            ..ids(0)
        },
        SessionIds {
            gsession: String::new(),
            ..ids(0)
        },
    ] {
        assert!(matches!(
            client
                .clone()
                .command("token".into(), bad.clone(), 1, Command::SkipAd)
                .await,
            Err(LoungeError::MissingSession)
        ));
        assert!(matches!(
            client.clone().subscribe("token".into(), bad).await,
            Err(LoungeError::MissingSession)
        ));
    }
    for offset in [0, u64::MAX] {
        assert!(matches!(
            client
                .clone()
                .command("token".into(), ids(0), offset, Command::SkipAd)
                .await,
            Err(LoungeError::InvalidCommand)
        ));
    }
    fixture.finish().await;
}

#[tokio::test]
async fn recovery_uses_fresh_screen_identity_without_exiting() {
    let mut fixture = Fixture::start(vec![(200, "".into()), (200, "".into())]).await;
    let client = fixture.transport();
    client.clone().recover("token".into()).await.unwrap();
    client.recover("token".into()).await.unwrap();
    let first = fixture.next().await.form();
    let second = fixture.next().await.form();
    assert_eq!(first.len(), 17);
    assert_eq!(first["device"], "LOUNGE_SCREEN");
    assert_eq!(first["connectParams"], CONNECT_PARAMS);
    assert_ne!(first["id"], "screen-id");
    assert_ne!(first["id"], second["id"]);
    assert_ne!(first["id"], first["sessionNonce"]);
    assert_ne!(first["sessionNonce"], second["sessionNonce"]);
    uuid::Uuid::parse_str(&first["id"]).unwrap();
    uuid::Uuid::parse_str(&first["sessionNonce"]).unwrap();
    fixture.finish().await;
}

#[tokio::test]
async fn subscription_streams_validated_batches_and_advances_ignored_aids() {
    let mut fixture = Fixture::start(vec![(200,
        frame(r#"[[3,["noop"]],[4,["unknown-private-event"]]]"#)
        + &frame(r#"[[5,["onStateChange",{"state":"bad"}]],[6,["nowPlaying",{"videoId":"é🦀","state":"1"}]]]"#))]).await;
    let mut stream = fixture
        .transport()
        .subscribe("token".into(), ids(2))
        .await
        .unwrap();
    let request = fixture.next().await;
    assert_eq!(request.method, "GET");
    assert!(request.body.is_empty());
    assert_common(&request.query(), 2);
    assert_eq!(request.query()["RID"], "rpc");
    assert_eq!(request.query()["CI"], "0");
    assert_eq!(request.query()["TYPE"], "xmlhttp");
    assert_eq!(request.query().len(), 12);
    let first = stream.next_batch().await.unwrap().unwrap();
    assert_eq!(first.last().unwrap().aid, 4);
    assert_eq!(first[1].event, LoungeEvent::Unknown);
    let second = stream.next_batch().await.unwrap().unwrap();
    assert!(matches!(second[0].event, LoungeEvent::Malformed { .. }));
    assert_eq!(second.last().unwrap().aid, 6);
    assert!(stream.next_batch().await.unwrap().is_none());
    fixture.finish().await;
}

#[tokio::test]
async fn stream_status_errors_read_bounded_body_not_reason_phrase() {
    let fixture = Fixture::start(vec![
        (400, "Unknown SID".into()),
        (410, "Gone".into()),
        (401, "Expired".into()),
        (401, "Connection denied".into()),
        (401, "unrecognized".into()),
    ])
    .await;
    let client = fixture.transport();
    assert!(matches!(
        client.clone().subscribe("token".into(), ids(0)).await,
        Err(LoungeError::UnknownSid)
    ));
    assert!(matches!(
        client.clone().subscribe("token".into(), ids(0)).await,
        Err(LoungeError::Gone)
    ));
    assert!(matches!(
        client.clone().subscribe("token".into(), ids(0)).await,
        Err(LoungeError::Expired)
    ));
    assert!(matches!(
        client.clone().connect("token".into()).await,
        Err(LoungeError::ConnectionDenied)
    ));
    assert!(matches!(
        client.connect("token".into()).await,
        Err(LoungeError::Expired)
    ));
    fixture.finish().await;
}

#[tokio::test]
async fn cancelled_next_batch_retains_partial_scalar_and_decoder_state() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let payload = frame(r#"[[1,["nowPlaying",{"videoId":"🦀"}]]]"#).into_bytes();
    let split = payload.iter().position(|&b| b == 0xf0).unwrap() + 2;
    let (release, wait) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        socket
            .write_all(
                b"HTTP/1.1 200 Fixture\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        socket
            .write_all(format!("{:x}\r\n", split).as_bytes())
            .await
            .unwrap();
        socket.write_all(&payload[..split]).await.unwrap();
        socket.write_all(b"\r\n").await.unwrap();
        wait.await.unwrap();
        socket
            .write_all(format!("{:x}\r\n", payload.len() - split).as_bytes())
            .await
            .unwrap();
        socket.write_all(&payload[split..]).await.unwrap();
        socket.write_all(b"\r\n0\r\n\r\n").await.unwrap();
    });
    let mut stream = transport(base)
        .subscribe("token".into(), ids(0))
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(30), stream.next_batch())
            .await
            .is_err()
    );
    release.send(()).unwrap();
    let batch = tokio::time::timeout(Duration::from_secs(5), stream.next_batch())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        matches!(&batch[0].event, LoungeEvent::NowPlaying { video_id, .. } if video_id == "🦀")
    );
    assert!(stream.next_batch().await.unwrap().is_none());
    server.await.unwrap();
}

#[test]
fn bytewise_fragmentation_coalescing_and_full_batch_validation() {
    let bytes =
        frame("[[1,[\"nowPlaying\",{\"videoId\":\"é🦀\"}]]]\n") + &frame("[[2,[\"noop\"]]]");
    let mut decoder = FrameDecoder::new();
    let mut aids = Vec::new();
    for byte in bytes.bytes() {
        decoder.push(&[byte]).unwrap();
        while let Some(batch) = decoder.next_batch().unwrap() {
            aids.extend(batch.into_iter().map(|e| e.aid));
        }
    }
    decoder.finish().unwrap();
    assert_eq!(aids, [1, 2]);
    for bad in [
        "[[1,[\"noop\"]],[2,[]]]",
        "[[1,[\"noop\"]],[-1,[\"noop\"]]]",
        "[[1,[\"noop\"]],[2,[false]]]",
        "[[1,[\"noop\"],3]]",
    ] {
        let mut decoder = FrameDecoder::new();
        decoder.push(frame(bad).as_bytes()).unwrap();
        assert!(decoder.next_batch().is_err());
    }
}
