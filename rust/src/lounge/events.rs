//! Typed subset of the Lounge event protocol used by iSponsorBlockTV.
//! Protocol reference: pyytlounge 3aec6089f5056362ab7c5742fc44c1e1866608b0
//! (GPL-3.0), events.py/wrapper.py; implementation deliberately does not retain
//! raw payloads in errors or debug output.
use serde_json::{Map, Value};

use super::wire::FrameError;

#[derive(Clone, Debug, PartialEq)]
pub struct Envelope {
    pub aid: u64,
    pub event: LoungeEvent,
}

#[derive(Clone, Debug, PartialEq)]
pub enum LoungeEvent {
    Connected {
        sid: String,
    },
    Session {
        gsession: String,
    },
    StateChange {
        position: Option<f64>,
        duration: Option<f64>,
        state: Option<i32>,
    },
    NowPlaying {
        video_id: String,
        position: Option<f64>,
        duration: Option<f64>,
        state: i32,
    },
    PlaybackSpeed {
        speed: f64,
    },
    AdState {
        ended: bool,
        skip_enabled: bool,
    },
    AdPlaying {
        content_video_id: Option<String>,
        skip_enabled: bool,
    },
    Volume {
        volume: Option<f64>,
        muted: Option<bool>,
    },
    AutoplayUpNext {
        video_id: Option<String>,
    },
    LoungeStatus {
        name: Option<String>,
        kids: bool,
    },
    Disconnected {
        shorts: bool,
    },
    Subtitles {
        video_id: Option<String>,
    },
    AutoplayMode {
        enabled: bool,
    },
    Noop,
    Unknown,
    Malformed {
        kind: &'static str,
    },
}

/// Validate every outer envelope before any event in this frame is delivered.
/// A malformed individual payload remains an envelope, so its AID still advances.
pub fn decode_batch(bytes: &[u8]) -> Result<Vec<Envelope>, FrameError> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| FrameError::Batch)?;
    let batch = value.as_array().ok_or(FrameError::Batch)?;
    for item in batch {
        let item = item
            .as_array()
            .filter(|v| v.len() == 2)
            .ok_or(FrameError::Batch)?;
        item[0].as_u64().ok_or(FrameError::Batch)?;
        let event = item[1].as_array().ok_or(FrameError::Batch)?;
        event
            .first()
            .and_then(Value::as_str)
            .ok_or(FrameError::Batch)?;
    }
    Ok(batch
        .iter()
        .map(|item| {
            let event = item[1].as_array().expect("validated event");
            Envelope {
                aid: item[0].as_u64().expect("validated aid"),
                event: parse_event(event[0].as_str().expect("validated name"), &event[1..]),
            }
        })
        .collect())
}

type Data = Map<String, Value>;
type Parsed<T> = Result<T, ()>;

fn text(data: &Data, key: &str) -> Parsed<Option<String>> {
    data.get(key)
        .map(|v| v.as_str().map(str::to_owned).ok_or(()))
        .transpose()
}

fn number(data: &Data, key: &str) -> Parsed<Option<f64>> {
    data.get(key)
        .map(|value| {
            let n = match value {
                Value::String(s) => s.parse::<f64>().map_err(|_| ())?,
                _ => value.as_f64().ok_or(())?,
            };
            if n.is_finite() { Ok(n) } else { Err(()) }
        })
        .transpose()
}

fn integer(data: &Data, key: &str) -> Parsed<Option<i32>> {
    data.get(key)
        .map(|value| match value {
            Value::String(s) => s.parse().map_err(|_| ()),
            _ => value.as_i64().and_then(|n| i32::try_from(n).ok()).ok_or(()),
        })
        .transpose()
}

fn boolean(data: &Data, key: &str) -> Parsed<Option<bool>> {
    data.get(key)
        .map(|value| match value.as_str() {
            Some("true") => Ok(true),
            Some("false") => Ok(false),
            _ => Err(()),
        })
        .transpose()
}

fn nonempty(value: Option<String>) -> Option<String> {
    value.filter(|s| !s.is_empty())
}

