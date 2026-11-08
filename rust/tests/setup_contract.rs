use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use isponsorblocktv::config::{ConfigDocument, ConfigStore, SaveOutcome};
use isponsorblocktv::http::Endpoints;
use isponsorblocktv::setup::{PromptIo, SetupError, SetupOutcome, cli};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use url::Url;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

impl AsyncWrite for Capture {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Poll::Ready(Ok(bytes.len()))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

fn prompt(lines: &[&str]) -> (PromptIo, Capture) {
    let output = Capture::default();
    let input = format!("{}\n", lines.join("\n"));
    (
        PromptIo::new(io::Cursor::new(input.into_bytes()), output.clone()),
        output,
    )
}

fn offline_endpoints() -> Endpoints {
    let url = Url::parse("http://127.0.0.1:1/").unwrap();
    Endpoints {
        lounge: url.clone(),
        youtube: url.clone(),
        sponsorblock: url,
    }
}

#[tokio::test]
async fn defaults_require_explicit_save_and_allow_incomplete_draft() {
    let directory = tempfile::tempdir().unwrap();
    let (store, mut document) = ConfigStore::load(directory.path().to_owned(), false).unwrap();
    let (mut input, output) = prompt(&["", "", "", "", "", "", "", "", "", "", "", "y"]);
    let outcome = cli::run(
        &store,
        &mut document,
        offline_endpoints(),
        &mut input,
        CancellationToken::new(),
        false,
    )
    .await
    .unwrap();
    assert!(matches!(outcome, SetupOutcome::Saved(SaveOutcome::Saved)));
    assert_eq!(document.0["skip_categories"], json!([]));
    assert_eq!(document.0["skip_count_tracking"], true);
    assert_eq!(document.0["mute_ads"], false);
    assert_eq!(document.0["skip_ads"], false);
    assert_eq!(document.0["auto_play"], true);
    assert!(document.runtime_config().is_err());
    assert!(output.text().contains("Config saved to"));
    let (_, loaded) = ConfigStore::load(directory.path().to_owned(), false).unwrap();
    assert_eq!(loaded, document);

    let (mut input, output) = prompt(&["", "", "", "", "", "", "", "", "", "", "", "y"]);
    let outcome = cli::run(
        &store,
        &mut document,
        offline_endpoints(),
        &mut input,
        CancellationToken::new(),
        false,
    )
    .await
    .unwrap();
    assert!(matches!(
        outcome,
        SetupOutcome::Saved(SaveOutcome::Unchanged)
    ));
    assert!(output.text().contains("Config unchanged at"));
}

#[tokio::test]
async fn declining_final_save_or_eof_keeps_disk_unchanged() {
    let directory = tempfile::tempdir().unwrap();
    let (store, mut document) = ConfigStore::load(directory.path().to_owned(), false).unwrap();
    document
        .0
        .insert("unknown".into(), json!({"untouched": true}));
    store.save(&document).unwrap();
    let previous = std::fs::read(store.config_file()).unwrap();
    let (mut input, output) = prompt(&["", "", "", "sponsor", "", "", "", "", "", "", "", ""]);
    assert!(matches!(
        cli::run(
            &store,
            &mut document,
            offline_endpoints(),
            &mut input,
            CancellationToken::new(),
            false
        )
        .await
        .unwrap(),
        SetupOutcome::NotSaved
    ));
    assert!(output.text().contains("No config file was saved to"));
    assert_eq!(std::fs::read(store.config_file()).unwrap(), previous);

    let mut input = PromptIo::scripted(b"y\nn\nn\n".to_vec());
    assert!(matches!(
        cli::run(
            &store,
            &mut document,
            offline_endpoints(),
            &mut input,
            CancellationToken::new(),
            false
        )
        .await,
        Err(SetupError::Interrupted)
    ));
    assert_eq!(std::fs::read(store.config_file()).unwrap(), previous);
}

#[tokio::test]
async fn retained_unknown_values_names_offsets_and_categories_are_lossless() {
    let directory = tempfile::tempdir().unwrap();
    let (store, _) = ConfigStore::load(directory.path().to_owned(), false).unwrap();
    let mut document: ConfigDocument = serde_json::from_str(r#"{
        "future":18446744073709551617,
        "devices":[{"screen_id":"saved-screen","name":null,"offset":-250.5,"future":-18446744073709551617}],
        "channel_whitelist":[{"id":"existing","name":null,"future":{"value":18446744073709551617}}],
        "skip_categories":["hook","custom-category"],
        "join_name":"untouched",
        "minimum_skip_length":-1.25
    }"#).unwrap();
    let original = document.clone();
    let (mut input, _) = prompt(&["", "", "", "", "", "", "", "", "", "", "", "y"]);
    cli::run(
        &store,
        &mut document,
        offline_endpoints(),
        &mut input,
        CancellationToken::new(),
        false,
    )
    .await
    .unwrap();
    for key in [
        "future",
        "devices",
        "channel_whitelist",
        "skip_categories",
        "join_name",
        "minimum_skip_length",
    ] {
        assert_eq!(document.0[key], original.0[key], "{key}");
    }
    let (_, reloaded) = ConfigStore::load(directory.path().to_owned(), false).unwrap();
    assert_eq!(document, reloaded);
}

#[tokio::test]
async fn migration_is_explicit_and_does_not_save_early() {
    let directory = tempfile::tempdir().unwrap();
    let (store, mut document) = ConfigStore::load(directory.path().to_owned(), false).unwrap();
    document.0.insert("atvs".into(), json!(null));
    store.save(&document).unwrap();
    let before = std::fs::read(store.config_file()).unwrap();
    let mut input = PromptIo::scripted(b"\ny\n".to_vec());
    assert!(matches!(
        cli::run(
            &store,
            &mut document,
            offline_endpoints(),
            &mut input,
            CancellationToken::new(),
            false
        )
        .await,
        Err(SetupError::Interrupted)
    ));
    assert!(!document.0.contains_key("atvs"));
    assert_eq!(std::fs::read(store.config_file()).unwrap(), before);
}

#[tokio::test]
async fn no_key_custom_channel_and_validation_reprompts_remain_available() {
    let directory = tempfile::tempdir().unwrap();
    let (store, mut document) = ConfigStore::load(directory.path().to_owned(), false).unwrap();
    let (mut input, output) = prompt(&[
        "maybe",
        "n",
        "n",
        "n",
        "sponsor, hook\texclusive_access",
        "y",
        "query",
        "0",
        "5",
        "",
        "custom-id",
        "",
        "/exit",
        "y",
        "NaN",
        "1e9999",
        "1e-9999",
        "-0.5",
        "n",
        "y",
        "y",
        "n",
        "https://user:secret@example.invalid/api",
        "https://example.invalid/api?bad=1",
        "https://example.invalid/api",
        "y",
    ]);
    cli::run(
        &store,
        &mut document,
        offline_endpoints(),
        &mut input,
        CancellationToken::new(),
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        document.0["channel_whitelist"],
        json!([{"id":"custom-id","name":"custom-id"}])
    );
    assert_eq!(
        document.0["skip_categories"],
        json!(["sponsor", "hook", "exclusive_access"])
    );
    assert_eq!(document.0["minimum_skip_length"], json!(-0.5));
    assert_eq!(document.0["mute_ads"], true);
    assert_eq!(document.0["skip_ads"], true);
    assert_eq!(document.0["auto_play"], false);
    assert_eq!(
        document.0["sponsorblock_api_url"],
        "https://example.invalid/api"
    );
    assert!(output.text().contains("Invalid input"));
    assert!(output.text().contains("Invalid selection"));
    assert!(output.text().contains("Channel ID cannot be empty"));
    assert!(!output.text().contains("secret"));
}

struct Request {
    method: String,
    url: Url,
    body: String,
}

async fn request(stream: TcpStream) -> (TcpStream, Request) {
    let mut stream = BufReader::new(stream);
    let mut line = String::new();
    stream.read_line(&mut line).await.unwrap();
    let fields: Vec<_> = line.split_whitespace().collect();
    let method = fields[0].to_owned();
    let url = Url::parse(&format!("http://localhost{}", fields[1])).unwrap();
    let mut length = 0;
    loop {
        line.clear();
        stream.read_line(&mut line).await.unwrap();
        if line == "\r\n" {
            break;
        }
        if let Some((key, value)) = line.split_once(':')
            && key.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap();
        }
    }
    let mut body = vec![0; length];
    stream.read_exact(&mut body).await.unwrap();
    (
        stream.into_inner(),
        Request {
            method,
            url,
            body: String::from_utf8(body).unwrap(),
        },
    )
}

