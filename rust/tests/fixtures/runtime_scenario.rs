//! Real HTTP/Lounge fixture shared by runtime contracts and the native scenario.
//! Every transport is loopback-only; no runtime flags or environment overrides.
#![allow(dead_code)]
use isponsorblocktv::{
    config::{ConfigDocument, RuntimeConfig},
    http::Endpoints,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot},
    task::{JoinHandle, JoinSet},
};
use tokio_util::sync::CancellationToken;
use url::Url;

pub struct Request {
    pub method: String,
    pub path: String,
    pub query: BTreeMap<String, String>,
    pub form: BTreeMap<String, String>,
    pub device: String,
    /// Deferred responses let tests order observations before HTTP completion.
    pub reply: Option<oneshot::Sender<u16>>,
    closed: Option<oneshot::Receiver<()>>,
}
impl Request {
    pub fn command(&self) -> Option<&str> {
        self.form.get("req0__sc").map(String::as_str)
    }
    pub fn respond(mut self, status: u16) {
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(status);
        }
    }

    pub async fn cancelled(mut self) {
        tokio::time::timeout(
            Duration::from_secs(5),
            self.closed.take().expect("observable deferred termination"),
        )
        .await
        .expect("client must release the termination socket")
        .expect("server must observe client cancellation, not drop its socket");
        assert!(self.reply.as_ref().unwrap().is_closed());
    }
}

enum StreamAction {
    Events(Vec<Value>),
    Close,
}
type Streams = Arc<Mutex<BTreeMap<String, mpsc::Sender<StreamAction>>>>;

pub struct Fixture {
    pub endpoints: Endpoints,
    requests: mpsc::Receiver<Request>,
    streams: Streams,
    cancel: CancellationToken,
    server: Option<JoinHandle<()>>,
}

impl Fixture {
    pub async fn start(defer_seeks: bool) -> Self {
        Self::start_with_options(defer_seeks, false, false).await
    }

    pub async fn start_with_deferred_segments() -> Self {
        Self::start_with_options(true, true, false).await
    }

    pub async fn start_with_deferred_terminations() -> Self {
        Self::start_with_options(false, false, true).await
    }

    async fn start_with_options(
        defer_seeks: bool,
        defer_segments: bool,
        defer_terminations: bool,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let endpoints = Endpoints {
            lounge: base.join("lounge/").unwrap(),
            youtube: base.join("youtube/").unwrap(),
            sponsorblock: base.join("sponsorblock/").unwrap(),
        };
        let streams = Streams::default();
        let shared = streams.clone();
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        let (sender, requests) = mpsc::channel(512);
        let server = tokio::spawn(async move {
            let mut jobs = JoinSet::new();
            loop {
                tokio::select! {
                    _ = stop.cancelled() => break,
                    accepted = listener.accept() => {
                        let (socket, _) = accepted.unwrap();
                        let sender = sender.clone(); let streams = shared.clone();
                        jobs.spawn(async move {
                            serve(socket, sender, streams, defer_seeks, defer_segments, defer_terminations).await;
                        });
                    },
                    result = jobs.join_next(), if !jobs.is_empty() => { if let Some(result) = result { result.unwrap(); } },
                }
            }
            jobs.abort_all();
            while jobs.join_next().await.is_some() {}
        });
        Self {
            endpoints,
            requests,
            streams,
            cancel,
            server: Some(server),
        }
    }

    pub fn config(&self, screens: &[&str]) -> RuntimeConfig {
        let document: ConfigDocument = serde_json::from_value(json!({
            "devices": screens.iter().map(|screen| json!({"screen_id":screen,"name":screen,"offset":0})).collect::<Vec<_>>(),
            "minimum_skip_length": 0,
            "mute_ads": true,
            "skip_ads": true,
            "sponsorblock_api_url": self.endpoints.sponsorblock.as_str(),
        })).unwrap();
        document.runtime_config().unwrap()
    }

    pub async fn next(&mut self) -> Request {
        tokio::time::timeout(Duration::from_secs(5), self.requests.recv())
            .await
            .expect("runtime request deadline")
            .expect("fixture alive")
    }

    pub async fn command(&mut self, name: &str, device: &str) -> Request {
        loop {
            let request = self.next().await;
            if request.command() == Some(name) && request.device == device {
                return request;
            }
        }
    }

    pub async fn report(&mut self) -> Request {
        loop {
            let request = self.next().await;
            if request.path.contains("viewedVideoSponsorTime") {
                return request;
            }
        }
    }

    pub async fn subscribed(&mut self, device: &str) {
        loop {
            if self.streams.lock().unwrap().contains_key(device) {
                return;
            }
            let request = self.next().await;
            assert!(
                request.reply.is_none(),
                "unexpected deferred command while awaiting subscription"
            );
        }
    }