fn parse_event(kind: &str, args: &[Value]) -> LoungeEvent {
    use LoungeEvent::*;
    let known = match kind {
        "c" => "c",
        "S" => "S",
        "onStateChange" => "onStateChange",
        "nowPlaying" => "nowPlaying",
        "onPlaybackSpeedChanged" => "onPlaybackSpeedChanged",
        "onAdStateChange" => "onAdStateChange",
        "adPlaying" => "adPlaying",
        "onVolumeChanged" => "onVolumeChanged",
        "autoplayUpNext" => "autoplayUpNext",
        "loungeStatus" => "loungeStatus",
        "loungeScreenDisconnected" => "loungeScreenDisconnected",
        "onSubtitlesTrackChanged" => "onSubtitlesTrackChanged",
        "onAutoplayModeChanged" => "onAutoplayModeChanged",
        "noop" => return Noop,
        _ => {
            tracing::debug!("Ignoring unsupported Lounge event");
            return Unknown;
        }
    };
    let parsed = (|| -> Parsed<LoungeEvent> {
        if kind == "c" || kind == "S" {
            let id = args
                .first()
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .ok_or(())?
                .to_owned();
            return Ok(if kind == "c" {
                Connected { sid: id }
            } else {
                Session { gsession: id }
            });
        }
        if kind == "loungeScreenDisconnected" && args.is_empty() {
            return Ok(Disconnected { shorts: false });
        }
        // An empty next-item event is explicitly permitted upstream.
        if kind == "autoplayUpNext" && args.is_empty() {
            return Ok(AutoplayUpNext { video_id: None });
        }
        let data = args.first().and_then(Value::as_object).ok_or(())?;
        Ok(match kind {
            "onStateChange" => StateChange {
                position: number(data, "currentTime")?,
                duration: number(data, "duration")?,
                state: integer(data, "state")?,
            },
            "nowPlaying" => NowPlaying {
                video_id: text(data, "videoId")?.unwrap_or_default(),
                position: number(data, "currentTime")?,
                duration: number(data, "duration")?,
                state: integer(data, "state")?.unwrap_or(-1),
            },
            "onPlaybackSpeedChanged" => {
                let value = data.get("playbackSpeed").ok_or(())?;
                let speed = if let Some(s) = value.as_str() {
                    s.parse().map_err(|_| ())?
                } else {
                    value.as_f64().ok_or(())?
                };
                PlaybackSpeed { speed }
            }
            "onAdStateChange" => {
                let state = text(data, "adState")?.ok_or(())?;
                let time = text(data, "currentTime")?;
                AdState {
                    ended: state == "0" && time.as_deref().is_some_and(|t| t != "0"),
                    skip_enabled: boolean(data, "isSkipEnabled")?.unwrap_or(false),
                }
            }
            "adPlaying" => AdPlaying {
                content_video_id: nonempty(text(data, "contentVideoId")?),
                skip_enabled: boolean(data, "isSkipEnabled")?.unwrap_or(false),
            },
            "onVolumeChanged" => Volume {
                volume: number(data, "volume")?,
                muted: boolean(data, "muted")?,
            },
            "autoplayUpNext" => AutoplayUpNext {
                video_id: nonempty(text(data, "videoId")?),
            },
            "loungeStatus" => parse_status(data)?,
            "loungeScreenDisconnected" => Disconnected {
                shorts: text(data, "reason")?.as_deref()
                    == Some("disconnectedByUserScreenInitiated"),
            },
            "onSubtitlesTrackChanged" => Subtitles {
                video_id: nonempty(text(data, "videoId")?),
            },
            "onAutoplayModeChanged" => match text(data, "autoplayMode")?.as_deref() {
                Some("ENABLED") => AutoplayMode { enabled: true },
                Some("DISABLED") => AutoplayMode { enabled: false },
                Some("UNSUPPORTED") => Unknown,
                _ => return Err(()),
            },
            _ => unreachable!("known kind"),
        })
    })();
    parsed.unwrap_or_else(|()| {
        tracing::warn!(event = known, "Ignoring malformed Lounge event payload");
        Malformed { kind: known }
    })
}

