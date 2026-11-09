use isponsorblocktv::{
    AppContext, PreparedApp,
    cli::{Cli, Command},
    config::{ConfigStore, Environment},
    http::Endpoints,
    run_app,
    setup::PromptIo,
};
use std::io::Cursor;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tokio_util::sync::CancellationToken;

#[path = "cast_scenario.rs"]
mod cast_fixture;
#[path = "discovery_scenario.rs"]
mod discovery_fixture;
#[path = "runtime_scenario.rs"]
mod runtime_fixture;

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let server = TcpListener::bind("127.0.0.1:0").await?;
    let address = server.local_addr()?;
    let fixture = tokio::spawn(async move {
        let mut requests = Vec::new();
        for reply in [
            r#"{"screen":{"screenId":"synthetic-screen-one","name":"Native fixture"}}"#,
            r#"{"items":[]}"#,
            r#"{"items":[{"snippet":{"channelId":"not-whitelisted"}}]}"#,
            r#"[{"videoID":"fixture-video","segments":[{"segment":[0,0.6],"UUID":"first","locked":0},{"segment":[1.1,1.7],"UUID":"second","locked":0}]}]"#,
        ] {
            let (mut stream, _) = server.accept().await.unwrap();
            let mut received = Vec::new();
            let mut buffer = [0_u8; 1024];
            loop {
                let count = stream.read(&mut buffer).await.unwrap();
                assert_ne!(count, 0);
                received.extend_from_slice(&buffer[..count]);
                if let Some(header_end) = received.windows(4).position(|bytes| bytes == b"\r\n\r\n")
                {
                    let headers = std::str::from_utf8(&received[..header_end]).unwrap();
                    let length: usize = headers
                        .lines()
                        .filter_map(|line| line.split_once(':'))
                        .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                        .map(|(_, value)| value.trim().parse().unwrap())
                        .unwrap_or(0);
                    if received.len() >= header_end + 4 + length {
                        break;
                    }
                }
                assert!(received.len() < 16384);
            }
            requests.push(String::from_utf8(received).unwrap());
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                reply.len(),
                reply
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        }
        requests
    });
    let root = tempfile::tempdir()?;
    let mut environment = Environment::capture()?;
    environment.docker = false;
    let base: url::Url = format!("http://{address}/").parse()?;
    let endpoints = Endpoints {
        lounge: base.clone(),
        youtube: base.clone(),
        sponsorblock: base,
    };
    let input = b"n\ny\n0000-1111 2222\nn\ny\nsynthetic-api-key\nsponsor hook\ny\nno results\n5\ncustom-channel\nCustom channel\n/exit\nn\ny\nn\nn\ny\n\ny\n";
    let prompt = PromptIo::new(Cursor::new(input.to_vec()), tokio::io::sink());
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        run_app(PreparedApp::load(
            Cli {
                data: Some(root.path().to_owned()),
                debug: false,
                http_tracing: false,
                legacy_setup: false,
                legacy_setup_cli: false,
                command: Some(Command::SetupCli),
            },
            AppContext {
                environment,
                endpoints: endpoints.clone(),
                shutdown: CancellationToken::new(),
                prompt: Some(prompt),
            },
        )?),
    )
    .await??;
    let (_, document) = ConfigStore::load(root.path().to_owned(), false)?;
    let runtime = document.runtime_config()?;
    assert_eq!(runtime.devices[0].screen_id, "synthetic-screen-one");
    assert_eq!(runtime.channel_whitelist[0].id, "custom-channel");
    assert_eq!(runtime.skip_categories, ["sponsor", "hook"]);
    let service = isponsorblocktv::api::ApiService::new(&runtime, endpoints, false)?;
    let segments = service.segments("fixture-video").await?;
    assert_eq!(segments.len(), 1);
    assert_eq!((segments[0].start, segments[0].end), (0.0, 1.7));
    assert_eq!(segments[0].uuids, ["first", "second"]);
    let requests = fixture.await?;
    assert!(requests[0].starts_with("POST /pairing/get_screen "));
    assert!(requests[0].ends_with("pairing_code=000011112222"));
    assert!(requests[1].starts_with("GET /search?"));
    assert!(requests[3].starts_with("GET /skipSegments/"));
    run_service(root.path()).await?;
    discovery_fixture::run().await?;
    cast_fixture::run().await;
    println!(
        "Native scenario passed: setup, saved config, SponsorBlock merge, two-device service, reconnect isolation, passive DIAL discovery, and clean shutdown."
    );
    Ok(())
}

