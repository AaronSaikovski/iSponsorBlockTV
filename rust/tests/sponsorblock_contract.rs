//! Real HTTP requests against isolated native fixtures; never uses public APIs.
use futures_util::FutureExt;
use isponsorblocktv::{
    api::ApiService,
    config::{ConfigDocument, RuntimeConfig},
    http::Endpoints,
};
use serde_json::json;
use std::{future::Future, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    task::JoinHandle,
};
use url::Url;

struct Fixture {
    endpoints: Endpoints,
    requests: mpsc::Receiver<String>,
    task: JoinHandle<()>,
}
impl Fixture {
    async fn new(replies: Vec<(u16, String)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let endpoints = Endpoints {
            lounge: base.clone(),
            youtube: base.join("youtube/").unwrap(),
            sponsorblock: base.join("api/").unwrap(),
        };
        let (sender, requests) = mpsc::channel(64);
        let task = tokio::spawn(async move {
            for (status, body) in replies {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut chunk = [0; 1024];
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert_ne!(n, 0);
                    bytes.extend_from_slice(&chunk[..n]);
                    if bytes.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                    assert!(bytes.len() < 16384);
                }
                sender
                    .send(String::from_utf8(bytes).unwrap())
                    .await
                    .unwrap();
                socket.write_all(format!("HTTP/1.1 {status} Fixture\r\nConnection: close\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
        });
        Self {
            endpoints,
            requests,
            task,
        }
    }
    fn service(&self, config: &RuntimeConfig) -> ApiService {
        ApiService::new(config, self.endpoints.clone(), false).unwrap()
    }
    async fn request(&mut self) -> String {
        tokio::time::timeout(Duration::from_secs(2), self.requests.recv())
            .await
            .unwrap()
            .unwrap()
    }
    async fn finish(self) {
        tokio::time::timeout(Duration::from_secs(2), self.task)
            .await
            .unwrap()
            .unwrap();
    }
}

struct DeferredFixture {
    endpoints: Endpoints,
    listener: TcpListener,
}

struct DeferredRequest {
    headers: String,
    socket: TcpStream,
}

impl DeferredFixture {
    async fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        Self {
            endpoints: Endpoints {
                lounge: base.clone(),
                youtube: base.join("youtube/").unwrap(),
                sponsorblock: base.join("api/").unwrap(),
            },
            listener,
        }
    }

    fn service(&self) -> ApiService {
        ApiService::new(&config(), self.endpoints.clone(), false).unwrap()
    }

    async fn request_while<F: Future + Unpin>(&self, lookup: &mut F) -> DeferredRequest {
        tokio::time::timeout(Duration::from_secs(2), async {
            tokio::select! {
                _ = lookup => panic!("lookup completed before its deferred response"),
                request = async {
                    let (mut socket, _) = self.listener.accept().await.unwrap();
                    let mut bytes = Vec::new();
                    loop {
                        let mut chunk = [0; 1024];
                        let count = socket.read(&mut chunk).await.unwrap();
                        assert_ne!(count, 0);
                        bytes.extend_from_slice(&chunk[..count]);
                        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                        assert!(bytes.len() < 16384);
                    }
                    DeferredRequest {
                        headers: String::from_utf8(bytes).unwrap(),
                        socket,
                    }
                } => request,
            }
        })
        .await
        .expect("lookup did not issue an HTTP request")
    }

    async fn assert_no_request_while<F: Future + Unpin>(&self, lookup: &mut F) {
        tokio::select! {
            _ = lookup => panic!("lookup completed before its deferred response"),
            request = tokio::time::timeout(Duration::from_millis(50), self.listener.accept()) => {
                assert!(request.is_err(), "unexpected additional HTTP request");
            }
        }
    }
}

impl DeferredRequest {
    async fn respond(mut self, status: u16, body: String) {
        self.socket
            .write_all(
                format!(
                    "HTTP/1.1 {status} Fixture\r\nConnection: close\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
    }

    async fn assert_closed(mut self) {
        let mut byte = [0];
        let result = tokio::time::timeout(Duration::from_secs(2), self.socket.read(&mut byte))
            .await
            .expect("cancelled lookup left its HTTP request running");
        match result {
            Ok(0) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                ) => {}
            result => panic!("expected a closed HTTP connection, got {result:?}"),
        }
    }
}
fn config() -> RuntimeConfig {
    serde_json::from_value::<ConfigDocument>(json!({"devices":[{"screen_id":"fixture"}]}))
        .unwrap()
        .runtime_config()
        .unwrap()
}
fn bucket(video: &str, locked: u8) -> String {
    json!([{"videoID":video,"segments":[{"segment":[0,2],"UUID":"uuid-a","locked":locked}]}])
        .to_string()
}
fn target(request: &str) -> Url {
    Url::parse(&format!(
        "http://fixture{}",
        request.split_whitespace().nth(1).unwrap()
    ))
    .unwrap()
}

#[tokio::test]
async fn exact_hash_categories_accept_header_and_reporting() {
    let mut fixture = Fixture::new(vec![
        (200, bucket("abc", 1)),
        (200, "".into()),
        (204, "".into()),
    ])
    .await;
    let mut options = config();
    options.skip_categories = ["hook", "exclusive_access", "future_category"]
        .map(String::from)
        .to_vec();
    let api = fixture.service(&options);
    let segments = api.segments("abc").await.unwrap();
    assert_eq!(segments[0].start, 0.0);
    let request = fixture.request().await;
    assert!(request.starts_with("GET /api/skipSegments/ba78?"));
    assert!(
        request
            .to_lowercase()
            .contains("accept: application/json\r\n")
    );
    assert_eq!(
        target(&request).query_pairs().collect::<Vec<_>>(),
        vec![
            ("category".into(), "hook".into()),
            ("category".into(), "exclusive_access".into()),
            ("category".into(), "future_category".into()),
            ("actionType".into(), "skip".into()),
            ("service".into(), "youtube".into())
        ]
    );
    let cached = api.clone().segments("abc").await.unwrap();
    assert!(Arc::ptr_eq(&segments, &cached));
    api.report_viewed(&["uuid-a".into(), "uuid-a".into(), "uuid&b".into()])
        .await
        .unwrap();
    for uuid in ["uuid-a", "uuid&b"] {
        let request = fixture.request().await;
        assert!(request.starts_with("POST /api/viewedVideoSponsorTime/?"));
        assert_eq!(
            target(&request).query_pairs().collect::<Vec<_>>(),
            vec![("UUID".into(), uuid.into())]
        );
    }
    fixture.finish().await;
}

#[tokio::test]
async fn errors_are_not_cached_and_selected_data_is_validated() {
    let malformed = json!([{"videoID":"v","segments":[{"segment":[0,2],"UUID":"a","locked":1},{"segment":[2],"UUID":"b","locked":1}]}]).to_string();
    let mut fixture = Fixture::new(vec![
        (500, "untrusted secret".into()),
        (200, malformed),
        (200, bucket("v", 0)),
    ])
    .await;
    let api = fixture.service(&config());
    let error = api.segments("v").await.unwrap_err();
    assert!(!error.to_string().contains("secret"));
    assert!(api.segments("v").await.is_err());
    assert_eq!(api.segments("v").await.unwrap().len(), 1);
    for _ in 0..3 {
        fixture.request().await;
    }
    fixture.finish().await;
}

#[tokio::test]
async fn locked_survives_and_other_successes_expire_exactly_at_boundary() {
    // Pause only between requests: paused-time auto-advance must not race TCP
    // readiness with reqwest's request deadline.
    let mut fixture = Fixture::new(vec![
        (200, bucket("locked", 1)),
        (200, bucket("unlocked", 0)),
        (404, "".into()),
        (200, "[]".into()),
        (200, bucket("unlocked", 0)),
        (200, "[]".into()),
        (200, "[]".into()),
    ])
    .await;
    let api = fixture.service(&config());
    let locked = api.segments("locked").await.unwrap();
    let unlocked = api.segments("unlocked").await.unwrap();
    let empty = api.segments("empty").await.unwrap();
    let absent = api.segments("absent").await.unwrap();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(299)).await;
    assert!(Arc::ptr_eq(
        &unlocked,
        &api.segments("unlocked").await.unwrap()
    ));
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::time::resume();
    assert!(Arc::ptr_eq(&locked, &api.segments("locked").await.unwrap()));
    assert!(!Arc::ptr_eq(
        &unlocked,
        &api.segments("unlocked").await.unwrap()
    ));
    assert!(!Arc::ptr_eq(&empty, &api.segments("empty").await.unwrap()));
    assert!(!Arc::ptr_eq(
        &absent,
        &api.segments("absent").await.unwrap()
    ));
    for _ in 0..7 {
        fixture.request().await;
    }
    fixture.finish().await;
}

#[tokio::test]
async fn eleventh_entry_evicts_least_recently_used() {
    let mut replies: Vec<_> = (0..11).map(|i| (200, bucket(&i.to_string(), 1))).collect();
    replies.push((200, bucket("1", 1)));
    let mut fixture = Fixture::new(replies).await;
    let api = fixture.service(&config());
    let first = api.segments("0").await.unwrap();
    for i in 1..10 {
        api.segments(&i.to_string()).await.unwrap();
    }
    assert!(Arc::ptr_eq(&first, &api.segments("0").await.unwrap()));
    api.segments("10").await.unwrap();
    api.segments("1").await.unwrap();
    for _ in 0..12 {
        fixture.request().await;
    }
    fixture.finish().await;
}

#[tokio::test]
async fn empty_categories_and_disabled_tracking_do_not_connect() {
    let fixture = Fixture::new(vec![]).await;
    let mut options = config();
    options.skip_categories.clear();
    options.skip_count_tracking = false;
    let api = fixture.service(&options);
    assert!(api.segments("v").await.unwrap().is_empty());
    api.report_viewed(&["uuid".into()]).await.unwrap();
    fixture.finish().await;
}

#[tokio::test]
async fn whitelist_bypasses_segments_only_and_is_reused() {
    let mut fixture = Fixture::new(vec![
        (
            200,
            json!({"items":[{"snippet":{"channelId":"allowed"}}]}).to_string(),
        ),
        (200, "".into()),
    ])
    .await;
    let mut options = config();
    options.apikey = "secret".into();
    options.channel_whitelist = serde_json::from_value::<ConfigDocument>(json!({"devices":[{"screen_id":"fixture"}],"apikey":"secret","channel_whitelist":[{"id":"allowed"}]})).unwrap().runtime_config().unwrap().channel_whitelist;
    let api = fixture.service(&options);
    assert!(api.segments("v").await.unwrap().is_empty());
    assert!(api.clone().segments("v").await.unwrap().is_empty());
    let request = fixture.request().await;
    assert_eq!(target(&request).path(), "/youtube/videos");
    api.report_viewed(&["uuid".into()]).await.unwrap();
    assert!(
        fixture
            .request()
            .await
            .starts_with("POST /api/viewedVideoSponsorTime/")
    );
    fixture.finish().await;
}

#[tokio::test]
async fn reporting_failure_does_not_retry_or_skip_remaining_unique_uuids() {
    let mut fixture = Fixture::new(vec![(500, "secret".into()), (200, "".into())]).await;
    let api = fixture.service(&config());
    assert!(
        api.report_viewed(&["a".into(), "a".into(), "b".into()])
            .await
            .is_err()
    );
    assert_eq!(target(&fixture.request().await).query(), Some("UUID=a"));
    assert_eq!(target(&fixture.request().await).query(), Some("UUID=b"));
    fixture.finish().await;
}

#[tokio::test]
async fn all_locked_but_filtered_empty_still_expires() {
    let mut fixture = Fixture::new(vec![(200, bucket("v", 1)), (200, bucket("v", 1))]).await;
    let mut options = config();
    options.minimum_skip_length = 2.0;
    let api = fixture.service(&options);
    assert!(api.segments("v").await.unwrap().is_empty());
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(300)).await;
    tokio::time::resume();
    assert!(api.segments("v").await.unwrap().is_empty());
    fixture.request().await;
    fixture.request().await;
    fixture.finish().await;
}

