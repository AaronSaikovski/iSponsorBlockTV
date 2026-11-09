#[path = "fixtures/runtime_scenario.rs"]
mod fixture;
use fixture::{Fixture, now_playing, state};
use futures_util::FutureExt;
use isponsorblocktv::{
    config::RuntimeConfig,
    runtime::{self, RuntimeError},
};
use serde_json::json;
use std::{collections::BTreeSet, panic::AssertUnwindSafe, time::Duration};
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

async fn connected_pair(fixture: &mut Fixture) {
    let mut connected = BTreeSet::new();
    while connected.len() != 2 {
        let request = fixture.next().await;
        assert!(["screen-a", "screen-b"].contains(&request.device.as_str()));
        assert!(request.command().is_none());
        assert!(request.reply.is_none());
        if request.method == "GET" && request.path.ends_with("/bind") {
            assert!(connected.insert(request.device));
        } else {
            assert!(
                request.path.ends_with("/get_lounge_token_batch")
                    || request.path.ends_with("/get_screen_availability")
                    || (request.method == "POST" && request.path.ends_with("/bind")),
                "unexpected connection request: {}",
                request.path
            );
        }
    }
}

#[tokio::test]
async fn two_real_devices_serialize_independently_and_report_after_acceptance() {
    fixture::run().await;
}

#[tokio::test]
async fn segment_lookup_survives_observations_and_schedules_from_latest_position() {
    let mut fixture = Fixture::start_with_deferred_segments().await;
    let (shutdown, service) = launch(&fixture, fixture.config(&["screen-a"]));
    fixture.subscribed("screen-a").await;
    fixture
        .events("screen-a", vec![now_playing("video-a", 20.0, 1)])
        .await;
    let lookup = loop {
        let request = fixture.next().await;
        if request.path.contains("skipSegments/") {
            break request;
        }
        assert!(request.command().is_none());
    };
    assert!(lookup.reply.is_some());

    for (position, playback_state, speed) in [(12.0, 2, "2"), (1.0, 1, "0.01")] {
        fixture
            .events(
                "screen-a",
                vec![
                    state(position, playback_state),
                    now_playing("video-a", position, playback_state),
                    json!(["onPlaybackSpeedChanged", {"playbackSpeed":speed}]),
                ],
            )
            .await;
        // This command proves all preceding observations were reduced while
        // the original lookup was still waiting on its oneshot response.
        let marker = fixture.next().await;
        assert_eq!(marker.command(), Some("getNowPlaying"));
    }

    lookup.respond(200);
    let seek = fixture.next().await;
    assert_eq!(seek.command(), Some("seekTo"));
    assert_eq!(seek.form["req0_newTime"], "5");
    seek.respond(200);
    assert!(fixture.next().await.path.contains("viewedVideoSponsorTime"));

    // Completed metadata also survives later observations. Rewind after
    // observing the seek target, and require another seek without a lookup.
    fixture
        .events("screen-a", vec![state(5.0, 2), state(1.0, 1)])
        .await;
    let seek = fixture.next().await;
    assert_eq!(seek.command(), Some("seekTo"));
    assert_eq!(seek.form["req0_newTime"], "5");
    seek.respond(200);
    assert!(fixture.next().await.path.contains("viewedVideoSponsorTime"));
    finish(fixture, shutdown, service).await;
}

