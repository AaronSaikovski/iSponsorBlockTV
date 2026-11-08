//! Native loopback contracts: no public services or process-global environment edits.
use std::{collections::BTreeMap, time::Duration};

use isponsorblocktv::{
    api::{ApiClient, ApiError, ApiOptions},
    http::{Endpoints, HttpClients, HttpError, MAX_BODY_BYTES, redacted_url},
};
use reqwest::Method;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::mpsc,
    task::JoinHandle,
};
use url::Url;

struct Request {
    method: String,
    target: String,
    headers: BTreeMap<String, String>,
    body: String,
}

impl Request {
    fn query(&self) -> BTreeMap<String, String> {
        Url::parse(&format!("http://fixture{}", self.target))
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect()
    }
}

struct Reply {
    status: u16,
    body: String,
    headers: Vec<(String, String)>,
    chunked: bool,
}

fn json(status: u16, body: &str) -> Reply {
    Reply {
        status,
        body: body.to_owned(),
        headers: vec![],
        chunked: false,
    }
}

struct Fixture {
    endpoints: Endpoints,
    requests: mpsc::Receiver<Request>,
    server: JoinHandle<()>,
}

impl Fixture {
    async fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let endpoints = Endpoints {
            lounge: base.join("api/lounge").unwrap(), // deliberately no trailing slash
            youtube: base.join("youtube/v3/").unwrap(),
            sponsorblock: base.join("sponsorblock/").unwrap(),
        };
        let (sender, requests) = mpsc::channel(512);
        let server = tokio::spawn(async move {
            for reply in replies {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let header_end = loop {
                    let mut chunk = [0u8; 2048];
                    let read = socket.read(&mut chunk).await.unwrap();
                    assert_ne!(read, 0, "request ended before headers");
                    bytes.extend_from_slice(&chunk[..read]);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        break end + 4;
                    }
                    assert!(bytes.len() < 16384, "request headers exceed fixture limit");
                };
                let text = std::str::from_utf8(&bytes[..header_end]).unwrap();
                let mut lines = text.split("\r\n");
                let mut first = lines.next().unwrap().split_whitespace();
                let method = first.next().unwrap().to_owned();
                let target = first.next().unwrap().to_owned();
                let headers: BTreeMap<_, _> = lines
                    .filter_map(|line| line.split_once(':'))
                    .map(|(key, value)| (key.to_ascii_lowercase(), value.trim().to_owned()))
                    .collect();
                let length = headers
                    .get("content-length")
                    .map_or(0, |v| v.parse::<usize>().unwrap());
                while bytes.len() < header_end + length {
                    let mut chunk = [0u8; 2048];
                    let read = socket.read(&mut chunk).await.unwrap();
                    assert_ne!(read, 0, "request ended before body");
                    bytes.extend_from_slice(&chunk[..read]);
                }
                sender
                    .send(Request {
                        method,
                        target,
                        headers,
                        body: String::from_utf8(bytes[header_end..header_end + length].to_vec())
                            .unwrap(),
                    })
                    .await
                    .unwrap();
                let mut wire = format!(
                    "HTTP/1.1 {} Fixture\r\nConnection: close\r\nContent-Type: application/json\r\n",
                    reply.status
                );
                if reply.chunked {
                    wire.push_str("Transfer-Encoding: chunked\r\n");
                } else if !reply
                    .headers
                    .iter()
                    .any(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                {
                    wire.push_str(&format!("Content-Length: {}\r\n", reply.body.len()));
                }
                for (key, value) in reply.headers {
                    wire.push_str(&format!("{key}: {value}\r\n"));
                }
                wire.push_str("\r\n");
                if reply.chunked {
                    // A boundary crossing proves the streaming cap is not just
                    // a check of Content-Length (which is absent here).
                    for chunk in reply.body.as_bytes().chunks(8192) {
                        wire.push_str(&format!("{:x}\r\n", chunk.len()));
                        wire.push_str(std::str::from_utf8(chunk).unwrap());
                        wire.push_str("\r\n");
                    }
                    wire.push_str("0\r\n\r\n");
                } else {
                    wire.push_str(&reply.body);
                }
                // A bounded-body client may intentionally close early.
                let _ = socket.write_all(wire.as_bytes()).await;
            }
        });
        Self {
            endpoints,
            requests,
            server,
        }
    }

    fn api(&self) -> ApiClient {
        self.api_with(ApiOptions {
            apikey: "fixture-secret-key".into(),
            ..ApiOptions::default()
        })
    }

    fn api_with(&self, options: ApiOptions) -> ApiClient {
        ApiClient::new(options, self.endpoints.clone()).unwrap()
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
async fn pairing_preserves_leading_zeroes_and_posts_exact_form() {
    let mut fixture = Fixture::start(vec![json(200,
        r#"{"screen":{"screenId":"fixture-screen-secret","name":"Living room","loungeToken":"fixture-token-secret"}}"#)]).await;
    let api = fixture.api();
    let result = api.pair_with_code("0012-3456 7890").await.unwrap().unwrap();
    assert_eq!(result.screen_id, "fixture-screen-secret");
    assert_eq!(result.name.as_deref(), Some("Living room"));
    assert_eq!(result.lounge_token.as_deref(), Some("fixture-token-secret"));
    let request = fixture.next().await;
    assert_eq!(request.method, "POST");
    assert_eq!(request.target, "/api/lounge/pairing/get_screen");
    assert_eq!(request.body, "pairing_code=001234567890");
    assert_eq!(
        request.headers["content-type"],
        "application/x-www-form-urlencoded"
    );
    let debug = format!("{result:?}");
    assert!(!debug.contains("fixture-screen-secret"));
    assert!(!debug.contains("fixture-token-secret"));
    fixture.finish().await;
}

#[tokio::test]
async fn pairing_distinguishes_rejection_malformed_and_service_failure() {
    let fixture = Fixture::start(vec![
        json(400, "rejected-code-secret"),
        json(404, "not JSON"),
        json(500, "upstream-secret"),
        json(200, "not JSON secret"),
        json(200, r#"{"screen":{}}"#),
        json(200, r#"{"screen":{"screenId":""}}"#),
        json(200, r#"{"screen":{"screenId":123}}"#),
        json(
            200,
            r#"{"screen":{"screenId":"valid","name":null,"loungeToken":null}}"#,
        ),
    ])
    .await;
    let api = fixture.api();
    assert!(api.pair_with_code("123").await.unwrap().is_none());
    assert!(api.pair_with_code("123").await.unwrap().is_none());
    let error = api.pair_with_code("private-code").await.unwrap_err();
    assert!(matches!(error, ApiError::Status { status: 500, .. }));
    assert!(!format!("{error:?} {error}").contains("secret"));
    for _ in 0..4 {
        assert!(matches!(
            api.pair_with_code("123").await,
            Err(ApiError::MalformedResponse { .. })
        ));
    }
    let result = api.pair_with_code("123").await.unwrap().unwrap();
    assert_eq!(result.name, None);
    assert_eq!(result.lounge_token, None);
    fixture.finish().await;
}

#[tokio::test]
async fn search_uses_exact_parameters_statistics_and_cache() {
    let mut fixture = Fixture::start(vec![
        json(200, r#"{"items":[{"snippet":{"channelId":"channel-a","channelTitle":"A <literal>"}},{"snippet":{"channelId":"channel-b","channelTitle":"B"}},{"snippet":{"channelId":"channel-c","channelTitle":"C"}}]}"#),
        json(200, r#"{"items":[{"statistics":{"hiddenSubscriberCount":false,"subscriberCount":"001234567"}}]}"#),
        json(200, r#"{"items":[{"statistics":{"hiddenSubscriberCount":true}}]}"#),
        json(200, r#"{"items":[]}"#),
    ]).await;
    let api = fixture.api();
    let results = api.search_channels("a & b").await.unwrap();
    assert_eq!(results.len(), 3);
    assert_eq!(results[0].name, "A <literal>");
    assert_eq!(results[0].subscribers, "1_234_567");
    assert_eq!(results[1].subscribers, "Hidden");
    assert_eq!(results[2].subscribers, "Hidden");
    assert_eq!(api.search_channels("a & b").await.unwrap(), results);
    let search = fixture.next().await;
    assert_eq!(search.method, "GET");
    assert!(search.target.starts_with("/youtube/v3/search?"));
    assert_eq!(
        search.query(),
        BTreeMap::from([
            ("q".into(), "a & b".into()),
            ("key".into(), "fixture-secret-key".into()),
            ("part".into(), "snippet".into()),
            ("type".into(), "channel".into()),
            ("maxResults".into(), "5".into()),
        ])
    );
    for id in ["channel-a", "channel-b", "channel-c"] {
        let details = fixture.next().await;
        assert!(details.target.starts_with("/youtube/v3/channels?"));
        assert_eq!(
            details.query(),
            BTreeMap::from([
                ("id".into(), id.into()),
                ("part".into(), "statistics".into()),
                ("key".into(), "fixture-secret-key".into()),
            ])
        );
    }
    fixture.finish().await;
}

#[tokio::test]
async fn empty_items_and_changed_key_have_independent_caches() {
    let mut fixture = Fixture::start(vec![
        json(200, r#"{"items":[]}"#),
        json(200, r#"{"items":[]}"#),
        json(200, r#"{"items":[]}"#),
    ])
    .await;
    let api = fixture.api();
    assert!(api.search_channels("empty").await.unwrap().is_empty());
    assert!(api.search_channels("empty").await.unwrap().is_empty());
    assert_eq!(api.channel_for_video("video").await.unwrap(), None);
    let changed = fixture.api_with(ApiOptions {
        apikey: "new-key".into(),
        ..ApiOptions::default()
    });
    assert!(changed.search_channels("empty").await.unwrap().is_empty());
    assert_eq!(fixture.next().await.query()["key"], "fixture-secret-key");
    let video = fixture.next().await;
    assert!(video.target.starts_with("/youtube/v3/videos?"));
    assert_eq!(video.query()["id"], "video");
    assert_eq!(video.query()["part"], "snippet");
    assert_eq!(fixture.next().await.query()["key"], "new-key");
    fixture.finish().await;
}

#[tokio::test]
async fn errors_are_not_cached_or_confused_with_empty_results() {
    let fixture = Fixture::start(vec![
        json(500, "secret not JSON"),
        json(200, r#"{"items":[]}"#),
        json(200, r#"{"error":{"message":"private response text"}}"#),
        json(200, "{}"),
        json(200, r#"{"items":null}"#),
        json(200, r#"{"items":[{"snippet":{"channelId":""}}]}"#),
        json(200, r#"{"items":[{"snippet":{"channelId":"channel"}}]}"#),
    ])
    .await;
    let api = fixture.api();
    assert!(matches!(
        api.search_channels("retry").await,
        Err(ApiError::Status { status: 500, .. })
    ));
    assert!(api.search_channels("retry").await.unwrap().is_empty());
    let error = api.channel_for_video("v").await.unwrap_err();
    assert!(matches!(error, ApiError::YoutubeRejected));
    assert!(!format!("{error:?} {error}").contains("private response text"));
    for _ in 0..3 {
        assert!(matches!(
            api.channel_for_video("v").await,
            Err(ApiError::MalformedResponse { .. })
        ));
    }
    assert_eq!(
        api.channel_for_video("v").await.unwrap().as_deref(),
        Some("channel")
    );
    fixture.finish().await;
}

#[tokio::test]
async fn malformed_subscriber_counts_fail_without_caching_partial_searches() {
    let search = r#"{"items":[{"snippet":{"channelId":"channel","channelTitle":"Channel"}}]}"#;
    let fixture = Fixture::start(vec![
        json(200, search), json(200, r#"{"items":[{"statistics":{"hiddenSubscriberCount":false,"subscriberCount":"-1"}}]}"#),
        json(200, search), json(200, r#"{"items":[{"statistics":{"hiddenSubscriberCount":false,"subscriberCount":"18446744073709551617"}}]}"#),
    ]).await;
    let api = fixture.api();
    assert!(matches!(
        api.search_channels("q").await,
        Err(ApiError::MalformedResponse { .. })
    ));
    assert_eq!(
        api.search_channels("q").await.unwrap()[0].subscribers,
        "18_446_744_073_709_551_617"
    );
    fixture.finish().await;
}

#[tokio::test]
async fn search_lru_evicts_the_least_recent_of_ten_entries() {
    let mut fixture = Fixture::start((0..12).map(|_| json(200, r#"{"items":[]}"#)).collect()).await;
    let api = fixture.api();
    for index in 0..10 {
        api.search_channels(&format!("q{index}")).await.unwrap();
    }
    api.search_channels("q0").await.unwrap(); // q1 is now least recently used
    api.search_channels("q10").await.unwrap();
    api.search_channels("q1").await.unwrap();
    for index in (0..10).chain([10, 1]) {
        assert_eq!(fixture.next().await.query()["q"], format!("q{index}"));
    }
    fixture.finish().await;
}

#[tokio::test]
async fn whitelist_is_bounded_and_scoped_to_immutable_options() {
    let body = r#"{"items":[{"snippet":{"channelId":"allowed"}}]}"#;
    let mut fixture = Fixture::start((0..103).map(|_| json(200, body)).collect()).await;
    let options = ApiOptions {
        apikey: "key".into(),
        channel_whitelist: vec!["allowed".into()],
        ..ApiOptions::default()
    };
    let api = fixture.api_with(options.clone());
    for index in 0..100 {
        assert!(api.is_whitelisted(&format!("v{index}")).await.unwrap());
    }
    assert!(api.is_whitelisted("v0").await.unwrap());
    assert!(api.is_whitelisted("v100").await.unwrap());
    assert!(api.is_whitelisted("v1").await.unwrap());
    let changed = fixture.api_with(ApiOptions {
        channel_whitelist: vec!["different".into()],
        ..options
    });
    assert!(!changed.is_whitelisted("v0").await.unwrap());
    for index in (0..100).chain([100, 1, 0]) {
        assert_eq!(fixture.next().await.query()["id"], format!("v{index}"));
    }
    fixture.finish().await;
}

#[tokio::test]
async fn whitelist_errors_retry_and_disabled_whitelist_needs_no_key_or_network() {
    let fixture = Fixture::start(vec![
        json(503, "bad"),
        json(200, r#"{"items":[{"snippet":{"channelId":"allowed"}}]}"#),
    ])
    .await;
    let disabled = fixture.api_with(ApiOptions::default());
    assert!(!disabled.is_whitelisted("v").await.unwrap());
    assert!(matches!(
        disabled.search_channels("q").await,
        Err(ApiError::MissingApiKey)
    ));
    let api = fixture.api_with(ApiOptions {
        apikey: "key".into(),
        channel_whitelist: vec!["allowed".into()],
        ..ApiOptions::default()
    });
    assert!(api.is_whitelisted("v").await.is_err());
    assert!(api.is_whitelisted("v").await.unwrap());
    fixture.finish().await;
}

#[tokio::test]
async fn declared_and_chunked_bodies_enforce_the_same_limit() {
    let mut declared = json(200, "");
    declared
        .headers
        .push(("Content-Length".into(), (MAX_BODY_BYTES + 1).to_string()));
    let mut chunked = json(200, &"x".repeat(MAX_BODY_BYTES + 1));
    chunked.chunked = true;
    let fixture = Fixture::start(vec![declared, chunked]).await;
    let api = fixture.api();
    for _ in 0..2 {
        assert!(matches!(
            api.pair_with_code("123").await,
            Err(ApiError::Http(HttpError::BodyTooLarge))
        ));
    }
    fixture.finish().await;
}

#[tokio::test]
async fn redirects_are_not_followed_and_mutating_errors_are_not_retried() {
    let mut redirect = json(302, "not JSON");
    redirect
        .headers
        .push(("Location".into(), "http://127.0.0.1:1/secret-target".into()));
    let mut fixture = Fixture::start(vec![redirect, json(503, "unavailable")]).await;
    let api = fixture.api();
    assert!(matches!(
        api.pair_with_code("secret-code").await,
        Err(ApiError::Status { status: 302, .. })
    ));
    assert!(matches!(
        api.pair_with_code("secret-code").await,
        Err(ApiError::Status { status: 503, .. })
    ));
    assert_eq!(fixture.next().await.method, "POST");
    assert_eq!(fixture.next().await.method, "POST");
    fixture.finish().await;
}

#[tokio::test]
async fn transport_errors_never_expose_secrets() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let url = Url::parse(&format!("http://{address}/private-path/")).unwrap();
    let api = ApiClient::new(
        ApiOptions {
            apikey: "secret-key".into(),
            ..ApiOptions::default()
        },
        Endpoints {
            lounge: url.clone(),
            youtube: url.clone(),
            sponsorblock: url,
        },
    )
    .unwrap();
    let error = api.channel_for_video("private-video").await.unwrap_err();
    assert!(matches!(error, ApiError::Http(HttpError::Transport)));
    let printed = format!("{error:?} {error}");
    for secret in [
        "private-path",
        "secret-key",
        "private-video",
        &address.to_string(),
    ] {
        assert!(!printed.contains(secret));
    }
}

#[test]
fn endpoints_and_trace_metadata_do_not_expose_secrets() {
    for url in [
        "ftp://example.test/",
        "https://user:password@example.test/",
        "https://example.test/?key=secret",
        "https://example.test/#secret",
    ] {
        let endpoints = Endpoints {
            youtube: Url::parse(url).unwrap(),
            ..Endpoints::default()
        };
        assert!(matches!(
            endpoints.normalized(),
            Err(HttpError::InvalidEndpoint)
        ));
    }
    let url = Url::parse("https://user:password@secret-host.test/private-screen/search?key=private-key&pairing_code=123#private-token").unwrap();
    assert_eq!(redacted_url(&url), "https://[redacted]/search");
    assert_eq!(
        redacted_url(&Url::parse("https://example.test/private-screen").unwrap()),
        "https://[redacted]/[redacted]"
    );
    let options = ApiOptions {
        apikey: "private-key".into(),
        ..ApiOptions::default()
    };
    assert!(!format!("{options:?}").contains("private-key"));
}

#[tokio::test]
async fn dial_has_no_redirects_and_cast_tls_policy_is_lan_scoped() {
    let mut reply = json(307, "redirect");
    reply
        .headers
        .push(("Location".into(), "http://127.0.0.1:1/".into()));
    let fixture = Fixture::start(vec![reply]).await;
    let clients = HttpClients::new(false, false).unwrap();
    let response = clients
        .dial
        .execute(
            clients
                .dial
                .request(Method::GET, fixture.endpoints.youtube.clone()),
        )
        .await
        .unwrap();
    assert_eq!(response.status.as_u16(), 307);
    for url in [
        "https://example.test/",
        "https://8.8.8.8/",
        "https://127.0.0.1/",
        "https://0.0.0.0/",
        "https://224.0.0.1/",
        "https://user:secret@192.168.1.2/",
        "https://192.168.1.2/#secret",
    ] {
        assert!(
            clients
                .cast_local
                .metadata_request(Method::GET, Url::parse(url).unwrap())
                .is_err()
        );
    }
    assert!(
        clients
            .cast_local
            .metadata_request(
                Method::GET,
                Url::parse("https://192.168.1.2:8443/setup/eureka_info?params=device_info,name")
                    .unwrap()
            )
            .is_ok()
    );
    fixture.finish().await;
}

// Test-only child processes isolate environment policy from parallel tests. The
// production executable has no endpoint/proxy test override.
#[tokio::test]
async fn proxy_policy_child_entry() {
    let Ok(base) = std::env::var("ISPBTV_API_TEST_ORIGIN") else {
        return;
    };
    let base = Url::parse(&base).unwrap();
    assert_eq!(base.host_str(), Some("127.0.0.1"));
    let endpoints = Endpoints {
        lounge: base.clone(),
        youtube: base.clone(),
        sponsorblock: base.clone(),
    };
    if std::env::var_os("ISPBTV_API_TEST_BYPASS").is_some() {
        let api = ApiClient::new(
            ApiOptions {
                apikey: "bypass-key".into(),
                use_proxy: true,
                ..ApiOptions::default()
            },
            endpoints,
        )
        .unwrap();
        assert!(api.search_channels("bypass").await.unwrap().is_empty());
        return;
    }
    let first = ApiClient::new(
        ApiOptions {
            apikey: "old-key".into(),
            ..ApiOptions::default()
        },
        endpoints.clone(),
    )
    .unwrap();
    assert!(first.search_channels("direct").await.unwrap().is_empty());
    let changed = ApiClient::new(
        ApiOptions {
            apikey: "new-key".into(),
            use_proxy: true,
            ..ApiOptions::default()
        },
        endpoints,
    )
    .unwrap();
    assert!(changed.search_channels("proxied").await.unwrap().is_empty());
    let clients = HttpClients::new(true, false).unwrap();
    assert_eq!(
        clients
            .dial
            .execute(clients.dial.request(Method::GET, base))
            .await
            .unwrap()
            .status
            .as_u16(),
        200
    );
}

#[tokio::test]
async fn proxy_environment_is_opt_in_and_never_used_for_dial() {
    let mut origin = Fixture::start((0..5).map(|_| json(200, r#"{"items":[]}"#)).collect()).await;
    let mut proxy = Fixture::start(vec![json(200, r#"{"items":[]}"#)]).await;
    for (bypass, lowercase) in [
        ("", false),
        ("*", false),
        ("127.0.0.1", false),
        (" example.test, * ", true),
    ] {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args(["--exact", "proxy_policy_child_entry", "--nocapture"])
            .env("ISPBTV_API_TEST_ORIGIN", origin.endpoints.youtube.as_str())
            .env("HTTP_PROXY", proxy.endpoints.youtube.as_str())
            .env("HTTPS_PROXY", proxy.endpoints.youtube.as_str())
            .env("ALL_PROXY", "")
            .env("http_proxy", "")
            .env("https_proxy", "")
            .env("all_proxy", "");
        if lowercase {
            child.env_remove("NO_PROXY").env("no_proxy", bypass);
        } else {
            child.env("NO_PROXY", bypass).env("no_proxy", "");
        }
        if !bypass.is_empty() {
            child.env("ISPBTV_API_TEST_BYPASS", "1");
        } else {
            child.env_remove("ISPBTV_API_TEST_BYPASS");
        }
        let status = tokio::task::spawn_blocking(move || child.status().unwrap())
            .await
            .unwrap();
        assert!(status.success(), "isolated proxy policy contract failed");
    }
    let direct = origin.next().await;
    assert_eq!(direct.query()["key"], "old-key");
    assert_eq!(direct.query()["q"], "direct");
    assert!(!direct.target.starts_with("http"));
    assert_eq!(origin.next().await.target, "/youtube/v3/");
    for _ in 0..3 {
        assert_eq!(origin.next().await.query()["q"], "bypass");
    }
    let proxied = proxy.next().await;
    let target = Url::parse(&proxied.target).unwrap();
    assert_eq!(
        target
            .query_pairs()
            .find(|(key, _)| key == "key")
            .unwrap()
            .1,
        "new-key"
    );
    assert_eq!(
        target.query_pairs().find(|(key, _)| key == "q").unwrap().1,
        "proxied"
    );
    origin.finish().await;
    proxy.finish().await;
}

#[tokio::test]
async fn ordinary_response_deadline_includes_body_consumption() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let (sent, headers_sent) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0u8; 4096];
        assert!(socket.read(&mut request).await.unwrap() > 0);
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nx")
            .await
            .unwrap();
        sent.send(()).unwrap();
        let _ = released.await;
    });
    let clients = HttpClients::new(false, false).unwrap();
    let request = clients.cloud.request(Method::GET, url);
    let client = tokio::spawn(async move { clients.cloud.execute(request).await });
    headers_sent.await.unwrap();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(11)).await;
    assert!(matches!(client.await.unwrap(), Err(HttpError::Timeout)));
    release.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn lounge_stream_has_no_ordinary_total_deadline() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0u8; 4096];
        assert!(socket.read(&mut request).await.unwrap() > 0);
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n")
            .await
            .unwrap();
        released.await.unwrap();
        socket.write_all(b"x").await.unwrap();
    });
    let clients = HttpClients::new(false, false).unwrap();
    let request = clients.lounge_stream.request(Method::GET, url);
    let mut response = clients.lounge_stream.execute_stream(request).await.unwrap();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(11)).await;
    release.send(()).unwrap();
    assert_eq!(response.chunk().await.unwrap().unwrap().as_ref(), b"x");
    assert!(response.chunk().await.unwrap().is_none());
    server.await.unwrap();
}

#[tokio::test]
async fn real_http_traces_contain_metadata_but_no_credentials_or_bodies() {
    use std::{
        io::Write,
        sync::{Arc, Mutex},
    };
    use tracing::instrument::WithSubscriber;

    #[derive(Clone)]
    struct Log(Arc<Mutex<Vec<u8>>>);
    impl Write for Log {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Log {
        type Writer = Log;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    let fixture = Fixture::start(vec![
        json(200, r#"{"screen":{"screenId":"sensitive-screen","loungeToken":"sensitive-token","name":"sensitive-name"}}"#),
        json(403, r#"{"error":{"message":"sensitive-response"}}"#),
    ]).await;
    let api = fixture.api_with(ApiOptions {
        apikey: "sensitive-key".into(),
        http_tracing: true,
        ..ApiOptions::default()
    });
    let log = Log(Arc::new(Mutex::new(Vec::new())));
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter("isponsorblocktv=debug")
        .with_ansi(false)
        .without_time()
        .with_writer(log.clone())
        .finish();
    async {
        assert!(api.pair_with_code("000011112222").await.unwrap().is_some());
        assert!(api.search_channels("sensitive-query").await.is_err());
    }
    .with_subscriber(subscriber)
    .await;
    let text = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
    for metadata in [
        "method=POST",
        "method=GET",
        "status=200",
        "status=403",
        "duration_ms=",
        "request_bytes=",
        "response_bytes=",
        "[redacted]/pairing/get_screen",
    ] {
        assert!(
            text.contains(metadata),
            "missing trace metadata: {metadata}"
        );
    }
    for secret in [
        "sensitive-screen",
        "sensitive-token",
        "sensitive-name",
        "sensitive-key",
        "sensitive-response",
        "sensitive-query",
        "000011112222",
    ] {
        assert!(!text.contains(secret), "trace included a secret");
    }
    fixture.finish().await;
}