#[tokio::test]
async fn concurrent_same_video_clones_share_one_request_and_cached_result() {
    let fixture = DeferredFixture::new().await;
    let api = fixture.service();
    let second = api.clone();
    let third = api.clone();
    let mut lookups =
        async { tokio::join!(api.segments("v"), second.segments("v"), third.segments("v")) }
            .boxed();
    let request = fixture.request_while(&mut lookups).await;
    fixture.assert_no_request_while(&mut lookups).await;
    request.respond(200, bucket("v", 0)).await;
    let (first, second, third) = tokio::time::timeout(Duration::from_secs(2), lookups)
        .await
        .unwrap();
    let first = first.unwrap();
    assert_eq!(first.len(), 1);
    assert!(Arc::ptr_eq(&first, &second.unwrap()));
    assert!(Arc::ptr_eq(&first, &third.unwrap()));
    assert!(Arc::ptr_eq(&first, &api.segments("v").await.unwrap()));
}

#[tokio::test]
async fn different_videos_issue_requests_before_either_response() {
    let fixture = DeferredFixture::new().await;
    let api = fixture.service();
    let other = api.clone();
    let mut lookups = async { tokio::join!(api.segments("a"), other.segments("b")) }.boxed();
    let first = fixture.request_while(&mut lookups).await;
    let second = fixture.request_while(&mut lookups).await;
    assert_ne!(
        target(&first.headers).path(),
        target(&second.headers).path()
    );
    let body = json!([
        {"videoID":"a","segments":[{"segment":[0,2],"UUID":"a","locked":0}]},
        {"videoID":"b","segments":[{"segment":[3,5],"UUID":"b","locked":0}]}
    ])
    .to_string();
    first.respond(200, body.clone()).await;
    second.respond(200, body).await;
    let (first, second) = tokio::time::timeout(Duration::from_secs(2), lookups)
        .await
        .unwrap();
    assert_eq!(first.unwrap()[0].uuids, ["a"]);
    assert_eq!(second.unwrap()[0].uuids, ["b"]);
}