#[tokio::test]
async fn shared_segment_lookup_survives_one_actor_changing_video() {
    let mut fixture = Fixture::start_with_deferred_segments().await;
    let (shutdown, mut service) = launch(&fixture, fixture.config(&["screen-a", "screen-b"]));
    let outcome = AssertUnwindSafe(async {
        connected_pair(&mut fixture).await;
        fixture
            .events(
                "screen-a",
                vec![
                    now_playing("video-a", 1.0, 1),
                    json!(["onPlaybackSpeedChanged", {"playbackSpeed":"0.01"}]),
                ],
            )
            .await;
        let mut original = None;
        let mut observed_a = false;
        while original.is_none() || !observed_a {
            let request = fixture.next().await;
            if request.path.contains("skipSegments/") {
                assert!(original.is_none(), "duplicate original segment lookup");
                assert!(request.reply.is_some());
                original = Some(request);
            } else {
                assert_eq!(request.command(), Some("getNowPlaying"));
                assert_eq!(request.device, "screen-a");
                assert!(!observed_a, "duplicate observation barrier");
                observed_a = true;
            }
        }
        let original = original.unwrap();
        let original_path = original.path.clone();

        fixture
            .events(
                "screen-b",
                vec![
                    now_playing("video-a", 1.0, 1),
                    json!(["onPlaybackSpeedChanged", {"playbackSpeed":"0.01"}]),
                ],
            )
            .await;
        // Each device's command proves its preceding observations were reduced.
        // Do not search/filter: a second lookup must fail this assertion.
        let barrier = fixture.next().await;
        assert_eq!(barrier.command(), Some("getNowPlaying"));
        assert_eq!(barrier.device, "screen-b");
        fixture.no_requests().await;

        fixture
            .events(
                "screen-a",
                vec![
                    now_playing("video-next", 1.0, 1),
                    json!(["onPlaybackSpeedChanged", {"playbackSpeed":"0.01"}]),
                ],
            )
            .await;
        let mut replacement = None;
        let mut switched = false;
        while replacement.is_none() || !switched {
            let request = fixture.next().await;
            if request.path.contains("skipSegments/") {
                assert_ne!(request.path, original_path, "original lookup was restarted");
                assert!(replacement.is_none(), "duplicate replacement lookup");
                assert!(request.reply.is_some());
                replacement = Some(request);
            } else {
                assert_eq!(request.command(), Some("getNowPlaying"));
                assert_eq!(request.device, "screen-a");
                assert!(!switched, "duplicate switch barrier");
                switched = true;
            }
        }

        // The first consumer is gone, but the shared HTTP response must still
        // reach the second actor, with no old-video mutation from screen-a.
        original.respond(200);
        let seek = fixture.next().await;
        assert_eq!(seek.command(), Some("seekTo"));
        assert_eq!(seek.device, "screen-b");
        assert_eq!(seek.form["req0_newTime"], "5");
        seek.respond(200);
        let report = fixture.next().await;
        assert!(report.path.contains("viewedVideoSponsorTime"));
        assert_eq!(report.query["UUID"], "uuid-video-a");

        fixture
            .events(
                "screen-a",
                vec![
                    state(1.0, 1),
                    json!(["onPlaybackSpeedChanged", {"playbackSpeed":"0.01"}]),
                ],
            )
            .await;
        let barrier = fixture.next().await;
        assert_eq!(barrier.command(), Some("getNowPlaying"));
        assert_eq!(barrier.device, "screen-a");
        fixture.no_requests().await;

        // Also prove the switched actor remains live and reports only its new
        // video's UUID once that distinct lookup is allowed to complete.
        replacement.unwrap().respond(200);
        let seek = fixture.next().await;
        assert_eq!(seek.command(), Some("seekTo"));
        assert_eq!(seek.device, "screen-a");
        assert_eq!(seek.form["req0_newTime"], "5");
        seek.respond(200);
        let report = fixture.next().await;
        assert!(report.path.contains("viewedVideoSponsorTime"));
        assert_eq!(report.query["UUID"], "uuid-video-next");
        fixture.no_requests().await;
    })
    .catch_unwind()
    .await;

    shutdown.cancel();
    let stopped = tokio::time::timeout(Duration::from_secs(4), &mut service).await;
    if stopped.is_err() {
        service.abort();
        let _ = service.await;
    }
    fixture.stop().await;
    outcome.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
    stopped.unwrap().unwrap().unwrap();
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

#[tokio::test]
async fn shutdown_shares_one_deadline_and_releases_both_blocked_terminations() {
    let mut fixture = Fixture::start_with_deferred_terminations().await;
    let (shutdown, mut service) = launch(&fixture, fixture.config(&["screen-a", "screen-b"]));
    let mut joined = false;
    let outcome = AssertUnwindSafe(async {
        connected_pair(&mut fixture).await;
        shutdown.cancel();
        let mut devices = BTreeSet::new();
        let mut pending = Vec::new();
        while pending.len() != 2 {
            let request = fixture.next().await;
            assert_eq!(
                request.form.get("TYPE").map(String::as_str),
                Some("terminate")
            );
            assert_eq!(
                request.form["clientDisconnectReason"],
                "MDX_SESSION_DISCONNECT_REASON_DISCONNECTED_BY_USER"
            );
            assert!(request.reply.is_some());
            assert!(devices.insert(request.device.clone()));
            pending.push(request);
        }
        assert_eq!(
            devices,
            BTreeSet::from(["screen-a".to_owned(), "screen-b".to_owned()])
        );

        // Both real TCP requests are blocked before pausing. A serial shutdown
        // cannot substitute two three-second budgets for the supervisor's one.
        tokio::time::pause();
        let paused_at = tokio::time::Instant::now();
        tokio::time::advance(Duration::from_secs(2)).await;
        let still_pending = !service.is_finished();
        tokio::time::advance(Duration::from_secs(1)).await;
        let stopped = tokio::time::timeout(Duration::from_millis(1), &mut service).await;
        joined = stopped.is_ok();
        let elapsed = paused_at.elapsed();
        // Resume before assertions or waiting for the TCP EOF notifications.
        tokio::time::resume();
        assert!(
            still_pending,
            "blocked termination returned before its deadline"
        );
        assert!(elapsed <= Duration::from_millis(3001));
        stopped
            .expect("both devices must stop within one three-second budget")
            .unwrap()
            .unwrap();
        for request in pending {
            request.cancelled().await;
        }
        fixture.no_requests().await;
    })
    .catch_unwind()
    .await;

    shutdown.cancel();
    if !joined {
        service.abort();
        let _ = service.await;
    }
    fixture.stop().await;
    outcome.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
}