async fn response(stream: &mut TcpStream, status: u16, body: &str) {
    let response = format!(
        "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await.unwrap();
    stream.shutdown().await.unwrap();
}

fn endpoints(listener: &TcpListener) -> Endpoints {
    let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    Endpoints {
        lounge: base.join("lounge/").unwrap(),
        youtube: base.join("youtube/").unwrap(),
        sponsorblock: base.join("sponsorblock/").unwrap(),
    }
}

#[tokio::test]
async fn real_pairing_and_search_use_updated_options_and_custom_empty_result() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoints = endpoints(&listener);
    let server = tokio::spawn(async move {
        let mut observed = Vec::new();
        for body in [
            r#"{"screen":{"screenId":"paired-screen","name":"TV 雪","loungeToken":"transient-secret"}}"#,
            r#"{"items":[]}"#,
            r#"{"items":[{"id":{"channelId":"returned-id"},"snippet":{"channelId":"returned-id","title":"Returned channel","channelTitle":"Returned channel"}}]}"#,
            r#"{"items":[{"id":"returned-id","statistics":{"hiddenSubscriberCount":false,"subscriberCount":"1234567"}}]}"#,
        ] {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut stream, request) = request(stream).await;
            observed.push(request);
            response(&mut stream, 200, body).await;
        }
        observed
    });
    let directory = tempfile::tempdir().unwrap();
    let (store, mut document) = ConfigStore::load(directory.path().to_owned(), false).unwrap();
    document.0.insert("apikey".into(), json!("old-key"));
    document.0.insert("use_proxy".into(), json!(true));
    let (mut input, output) = prompt(&[
        "n",
        "y",
        "１２３４５６７８９０１２",
        "0012-3456 7890",
        "n",
        "y",
        "new-key",
        "sponsor",
        "y",
        "empty",
        "5",
        "custom-id",
        "Custom name",
        "found",
        "1",
        "0",
        "/exit",
        "n",
        "",
        "",
        "",
        "",
        "",
        "y",
    ]);
    tokio::time::timeout(
        Duration::from_secs(5),
        cli::run(
            &store,
            &mut document,
            endpoints,
            &mut input,
            CancellationToken::new(),
            true,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let requests = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].url.path(), "/lounge/pairing/get_screen");
    assert_eq!(requests[0].body, "pairing_code=001234567890");
    for request in &requests[1..] {
        assert_eq!(request.method, "GET");
        assert!(
            request
                .url
                .query_pairs()
                .any(|(key, value)| key == "key" && value == "new-key")
        );
        assert!(!request.url.query().unwrap().contains("old-key"));
    }
    assert_eq!(
        document.0["devices"][0],
        json!({"screen_id":"paired-screen", "name":"TV 雪", "offset":0})
    );
    assert_eq!(
        document.0["channel_whitelist"],
        json!([{"id":"custom-id","name":"Custom name"},{"id":"returned-id","name":"Returned channel"}])
    );
    assert_eq!(document.0["use_proxy"], false);
    assert!(document.runtime_config().is_ok());
    assert!(output.text().contains("1_234_567"));
    assert!(!output.text().contains("new-key"));
    assert!(!output.text().contains("paired-screen"));
    assert!(!output.text().contains("transient-secret"));
    assert!(
        !std::fs::read_to_string(store.config_file())
            .unwrap()
            .contains("transient-secret")
    );
}

