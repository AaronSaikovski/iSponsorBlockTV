#[path = "fixtures/runtime_scenario.rs"]
mod fixture;
use fixture::{Fixture, now_playing, state};
use isponsorblocktv::{
    config::RuntimeConfig,
    runtime::{self, RuntimeError},
};
use serde_json::json;
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

fn launch(
    fixture: &Fixture,
    config: RuntimeConfig,
) -> (CancellationToken, JoinHandle<Result<(), RuntimeError>>) {
    let shutdown = CancellationToken::new();
    let service = tokio::spawn(runtime::run(
        config,
        fixture.endpoints.clone(),
        false,
        shutdown.clone(),
    ));
    (shutdown, service)
}
async fn finish(
    fixture: Fixture,
    shutdown: CancellationToken,
    service: JoinHandle<Result<(), RuntimeError>>,
) {
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(4), service)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    fixture.stop().await;
}

#[tokio::test]
async fn two_real_devices_serialize_independently_and_report_after_acceptance() {
    fixture::run().await;
}

#[tokio::test]
async fn successful_seek_stays_suppressed_until_target_observation_then_rewind() {
    let mut fixture = Fixture::start(true).await;
    let (shutdown, service) = launch(&fixture, fixture.config(&["screen-a"]));
    fixture.subscribed("screen-a").await;
    fixture
        .events("screen-a", vec![now_playing("video-a", 0.0, 1)])
        .await;
    let first = fixture.command("seekTo", "screen-a").await;
    assert_eq!(first.form["ofs"], "1");
    assert_eq!(first.query["RID"], "2");
    first.respond(200);
    fixture.report().await;
    fixture
        .events(
            "screen-a",
            vec![state(1.0, 1), state(1.0, 2), state(1.0, 1)],
        )
        .await;
    fixture.no_command_or_report().await;
    fixture
        .events("screen-a", vec![state(5.0, 1), state(0.0, 1)])
        .await;
    let second = fixture.command("seekTo", "screen-a").await;
    assert_eq!(second.form["ofs"], "2");
    assert_eq!(second.query["RID"], "3");
    second.respond(200);
    fixture.report().await;
    finish(fixture, shutdown, service).await;
}

#[tokio::test]
async fn ambiguous_seek_is_not_replayed_and_only_requests_fresh_state() {
    let mut fixture = Fixture::start(true).await;
    let (shutdown, service) = launch(&fixture, fixture.config(&["screen-a"]));
    fixture.subscribed("screen-a").await;
    fixture
        .events("screen-a", vec![now_playing("video-a", 0.0, 1)])
        .await;
    fixture.command("seekTo", "screen-a").await.respond(500);
    let refresh = fixture.command("getNowPlaying", "screen-a").await;
    assert_eq!(refresh.form["ofs"], "2");
    fixture.events("screen-a", vec![state(0.5, 1)]).await;
    fixture.no_command_or_report().await;
    finish(fixture, shutdown, service).await;
}

#[tokio::test]
async fn old_video_completion_cannot_report() {
    let mut fixture = Fixture::start(true).await;
    let (shutdown, service) = launch(&fixture, fixture.config(&["screen-a"]));
    fixture.subscribed("screen-a").await;
    fixture
        .events("screen-a", vec![now_playing("video-a", 0.0, 1)])
        .await;
    let first = fixture.command("seekTo", "screen-a").await;
    fixture
        .events("screen-a", vec![now_playing("video-next", 10.0, 1)])
        .await;
    // The next video's real API request proves the actor reduced the new ID.
    loop {
        let request = fixture.next().await;
        if request.path.contains("skipSegments/") {
            break;
        }
    }
    first.respond(200);
    fixture.no_command_or_report().await;
    finish(fixture, shutdown, service).await;
}

#[tokio::test]
async fn old_session_completion_cannot_report_or_replay() {
    let mut fixture = Fixture::start(true).await;
    let (shutdown, service) = launch(&fixture, fixture.config(&["screen-a"]));
    fixture.subscribed("screen-a").await;
    fixture
        .events("screen-a", vec![now_playing("video-a", 0.0, 1)])
        .await;
    let first = fixture.command("seekTo", "screen-a").await;
    fixture
        .events("screen-a", vec![json!(["c", "replacement-session"])])
        .await;
    // Let the stream reader deliver the replacement before releasing HTTP.
    fixture.no_command_or_report().await;
    first.respond(200);
    fixture.no_command_or_report().await;
    finish(fixture, shutdown, service).await;
}

#[tokio::test]
async fn disabled_tracking_never_posts_a_view() {
    let mut fixture = Fixture::start(true).await;
    let mut config = fixture.config(&["screen-a"]);
    config.skip_count_tracking = false;
    let (shutdown, service) = launch(&fixture, config);
    fixture.subscribed("screen-a").await;
    fixture
        .events("screen-a", vec![now_playing("video-a", 0.0, 1)])
        .await;
    fixture.command("seekTo", "screen-a").await.respond(200);
    fixture.no_command_or_report().await;
    finish(fixture, shutdown, service).await;
}