    pub async fn reconnected(&mut self, device: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            if self.streams.lock().unwrap().contains_key(device) {
                return;
            }
            let request = tokio::time::timeout_at(deadline, self.requests.recv())
                .await
                .expect("ten-second reconnect deadline")
                .expect("fixture alive");
            assert!(
                request.reply.is_none(),
                "unexpected deferred command while awaiting reconnect"
            );
        }
    }

    pub async fn events(&self, device: &str, events: Vec<Value>) {
        let sender = self
            .streams
            .lock()
            .unwrap()
            .get(device)
            .cloned()
            .expect("subscribed device");
        sender.send(StreamAction::Events(events)).await.unwrap();
    }

    pub async fn drop_stream(&self, device: &str) {
        let sender = self
            .streams
            .lock()
            .unwrap()
            .remove(device)
            .expect("subscribed device");
        sender.send(StreamAction::Close).await.unwrap();
    }

    pub async fn no_command_or_report(&mut self) {
        // Drain harmless HTTP requests but fail on any externally visible control.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(100);
        while let Ok(Some(request)) = tokio::time::timeout_at(deadline, self.requests.recv()).await
        {
            assert!(
                request.command().is_none(),
                "unexpected command {:?}",
                request.command()
            );
            assert!(
                !request.path.contains("viewedVideoSponsorTime"),
                "unexpected viewed report"
            );
        }
    }

    pub async fn no_requests(&mut self) {
        match tokio::time::timeout(Duration::from_millis(100), self.requests.recv()).await {
            Err(_) => {}
            Ok(Some(request)) => panic!(
                "unexpected {} {} from {}: {:?}",
                request.method,
                request.path,
                request.device,
                request.command()
            ),
            Ok(None) => panic!("fixture stopped before quiet period"),
        }
    }

    pub async fn stop(mut self) {
        self.cancel.cancel();
        self.server.take().unwrap().await.unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(server) = &self.server {
            server.abort();
        }
    }
}

pub fn now_playing(video: &str, position: f64, state: i32) -> Value {
    json!(["nowPlaying", {"videoId":video,"currentTime":position.to_string(),"state":state.to_string()}])
}
pub fn state(position: f64, state: i32) -> Value {
    json!(["onStateChange", {"currentTime":position.to_string(),"state":state.to_string()}])
}
fn frame(value: Value) -> String {
    let payload = value.to_string();
    format!("{}\n{}", payload.encode_utf16().count(), payload)
}

async fn read_request(socket: &mut TcpStream) -> Request {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 2048];
    let end = loop {
        let n = socket.read(&mut chunk).await.unwrap();
        assert_ne!(n, 0);
        bytes.extend_from_slice(&chunk[..n]);
        if let Some(index) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            break index + 4;
        }
        assert!(bytes.len() < 16384);
    };
    let head = std::str::from_utf8(&bytes[..end]).unwrap();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap().split_whitespace();
    let method = first.next().unwrap().to_owned();
    let url = Url::parse(&format!("http://fixture{}", first.next().unwrap())).unwrap();
    let length = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .map_or(0, |(_, v)| v.trim().parse::<usize>().unwrap());
    while bytes.len() < end + length {
        let n = socket.read(&mut chunk).await.unwrap();
        assert_ne!(n, 0);
        bytes.extend_from_slice(&chunk[..n]);
    }
    let query: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
    let form: BTreeMap<_, _> = url::form_urlencoded::parse(&bytes[end..end + length])
        .into_owned()
        .collect();
    let device = form
        .get("screen_ids")
        .or_else(|| form.get("id"))
        .or_else(|| form.get("lounge_token"))
        .or_else(|| query.get("loungeIdToken"))
        .cloned()
        .unwrap_or_default();
    Request {
        method,
        path: url.path().into(),
        query,
        form,
        device,
        reply: None,
        closed: None,
    }
}