async fn run_service(data_dir: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    use runtime_fixture::{Fixture, now_playing};
    use serde_json::json;
    let mut fixture = Fixture::start(false).await;
    let (store, mut document) = ConfigStore::load(data_dir.to_owned(), false)?;
    document.0.insert(
        "devices".into(),
        json!([
            {"screen_id":"screen-a","name":"Native fixture A","offset":0},
            {"screen_id":"screen-b","name":"Native fixture B","offset":0}
        ]),
    );
    document.0.insert("channel_whitelist".into(), json!([]));
    document.0.insert("minimum_skip_length".into(), json!(0));
    document.0.insert("skip_count_tracking".into(), json!(true));
    document.0.insert("mute_ads".into(), json!(true));
    document.0.insert("skip_ads".into(), json!(true));
    document.0.insert("auto_play".into(), json!(true));
    store.save(&document)?;
    let mut environment = Environment::capture()?;
    environment.docker = false;
    let shutdown = CancellationToken::new();
    let service = tokio::spawn(run_app(PreparedApp::load(
        Cli {
            data: Some(data_dir.to_owned()),
            debug: false,
            http_tracing: false,
            legacy_setup: false,
            legacy_setup_cli: false,
            command: Some(Command::Start),
        },
        AppContext {
            environment,
            endpoints: fixture.endpoints.clone(),
            shutdown: shutdown.clone(),
            prompt: None,
        },
    )?));
    fixture.subscribed("screen-a").await;
    fixture.subscribed("screen-b").await;
    fixture
        .events("screen-a", vec![now_playing("video-a", 0.0, 1)])
        .await;
    assert_eq!(
        fixture.command("seekTo", "screen-a").await.form["req0_newTime"],
        "5"
    );
    assert_eq!(fixture.report().await.query["UUID"], "uuid-video-a");
    fixture.drop_stream("screen-a").await;
    fixture
        .events("screen-b", vec![now_playing("video-b", 0.0, 1)])
        .await;
    assert_eq!(
        fixture.command("seekTo", "screen-b").await.form["req0_newTime"],
        "5"
    );
    assert_eq!(fixture.report().await.query["UUID"], "uuid-video-b");
    fixture
        .events(
            "screen-b",
            vec![json!(["onAdStateChange", {
                "adState":"1","currentTime":"0","isSkipEnabled":"true"
            }])],
        )
        .await;
    fixture.command("skipAd", "screen-b").await;
    assert_eq!(
        fixture.command("setVolume", "screen-b").await.form["req0_muted"],
        "true"
    );
    fixture
        .events(
            "screen-b",
            vec![
                json!(["onVolumeChanged", {"volume":"37","muted":"true"}]),
                json!(["onAdStateChange", {"adState":"0","currentTime":"1"}]),
            ],
        )
        .await;
    let restored = fixture.command("setVolume", "screen-b").await;
    assert_eq!(restored.form["req0_muted"], "false");
    assert_eq!(restored.form["req0_volume"], "37");
    fixture
        .events(
            "screen-b",
            vec![json!(["onAutoplayModeChanged", {
                "autoplayMode":"DISABLED"
            }])],
        )
        .await;
    assert_eq!(
        fixture.command("setAutoplayMode", "screen-b").await.form["req0_autoplayMode"],
        "ENABLED"
    );
    fixture.reconnected("screen-a").await;
    fixture
        .events("screen-a", vec![now_playing("video-next", 0.0, 1)])
        .await;
    assert_eq!(
        fixture.command("seekTo", "screen-a").await.form["req0_newTime"],
        "5"
    );
    assert_eq!(fixture.report().await.query["UUID"], "uuid-video-next");
    shutdown.cancel();
    tokio::time::timeout(std::time::Duration::from_secs(4), service).await???;
    fixture.stop().await;
    Ok(())
}