#[tokio::test]
async fn paused_buffering_ended_and_invalid_speed_never_seek() {
    let mut fixture = Fixture::start(false).await;
    let (shutdown, service) = launch(&fixture, fixture.config(&["screen-a"]));
    fixture.subscribed("screen-a").await;
    for wire_state in [-1, 0, 2, 3, 5, 1081] {
        fixture
            .events("screen-a", vec![now_playing("video-a", 0.0, wire_state)])
            .await;
        fixture.no_command_or_report().await;
    }
    fixture
        .events(
            "screen-a",
            vec![
                json!(["onPlaybackSpeedChanged", {"playbackSpeed":"0"}]),
                now_playing("video-a", 0.0, 1),
            ],
        )
        .await;
    fixture.command("getNowPlaying", "screen-a").await;
    fixture.no_command_or_report().await;
    finish(fixture, shutdown, service).await;
}

#[tokio::test]
async fn ad_policies_are_independent_and_preserve_users_mute() {
    for mute in [false, true] {
        for skip in [false, true] {
            for already_muted in [false, true] {
                let mut fixture = Fixture::start(false).await;
                let mut config = fixture.config(&["screen-a"]);
                config.mute_ads = mute;
                config.skip_ads = skip;
                let (shutdown, service) = launch(&fixture, config);
                fixture.subscribed("screen-a").await;
                fixture.events("screen-a", vec![
            json!(["onVolumeChanged", {"volume":"23","muted":if already_muted {"true"} else {"false"}}]),
            json!(["onAdStateChange", {"adState":"1","currentTime":"0","isSkipEnabled":"true"}]),
        ]).await;
                if skip {
                    fixture.command("skipAd", "screen-a").await;
                }
                if mute && !already_muted {
                    let command = fixture.command("setVolume", "screen-a").await;
                    assert_eq!(command.form["req0_muted"], "true");
                }
                fixture
                    .events(
                        "screen-a",
                        vec![
                            json!(["onVolumeChanged", {"volume":"41"}]),
                            json!(["onAdStateChange", {"adState":"0","currentTime":"1"}]),
                        ],
                    )
                    .await;
                if mute && !already_muted {
                    let command = fixture.command("setVolume", "screen-a").await;
                    assert_eq!(command.form["req0_muted"], "false");
                    assert_eq!(command.form["req0_volume"], "41");
                }
                assert_eq!(fixture.next().await.command(), Some("getNowPlaying"));
                fixture.no_command_or_report().await;
                finish(fixture, shutdown, service).await;
            }
        }
    }
}

#[tokio::test]
async fn explicit_unmute_ends_ownership_and_matching_autoplay_has_no_feedback() {
    let mut fixture = Fixture::start(false).await;
    let (shutdown, service) = launch(&fixture, fixture.config(&["screen-a"]));
    fixture.subscribed("screen-a").await;
    fixture
        .events(
            "screen-a",
            vec![json!(["onAdStateChange", {"adState":"1","currentTime":"0"}])],
        )
        .await;
    fixture.command("setVolume", "screen-a").await;
    fixture
        .events(
            "screen-a",
            vec![
                json!(["onVolumeChanged", {"muted":"false"}]),
                json!(["onAdStateChange", {"adState":"0","currentTime":"1"}]),
                json!(["onAutoplayModeChanged", {"autoplayMode":"ENABLED"}]),
            ],
        )
        .await;
    assert_eq!(fixture.next().await.command(), Some("getNowPlaying"));
    fixture.no_command_or_report().await;
    finish(fixture, shutdown, service).await;
}

#[tokio::test]
async fn dropped_stream_retries_while_other_device_keeps_working() {
    let mut fixture = Fixture::start(true).await;
    let (shutdown, service) = launch(&fixture, fixture.config(&["screen-a", "screen-b"]));
    fixture.subscribed("screen-a").await;
    fixture.subscribed("screen-b").await;
    fixture.drop_stream("screen-a").await;
    fixture
        .events("screen-b", vec![now_playing("video-b", 0.0, 1)])
        .await;
    fixture.command("seekTo", "screen-b").await.respond(200);
    fixture.report().await;
    fixture.reconnected("screen-a").await;
    fixture
        .events("screen-a", vec![now_playing("video-a", 0.0, 1)])
        .await;
    fixture.command("seekTo", "screen-a").await.respond(200);
    fixture.report().await;
    finish(fixture, shutdown, service).await;
}

#[tokio::test]
async fn shutdown_cancels_pending_http_and_terminates_connected_devices() {
    let mut fixture = Fixture::start(true).await;
    let (shutdown, service) = launch(&fixture, fixture.config(&["screen-a"]));
    fixture.subscribed("screen-a").await;
    fixture
        .events("screen-a", vec![now_playing("video-a", 0.0, 1)])
        .await;
    let pending = fixture.command("seekTo", "screen-a").await;
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(4), service)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    loop {
        let request = fixture.next().await;
        if request
            .form
            .get("TYPE")
            .is_some_and(|value| value == "terminate")
        {
            assert_eq!(request.query["RID"], "2");
            assert_eq!(
                request.form["clientDisconnectReason"],
                "MDX_SESSION_DISCONNECT_REASON_DISCONNECTED_BY_USER"
            );
            break;
        }
    }
    drop(pending);
    fixture.stop().await;
}
