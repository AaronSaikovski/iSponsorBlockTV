//! Stateless request transport. The device actor owns counters, epochs, retry
//! policy, serialization, and cancellation; every request here owns its snapshot.
//! Wire identity follows the project's ytlounge.py and pyytlounge (GPL-3.0),
//! pinned at 3aec6089f5056362ab7c5742fc44c1e1866608b0.
use std::sync::Arc;

use reqwest::{Method, StatusCode};
use serde_json::Value;
use thiserror::Error;
use url::Url;

use super::{Envelope, FrameDecoder, FrameError};
use crate::http::{
    HttpClients, HttpError, HttpStreamResponse, MAX_BODY_BYTES, REQUEST_TIMEOUT, validate_base,
};

pub const CONNECT_PARAMS: &str = r#"{"setStatesParams": "{"playbackSpeed":0}"}"#;
const APP: &str = "ytios-phone-20.15.1";
type Form = Vec<(&'static str, String)>;

#[derive(Clone, PartialEq, Eq)]
pub struct SessionIds {
    pub sid: String,
    pub gsession: String,
    pub aid: u64,
}

impl std::fmt::Debug for SessionIds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionIds")
            .field("aid", &self.aid)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    SeekTo { seconds: f64 },
    SkipAd,
    SetVolume { volume: f64, muted: bool },
    SetPlaylist { video_id: String },
    GetNowPlaying,
    SetAutoplayMode { enabled: bool },
}