#[tokio::test]
async fn http_failures_preserve_draft_and_allow_pairing_retry_and_manual_channel() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoints = endpoints(&listener);
    let server = tokio::spawn(async move {
        for (status, body) in [
            (400, r#"{"secret":"rejected-pairing-code"}"#),
            (200, "not-json-secret"),
            (200, r#"{"screen":{"screenId":"success"}}"#),
            (403, r#"{"error":{"message":"api-key-secret"}}"#),
        ] {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut stream, _) = request(stream).await;
            response(&mut stream, status, body).await;
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let (store, mut document) = ConfigStore::load(directory.path().to_owned(), false).unwrap();
    document.0.insert("apikey".into(), json!("key"));
    let (mut input, output) = prompt(&[
        "",
        "y",
        "000000000001",
        "y",
        "000000000002",
        "y",
        "000000000003",
        "n",
        "n",
        "",
        "y",
        "query",
        "5",
        "manual",
        "",
        "/exit",
        "",
        "",
        "",
        "",
        "",
        "",
        "y",
    ]);
    tokio::time::timeout(
        Duration::from_secs(5),
        cli::run(
            &store,
            &mut document,
            endpoints,
            &mut input,
            CancellationToken::new(),
            false,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(document.0["devices"].as_array().unwrap().len(), 1);
    assert_eq!(document.0["channel_whitelist"][0]["id"], "manual");
    assert!(output.text().contains("Code rejected"));
    assert!(output.text().contains("Pairing failed"));
    assert!(output.text().contains("Channel search failed"));
    for secret in ["rejected-pairing-code", "not-json-secret", "api-key-secret"] {
        assert!(!output.text().contains(secret));
    }
}

#[tokio::test]
async fn cancellation_interrupts_pending_input_and_backpressured_output() {
    let directory = tempfile::tempdir().unwrap();
    let (store, mut document) = ConfigStore::load(directory.path().to_owned(), false).unwrap();
    let (reader, _held_open) = tokio::io::duplex(64);
    let token = CancellationToken::new();
    let mut input = PromptIo::new(BufReader::new(reader), tokio::io::sink());
    let run = cli::run(
        &store,
        &mut document,
        offline_endpoints(),
        &mut input,
        token.clone(),
        false,
    );
    let cancel = async {
        tokio::task::yield_now().await;
        token.cancel();
    };
    let (result, ()) = tokio::join!(run, cancel);
    assert!(matches!(result, Err(SetupError::Interrupted)));
    assert!(!store.config_file().exists());

    let (writer, _blocked_reader) = tokio::io::duplex(1);
    let token = CancellationToken::new();
    let mut input = PromptIo::new(io::Cursor::new(Vec::new()), writer);
    let write = input.write("blocked prompt output", &token);
    let cancel = async {
        tokio::task::yield_now().await;
        token.cancel();
    };
    let (result, ()) = tokio::join!(write, cancel);
    assert!(matches!(result, Err(SetupError::Interrupted)));
}

#[tokio::test]
async fn cancelling_inflight_pairing_drops_request_without_saving() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoints = endpoints(&listener);
    let (started, received) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (mut stream, _) = request(stream).await;
        started.send(()).unwrap();
        let mut byte = [0];
        stream.read(&mut byte).await.unwrap()
    });
    let directory = tempfile::tempdir().unwrap();
    let (store, mut document) = ConfigStore::load(directory.path().to_owned(), false).unwrap();
    let mut input = PromptIo::scripted(b"n\ny\n000000000001\n".to_vec());
    let token = CancellationToken::new();
    let run = cli::run(
        &store,
        &mut document,
        endpoints,
        &mut input,
        token.clone(),
        false,
    );
    let cancel = async {
        received.await.unwrap();
        token.cancel();
    };
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(run, cancel) })
            .await
            .unwrap();
    assert!(matches!(result, Err(SetupError::Interrupted)));
    assert!(!store.config_file().exists());
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn failed_save_preserves_draft_and_existing_destination() {
    let directory = tempfile::tempdir().unwrap();
    let (store, mut document) = ConfigStore::load(directory.path().to_owned(), false).unwrap();
    std::fs::create_dir(store.config_file()).unwrap();
    let marker = store.config_file().join("existing");
    std::fs::write(&marker, "unchanged").unwrap();
    let (mut input, _) = prompt(&["", "", "", "hook", "", "", "", "", "", "", "", "y"]);
    let result = cli::run(
        &store,
        &mut document,
        offline_endpoints(),
        &mut input,
        CancellationToken::new(),
        false,
    )
    .await;
    assert!(matches!(result, Err(SetupError::Config(_))));
    assert_eq!(document.0["skip_categories"], json!(["hook"]));
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "unchanged");
}

#[tokio::test]
async fn ctrl_c_character_and_partial_line_cancellation_do_not_lose_input() {
    let mut prompt = PromptIo::scripted(b"\x03\n".to_vec());
    assert!(matches!(
        prompt.read_line("", &CancellationToken::new()).await,
        Err(SetupError::Interrupted)
    ));
    let (reader, mut writer) = tokio::io::duplex(64);
    writer.write_all(b"partial").await.unwrap();
    let mut prompt = PromptIo::new(BufReader::new(reader), tokio::io::sink());
    let token = CancellationToken::new();
    let read = prompt.read_line("", &token);
    let cancel = async {
        tokio::task::yield_now().await;
        token.cancel();
    };
    let (result, ()) = tokio::join!(read, cancel);
    assert!(matches!(result, Err(SetupError::Interrupted)));
    writer.write_all(b" line\n").await.unwrap();
    assert_eq!(
        prompt
            .read_line("", &CancellationToken::new())
            .await
            .unwrap(),
        "partial line"
    );
}