#[tokio::test]
async fn cancelling_first_waiter_does_not_cancel_its_follower() {
    let fixture = DeferredFixture::new().await;
    let api = fixture.service();
    let other = api.clone();
    let mut first = api.segments("v").boxed();
    let request = fixture.request_while(&mut first).await;
    let mut follower = other.segments("v").boxed();
    assert!(futures_util::poll!(&mut follower).is_pending());
    drop(first);
    fixture.assert_no_request_while(&mut follower).await;
    request.respond(200, bucket("v", 0)).await;
    let segments = tokio::time::timeout(Duration::from_secs(2), follower)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(segments.len(), 1);
    assert!(Arc::ptr_eq(&segments, &api.segments("v").await.unwrap()));
}

#[tokio::test]
async fn cancelling_all_waiters_closes_connection_and_allows_fresh_retry() {
    let fixture = DeferredFixture::new().await;
    let api = fixture.service();
    let other = api.clone();
    let mut first = api.segments("v").boxed();
    let request = fixture.request_while(&mut first).await;
    let mut follower = other.segments("v").boxed();
    assert!(futures_util::poll!(&mut follower).is_pending());
    drop(first);
    fixture.assert_no_request_while(&mut follower).await;
    drop(follower);
    request.assert_closed().await;

    let mut retry = api.segments("v").boxed();
    let request = fixture.request_while(&mut retry).await;
    request.respond(200, bucket("v", 0)).await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), retry)
            .await
            .unwrap()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn shared_failure_is_not_cached_even_while_a_follower_has_not_resumed() {
    for (status, body) in [(500, "untrusted secret"), (200, "invalid secret JSON")] {
        let fixture = DeferredFixture::new().await;
        let api = fixture.service();
        let other = api.clone();
        let mut first = api.segments("v").boxed();
        let request = fixture.request_while(&mut first).await;
        let mut follower = other.segments("v").boxed();
        assert!(futures_util::poll!(&mut follower).is_pending());
        request.respond(status, body.into()).await;
        let first_error = tokio::time::timeout(Duration::from_secs(2), first)
            .await
            .unwrap()
            .unwrap_err();
        assert!(!first_error.to_string().contains("secret"));

        // The old follower still owns the failed shared result. An independent
        // caller must nevertheless start a new HTTP request immediately.
        let mut retry = api.segments("v").boxed();
        let request = fixture.request_while(&mut retry).await;
        request.respond(200, bucket("v", 0)).await;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), retry)
                .await
                .unwrap()
                .unwrap()
                .len(),
            1
        );
        let follower_error = tokio::time::timeout(Duration::from_secs(2), follower)
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(first_error.to_string(), follower_error.to_string());
    }
}

#[tokio::test]
async fn expired_cache_entry_coalesces_concurrent_refreshes() {
    let fixture = DeferredFixture::new().await;
    let api = fixture.service();
    let mut initial = api.segments("v").boxed();
    let request = fixture.request_while(&mut initial).await;
    request.respond(200, bucket("v", 0)).await;
    let cached = tokio::time::timeout(Duration::from_secs(2), initial)
        .await
        .unwrap()
        .unwrap();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(300)).await;
    tokio::time::resume();

    let other = api.clone();
    let mut lookups = async { tokio::join!(api.segments("v"), other.segments("v")) }.boxed();
    let request = fixture.request_while(&mut lookups).await;
    fixture.assert_no_request_while(&mut lookups).await;
    request.respond(200, bucket("v", 0)).await;
    let (first, second) = tokio::time::timeout(Duration::from_secs(2), lookups)
        .await
        .unwrap();
    let refreshed = first.unwrap();
    assert!(!Arc::ptr_eq(&cached, &refreshed));
    assert!(Arc::ptr_eq(&refreshed, &second.unwrap()));
    assert!(Arc::ptr_eq(&refreshed, &api.segments("v").await.unwrap()));
}