#[derive(Debug, Error)]
pub enum LoungeError {
    #[error(transparent)]
    Http(#[from] HttpError),
    #[error(transparent)]
    Framing(#[from] FrameError),
    #[error("Lounge session is no longer known")]
    UnknownSid,
    #[error("Lounge session is gone")]
    Gone,
    #[error("Lounge authorization expired")]
    Expired,
    #[error("Lounge connection denied")]
    ConnectionDenied,
    #[error("Lounge request failed with HTTP status {0}")]
    Status(u16),
    #[error("malformed Lounge response")]
    MalformedResponse,
    #[error("Lounge commands require both connection identifiers")]
    MissingSession,
    #[error("invalid Lounge command parameters or counter")]
    InvalidCommand,
}

struct TransportInner {
    clients: HttpClients,
    base: Url,
    screen_id: String,
    join_name: String,
}

#[derive(Clone)]
pub struct LoungeTransport(Arc<TransportInner>);

impl LoungeTransport {
    pub fn new(
        clients: HttpClients,
        mut base: Url,
        screen_id: String,
        join_name: String,
    ) -> Result<Self, LoungeError> {
        validate_base(&base)?;
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        Ok(Self(Arc::new(TransportInner {
            clients,
            base,
            screen_id,
            join_name,
        })))
    }

    fn endpoint(&self, suffix: &str) -> Url {
        self.0
            .base
            .join(suffix)
            .expect("validated base and fixed suffix")
    }

    pub async fn refresh_token(self) -> Result<String, LoungeError> {
        let client = &self.0.clients.cloud;
        let response = client
            .execute(
                client
                    .request(
                        Method::POST,
                        self.endpoint("pairing/get_lounge_token_batch"),
                    )
                    .form(&[("screen_ids", self.0.screen_id.as_str())]),
            )
            .await?;
        check_status(response.status, &response.body, false)?;
        let value: Value =
            serde_json::from_slice(&response.body).map_err(|_| LoungeError::MalformedResponse)?;
        let screens = value
            .get("screens")
            .and_then(Value::as_array)
            .ok_or(LoungeError::MalformedResponse)?;
        screens
            .iter()
            .find_map(|screen| {
                if screen.get("screenId").and_then(Value::as_str) != Some(self.0.screen_id.as_str())
                {
                    return None;
                }
                screen
                    .get("loungeToken")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
            })
            .ok_or(LoungeError::MalformedResponse)
    }

    pub async fn availability(self, token: String) -> Result<bool, LoungeError> {
        let client = &self.0.clients.cloud;
        let response = client
            .execute(
                client
                    .request(
                        Method::POST,
                        self.endpoint("pairing/get_screen_availability"),
                    )
                    .form(&[("lounge_token", token)]),
            )
            .await?;
        check_status(response.status, &response.body, false)?;
        let value: Value =
            serde_json::from_slice(&response.body).map_err(|_| LoungeError::MalformedResponse)?;
        let object = value.as_object().ok_or(LoungeError::MalformedResponse)?;
        let Some(screens) = object.get("screens") else {
            return Ok(false);
        };
        let screens = screens.as_array().ok_or(LoungeError::MalformedResponse)?;
        Ok(screens
            .first()
            .and_then(|screen| screen.get("status"))
            .and_then(Value::as_str)
            == Some("online"))
    }

    pub async fn connect(self, token: String) -> Result<Vec<Envelope>, LoungeError> {
        let client = &self.0.clients.cloud;
        let response = client
            .execute(
                client
                    .request(Method::POST, self.endpoint("bc/bind"))
                    .form(&self.connect_form(token, false)),
            )
            .await?;
        check_status(response.status, &response.body, true)?;
        let mut decoder = FrameDecoder::new();
        decoder.push(&response.body)?;
        let mut events = Vec::new();
        while let Some(batch) = decoder.next_batch()? {
            events.extend(batch);
        }
        decoder.finish()?;
        // c and S may arrive in separate frames. The actor must obtain both
        // before subscribing or dispatching commands.
        Ok(events)
    }

    pub async fn recover(self, token: String) -> Result<(), LoungeError> {
        let client = &self.0.clients.cloud;
        let response = client
            .execute(
                client
                    .request(Method::POST, self.endpoint("bc/bind"))
                    .form(&self.connect_form(token, true)),
            )
            .await?;
        check_status(response.status, &response.body, true)
    }

    pub async fn subscribe(
        self,
        token: String,
        ids: SessionIds,
    ) -> Result<LoungeStream, LoungeError> {
        let mut query = self.common(token, ids)?;
        query.extend([
            ("RID", "rpc".into()),
            ("CI", "0".into()),
            ("TYPE", "xmlhttp".into()),
        ]);
        let client = &self.0.clients.lounge_stream;
        // The stream body has no total deadline, but opening its response does.
        let mut response = tokio::time::timeout(
            REQUEST_TIMEOUT,
            client.execute_stream(
                client
                    .request(Method::GET, self.endpoint("bc/bind"))
                    .query(&query),
            ),
        )
        .await
        .map_err(|_| HttpError::Timeout)??;
        let status = response.status();
        if status != StatusCode::OK {
            let body = tokio::time::timeout(REQUEST_TIMEOUT, bounded_error_body(&mut response))
                .await
                .map_err(|_| HttpError::Timeout)??;
            check_status(status, &body, false)?;
        }
        Ok(LoungeStream {
            response,
            decoder: FrameDecoder::new(),
            ended: false,
        })
    }

    /// Offset is allocated by the actor before spawning this call, and remains
    /// consumed on every failure. This transport never retries mutations.
    pub async fn command(
        self,
        token: String,
        ids: SessionIds,
        offset: u64,
        command: Command,
    ) -> Result<(), LoungeError> {
        let rid = offset
            .checked_add(1)
            .filter(|_| offset > 0)
            .ok_or(LoungeError::InvalidCommand)?;
        let mut query = self.common(token, ids)?;
        query.push(("RID", rid.to_string()));
        let body = command_form(offset, command)?;
        let client = &self.0.clients.cloud;
        let response = client
            .execute(
                client
                    .request(Method::POST, self.endpoint("bc/bind"))
                    .query(&query)
                    .form(&body),
            )
            .await?;
        check_status(response.status, &response.body, false)
    }

    pub async fn disconnect(
        self,
        token: String,
        ids: SessionIds,
        offset: u64,
    ) -> Result<(), LoungeError> {
        let mut query = self.common(token, ids)?;
        query.extend([
            ("CVER", "1".into()),
            ("RID", offset.to_string()),
            ("auth_failure_option", "send_error".into()),
        ]);
        let body = [
            ("ui", ""),
            ("TYPE", "terminate"),
            (
                "clientDisconnectReason",
                "MDX_SESSION_DISCONNECT_REASON_DISCONNECTED_BY_USER",
            ),
        ];
        let client = &self.0.clients.cloud;
        let response = client
            .execute(
                client
                    .request(Method::POST, self.endpoint("bc/bind"))
                    .query(&query)
                    .form(&body),
            )
            .await?;
        check_status(response.status, &response.body, false)
    }

    fn common(&self, token: String, ids: SessionIds) -> Result<Form, LoungeError> {
        if ids.sid.is_empty() || ids.gsession.is_empty() {
            return Err(LoungeError::MissingSession);
        }
        Ok(vec![
            ("name", self.0.join_name.clone()),
            ("loungeIdToken", token),
            ("SID", ids.sid),
            ("AID", ids.aid.to_string()),
            ("gsessionid", ids.gsession),
            ("device", "REMOTE_CONTROL".into()),
            ("app", APP.into()),
            ("VER", "8".into()),
            ("v", "2".into()),
        ])
    }

    fn connect_form(&self, token: String, recovery: bool) -> Form {
        let mut body = vec![
            (
                "id",
                if recovery {
                    uuid::Uuid::new_v4().to_string()
                } else {
                    self.0.screen_id.clone()
                },
            ),
            ("mdx-version", "3".into()),
            ("TYPE", "xmlhttp".into()),
            ("theme", "cl".into()),
            ("sessionSource", "MDX_SESSION_SOURCE_UNKNOWN".into()),
            ("connectParams", CONNECT_PARAMS.into()),
            ("RID", "1".into()),
            ("CVER", "1".into()),
            ("capabilities", "que,dsdtr,atp,vsp".into()),
            ("ui", "false".into()),
            ("app", APP.into()),
            ("pairing_type", "manual".into()),
            ("VER", "8".into()),
            ("loungeIdToken", token),
            (
                "device",
                if recovery {
                    "LOUNGE_SCREEN"
                } else {
                    "REMOTE_CONTROL"
                }
                .into(),
            ),
            ("name", self.0.join_name.clone()),
        ];
        if recovery {
            body.push(("sessionNonce", uuid::Uuid::new_v4().to_string()));
        }
        body
    }
}

fn command_form(offset: u64, command: Command) -> Result<Form, LoungeError> {
    let mut form = vec![("count", "1".into()), ("ofs", offset.to_string())];
    let name = match command {
        Command::SeekTo { seconds } => {
            if !seconds.is_finite() || seconds < 0.0 {
                return Err(LoungeError::InvalidCommand);
            }
            form.push(("req0_newTime", seconds.to_string()));
            "seekTo"
        }
        Command::SkipAd => "skipAd",
        Command::SetVolume { volume, muted } => {
            if !volume.is_finite() {
                return Err(LoungeError::InvalidCommand);
            }
            form.push(("req0_volume", volume.to_string()));
            form.push(("req0_muted", muted.to_string()));
            "setVolume"
        }
        Command::SetPlaylist { video_id } => {
            if video_id.is_empty() {
                return Err(LoungeError::InvalidCommand);
            }
            form.push(("req0_videoId", video_id));
            "setPlaylist"
        }
        Command::GetNowPlaying => "getNowPlaying",
        Command::SetAutoplayMode { enabled } => {
            form.push((
                "req0_autoplayMode",
                if enabled { "ENABLED" } else { "DISABLED" }.into(),
            ));
            "setAutoplayMode"
        }
    };
    form.push(("req0__sc", name.into()));
    Ok(form)
}

fn check_status(status: StatusCode, body: &[u8], connecting: bool) -> Result<(), LoungeError> {
    if status == StatusCode::OK {
        return Ok(());
    }
    let contains = |needle: &[u8]| body.windows(needle.len()).any(|window| window == needle);
    Err(match status.as_u16() {
        400 if contains(b"Unknown SID") => LoungeError::UnknownSid,
        410 if contains(b"Gone") => LoungeError::Gone,
        401 if connecting && contains(b"Connection denied") => LoungeError::ConnectionDenied,
        401 if connecting || contains(b"Expired") => LoungeError::Expired,
        code => LoungeError::Status(code),
    })
}

async fn bounded_error_body(response: &mut HttpStreamResponse) -> Result<Vec<u8>, HttpError> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if chunk.len() > MAX_BODY_BYTES - body.len() {
            return Err(HttpError::BodyTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

pub struct LoungeStream {
    response: HttpStreamResponse,
    decoder: FrameDecoder,
    ended: bool,
}

impl LoungeStream {
    /// The only await is the cancellation-safe HTTP chunk read. Decoder state
    /// and already-read bytes survive cancellation of this future. Dropping the
    /// stream closes its response; no detached reader tasks exist.
    pub async fn next_batch(&mut self) -> Result<Option<Vec<Envelope>>, LoungeError> {
        if self.ended {
            return Ok(None);
        }
        loop {
            match self.decoder.next_batch() {
                Ok(Some(batch)) => return Ok(Some(batch)),
                Ok(None) => {}
                Err(error) => {
                    self.ended = true;
                    return Err(error.into());
                }
            }
            match self.response.chunk().await {
                Ok(Some(chunk)) => {
                    if let Err(error) = self.decoder.push(&chunk) {
                        self.ended = true;
                        return Err(error.into());
                    }
                }
                Ok(None) => {
                    self.ended = true;
                    self.decoder.finish()?;
                    return Ok(None);
                }
                Err(error) => {
                    self.ended = true;
                    return Err(error.into());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_status_uses_body_never_reason_phrase() {
        assert!(matches!(
            check_status(StatusCode::BAD_REQUEST, b"Unknown SID", false),
            Err(LoungeError::UnknownSid)
        ));
        assert!(matches!(
            check_status(StatusCode::GONE, b"Gone", false),
            Err(LoungeError::Gone)
        ));
        assert!(matches!(
            check_status(StatusCode::UNAUTHORIZED, b"Expired", false),
            Err(LoungeError::Expired)
        ));
        assert!(matches!(
            check_status(StatusCode::UNAUTHORIZED, b"Connection denied", true),
            Err(LoungeError::ConnectionDenied)
        ));
        assert!(matches!(
            check_status(StatusCode::UNAUTHORIZED, b"other", true),
            Err(LoungeError::Expired)
        ));
        assert!(matches!(
            check_status(StatusCode::BAD_REQUEST, b"", false),
            Err(LoungeError::Status(400))
        ));
    }

    #[test]
    fn command_mapping_and_invalid_parameters() {
        for (command, name, argument) in [
            (
                Command::SeekTo { seconds: 1.25 },
                "seekTo",
                Some(("req0_newTime", "1.25")),
            ),
            (Command::SkipAd, "skipAd", None),
            (
                Command::SetVolume {
                    volume: 52.0,
                    muted: true,
                },
                "setVolume",
                Some(("req0_muted", "true")),
            ),
            (
                Command::SetPlaylist {
                    video_id: "video".into(),
                },
                "setPlaylist",
                Some(("req0_videoId", "video")),
            ),
            (Command::GetNowPlaying, "getNowPlaying", None),
            (
                Command::SetAutoplayMode { enabled: false },
                "setAutoplayMode",
                Some(("req0_autoplayMode", "DISABLED")),
            ),
        ] {
            let form = command_form(7, command).unwrap();
            assert!(form.contains(&("req0__sc", name.into())));
            assert!(form.contains(&("ofs", "7".into())));
            if let Some((key, value)) = argument {
                assert!(form.contains(&(key, value.into())));
            }
        }
        assert!(command_form(1, Command::SeekTo { seconds: f64::NAN }).is_err());
        assert!(
            command_form(
                1,
                Command::SetVolume {
                    volume: f64::INFINITY,
                    muted: false
                }
            )
            .is_err()
        );
    }
}