async fn reply(socket: &mut TcpStream, status: u16, body: &str) {
    let header = format!(
        "HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = socket.write_all(header.as_bytes()).await;
    let _ = socket.write_all(body.as_bytes()).await;
}

async fn serve(
    mut socket: TcpStream,
    sender: mpsc::Sender<Request>,
    streams: Streams,
    defer_seeks: bool,
    defer_segments: bool,
    defer_terminations: bool,
) {
    let mut request = read_request(&mut socket).await;
    let path = request.path.clone();
    let device = request.device.clone();
    if path.ends_with("get_lounge_token_batch") {
        sender.send(request).await.unwrap();
        reply(
            &mut socket,
            200,
            &json!({"screens":[{"screenId":device,"loungeToken":device}]}).to_string(),
        )
        .await;
    } else if path.ends_with("get_screen_availability") {
        sender.send(request).await.unwrap();
        reply(&mut socket, 200, r#"{"screens":[{"status":"online"}]}"#).await;
    } else if path.contains("skipSegments/") {
        let receive = if defer_segments {
            let (tx, rx) = oneshot::channel();
            request.reply = Some(tx);
            Some(rx)
        } else {
            None
        };
        sender.send(request).await.unwrap();
        let segments: Vec<_> = ["video-a", "video-b", "video-next"].iter().map(|video| json!({
            "videoID":video,"segments":[{"segment":[0,5],"UUID":format!("uuid-{video}"),"locked":1,"category":"sponsor","actionType":"skip"}]
        })).collect();
        let status = match receive {
            Some(rx) => rx.await.unwrap_or(500),
            None => 200,
        };
        reply(&mut socket, status, &json!(segments).to_string()).await;
    } else if path.contains("viewedVideoSponsorTime") {
        sender.send(request).await.unwrap();
        reply(&mut socket, 200, "{}").await;
    } else if request.method == "GET" && path.ends_with("bind") {
        let (tx, mut rx) = mpsc::channel(256);
        streams.lock().unwrap().insert(device.clone(), tx);
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
            .await
            .unwrap();
        sender.send(request).await.unwrap();
        let mut aid = 2u64;
        while let Some(action) = rx.recv().await {
            match action {
                StreamAction::Close => break,
                StreamAction::Events(events) => {
                    let batch: Vec<_> = events
                        .into_iter()
                        .map(|event| {
                            aid += 1;
                            json!([aid, event])
                        })
                        .collect();
                    let body = frame(json!(batch));
                    let chunk = format!("{:x}\r\n{body}\r\n", body.len());
                    if socket.write_all(chunk.as_bytes()).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = socket.write_all(b"0\r\n\r\n").await;
    } else if request.form.contains_key("req0__sc") {
        let receive = if defer_seeks && request.command() == Some("seekTo") {
            let (tx, rx) = oneshot::channel();
            request.reply = Some(tx);
            Some(rx)
        } else {
            None
        };
        sender.send(request).await.unwrap();
        let status = match receive {
            Some(rx) => rx.await.unwrap_or(500),
            None => 200,
        };
        reply(&mut socket, status, "{}").await;
    } else if request.form.get("TYPE").is_some_and(|v| v == "terminate") {
        if defer_terminations {
            let (response, receive) = oneshot::channel();
            let (closed, observe_closed) = oneshot::channel();
            request.reply = Some(response);
            request.closed = Some(observe_closed);
            sender.send(request).await.unwrap();
            let mut byte = [0u8; 1];
            tokio::select! {
                status = receive => {
                    reply(&mut socket, status.unwrap_or(500), "{}").await;
                }
                result = socket.read(&mut byte) => {
                    assert!(
                        matches!(result, Ok(0))
                            || matches!(&result, Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset),
                        "termination socket must close without a response: {result:?}"
                    );
                    let _ = closed.send(());
                }
            }
        } else {
            sender.send(request).await.unwrap();
            reply(&mut socket, 200, "{}").await;
        }
    } else if path.ends_with("bind") {
        sender.send(request).await.unwrap();
        // Separate c and S frames ensure the actor waits for both identifiers.
        let body = format!(
            "{}{}",
            frame(json!([[0, ["c", format!("sid-{device}")]]])),
            frame(json!([[1, ["S", format!("session-{device}")]]]))
        );
        reply(&mut socket, 200, &body).await;
    } else {
        panic!("unexpected loopback request path {path}");
    }
}

/// A production-runtime smoke scenario with independent devices, ad controls,
/// reporting, and clean cancellation. Callers can replace the spawn with run_app
/// while reusing Fixture and its observable wire requests.
pub async fn run() {
    let mut fixture = Fixture::start(true).await;
    let config = fixture.config(&["screen-a", "screen-b"]);
    let shutdown = CancellationToken::new();
    let service = tokio::spawn(isponsorblocktv::runtime::run(
        config,
        fixture.endpoints.clone(),
        false,
        shutdown.clone(),
    ));
    fixture.subscribed("screen-a").await;
    fixture.subscribed("screen-b").await;
    fixture
        .events("screen-a", vec![now_playing("video-a", 0.0, 1)])
        .await;
    let first = fixture.command("seekTo", "screen-a").await;
    assert_eq!(first.form["req0_newTime"], "5");
    fixture.events("screen-a", vec![state(5.0, 1)]).await;
    fixture
        .events("screen-b", vec![now_playing("video-b", 0.0, 1)])
        .await;
    let second = fixture.command("seekTo", "screen-b").await;
    second.respond(200);
    assert_eq!(fixture.report().await.query["UUID"], "uuid-video-b");
    first.respond(200);
    assert_eq!(fixture.report().await.query["UUID"], "uuid-video-a");
    fixture.events("screen-b", vec![json!(["onAdStateChange", {"adState":"1","currentTime":"0","isSkipEnabled":"true"}])]).await;
    fixture.command("skipAd", "screen-b").await;
    let mute = fixture.command("setVolume", "screen-b").await;
    assert_eq!(mute.form["req0_muted"], "true");
    fixture
        .events(
            "screen-b",
            vec![
                json!(["onVolumeChanged", {"volume":"37","muted":"true"}]),
                json!(["onAdStateChange", {"adState":"0","currentTime":"1"}]),
            ],
        )
        .await;
    let restore = fixture.command("setVolume", "screen-b").await;
    assert_eq!(restore.form["req0_muted"], "false");
    assert_eq!(restore.form["req0_volume"], "37");
    fixture
        .events(
            "screen-b",
            vec![json!(["onAutoplayModeChanged", {"autoplayMode":"DISABLED"}])],
        )
        .await;
    fixture.command("setAutoplayMode", "screen-b").await;
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(4), service)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    fixture.stop().await;
}