fn parse_status(data: &Data) -> Parsed<LoungeEvent> {
    let encoded = data.get("devices").and_then(Value::as_str).ok_or(())?;
    let devices: Value = serde_json::from_str(encoded).map_err(|_| ())?;
    let devices = devices.as_array().ok_or(())?;
    let mut name = None;
    let mut kids = false;
    for device in devices {
        let device = device.as_object().ok_or(())?;
        if text(device, "type")?.as_deref() != Some("LOUNGE_SCREEN") {
            continue;
        }
        if name.is_none() {
            name = text(device, "name")?;
        }
        if let Some(encoded) = text(device, "deviceInfo")? {
            let info: Value = serde_json::from_str(&encoded).map_err(|_| ())?;
            if !info.is_null() {
                let info = info.as_object().ok_or(())?;
                kids |= text(info, "clientName")?.as_deref() == Some("TVHTML5_FOR_KIDS");
            }
        }
    }
    Ok(LoungeEvent::LoungeStatus { name, kids })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn missing_optionals_and_raw_states_are_preserved() {
        for state in [-1, 0, 1, 2, 3, 5, 1081] {
            let event = parse_event("onStateChange", &[json!({"state":state.to_string()})]);
            assert_eq!(
                event,
                LoungeEvent::StateChange {
                    position: None,
                    duration: None,
                    state: Some(state)
                }
            );
        }
        assert_eq!(
            parse_event("nowPlaying", &[json!({})]),
            LoungeEvent::NowPlaying {
                video_id: String::new(),
                position: None,
                duration: None,
                state: -1
            }
        );
    }

    #[test]
    fn malformed_and_unknown_payloads_keep_aid() {
        let events = decode_batch(br#"[[5,["onStateChange",{"currentTime":"NaN"}]],[6,["private-unknown",{}]],[7,["noop"]]]"#).unwrap();
        assert!(matches!(events[0].event, LoungeEvent::Malformed { .. }));
        assert_eq!(events[1].event, LoungeEvent::Unknown);
        assert_eq!(events.last().unwrap().aid, 7);
    }

    #[test]
    fn invalid_speed_reaches_actor_for_refresh() {
        assert!(
            matches!(parse_event("onPlaybackSpeedChanged", &[json!({"playbackSpeed":"NaN"})]), LoungeEvent::PlaybackSpeed { speed } if speed.is_nan())
        );
    }

    #[test]
    fn nested_status_detects_kids_profile() {
        let devices = json!([{"type":"LOUNGE_SCREEN","name":"<literal>🦀", "deviceInfo":json!({"clientName":"TVHTML5_FOR_KIDS"}).to_string()}]).to_string();
        assert_eq!(
            parse_event("loungeStatus", &[json!({"devices":devices})]),
            LoungeEvent::LoungeStatus {
                name: Some("<literal>🦀".into()),
                kids: true
            }
        );
        assert!(matches!(
            parse_event("loungeStatus", &[json!({"devices":"not JSON"})]),
            LoungeEvent::Malformed { .. }
        ));
    }

    #[test]
    fn control_event_fields_preserve_wire_semantics() {
        use LoungeEvent::*;
        for (kind, payload, expected) in [
            (
                "onAdStateChange",
                json!({"adState":"0","currentTime":"1"}),
                AdState {
                    ended: true,
                    skip_enabled: false,
                },
            ),
            (
                "onAdStateChange",
                json!({"adState":"0","currentTime":"0","isSkipEnabled":"true"}),
                AdState {
                    ended: false,
                    skip_enabled: true,
                },
            ),
            (
                "adPlaying",
                json!({"contentVideoId":"next","isSkipEnabled":"true"}),
                AdPlaying {
                    content_video_id: Some("next".into()),
                    skip_enabled: true,
                },
            ),
            (
                "onVolumeChanged",
                json!({"volume":"17.5"}),
                Volume {
                    volume: Some(17.5),
                    muted: None,
                },
            ),
            (
                "onVolumeChanged",
                json!({"muted":"false"}),
                Volume {
                    volume: None,
                    muted: Some(false),
                },
            ),
            (
                "autoplayUpNext",
                json!({"videoId":""}),
                AutoplayUpNext { video_id: None },
            ),
            (
                "autoplayUpNext",
                json!({"videoId":"next"}),
                AutoplayUpNext {
                    video_id: Some("next".into()),
                },
            ),
            (
                "loungeScreenDisconnected",
                json!({"reason":"disconnectedByUserScreenInitiated"}),
                Disconnected { shorts: true },
            ),
            (
                "loungeScreenDisconnected",
                json!({"reason":"other"}),
                Disconnected { shorts: false },
            ),
            (
                "onSubtitlesTrackChanged",
                json!({"videoId":"short"}),
                Subtitles {
                    video_id: Some("short".into()),
                },
            ),
            (
                "onAutoplayModeChanged",
                json!({"autoplayMode":"ENABLED"}),
                AutoplayMode { enabled: true },
            ),
            (
                "onAutoplayModeChanged",
                json!({"autoplayMode":"DISABLED"}),
                AutoplayMode { enabled: false },
            ),
            (
                "onAutoplayModeChanged",
                json!({"autoplayMode":"UNSUPPORTED"}),
                Unknown,
            ),
        ] {
            assert_eq!(parse_event(kind, &[payload]), expected);
        }
        assert_eq!(
            parse_event("loungeScreenDisconnected", &[]),
            Disconnected { shorts: false }
        );
        assert_eq!(
            parse_event("autoplayUpNext", &[]),
            AutoplayUpNext { video_id: None }
        );
        assert!(matches!(
            parse_event("onVolumeChanged", &[json!({"muted":true})]),
            Malformed { .. }
        ));
    }
}
