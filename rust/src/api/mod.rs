//! Pairing and YouTube operations share immutable network/key/whitelist options.
//! Clone clients to share caches; construct a new client when setup options change.
mod pairing;
pub mod sponsorblock;
mod youtube;
pub use sponsorblock::{ApiService, Segment};

use std::{
    fmt,
    num::NonZeroUsize,
    sync::{Arc, Mutex},
};

use lru::LruCache;
use serde::de::DeserializeOwned;
use thiserror::Error;

use crate::http::{Endpoints, HttpClients, HttpError, HttpResponse};

#[derive(Clone, Default)]
pub struct ApiOptions {
    pub apikey: String,
    pub use_proxy: bool,
    pub http_tracing: bool,
    pub channel_whitelist: Vec<String>,
}

impl fmt::Debug for ApiOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiOptions")
            .field("apikey", &"[redacted]")
            .field("use_proxy", &self.use_proxy)
            .field("http_tracing", &self.http_tracing)
            .field("whitelist_count", &self.channel_whitelist.len())
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct PairingResult {
    pub screen_id: String,
    pub name: Option<String>,
    pub lounge_token: Option<String>,
}

impl fmt::Debug for PairingResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairingResult")
            .field("screen_id", &"[redacted]")
            .field("name", &"[redacted]")
            .field("lounge_token", &"[redacted]")
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelResult {
    pub id: String,
    pub name: String,
    pub subscribers: String,
}

#[derive(Clone, Debug, Error)]
pub enum ApiError {
    #[error(transparent)]
    Http(#[from] HttpError),
    #[error("{service} returned HTTP {status}; check credentials, quota, and service availability")]
    Status { service: &'static str, status: u16 },
    #[error("{service} returned a malformed response")]
    MalformedResponse { service: &'static str },
    #[error(
        "YouTube rejected the API request; check the API key, enabled YouTube Data API, and quota"
    )]
    YoutubeRejected,
    #[error("a YouTube API key is required for channel search and whitelist lookup")]
    MissingApiKey,
    #[error("API cache is unavailable")]
    CacheUnavailable,
}

struct ApiState {
    apikey: String,
    channel_whitelist: Vec<String>,
    endpoints: Endpoints,
    http: HttpClients,
    searches: Mutex<LruCache<String, Vec<ChannelResult>>>,
    whitelist: Mutex<LruCache<String, bool>>,
}

#[derive(Clone)]
pub struct ApiClient {
    state: Arc<ApiState>,
}

impl ApiClient {
    pub fn new(options: ApiOptions, endpoints: Endpoints) -> Result<Self, ApiError> {
        let endpoints = endpoints.normalized()?;
        let http = HttpClients::new(options.use_proxy, options.http_tracing)?;
        Ok(Self::from_http(
            options.apikey,
            options.channel_whitelist,
            endpoints,
            http,
        ))
    }

    /// Endpoints must already be normalized. Transport policy belongs to `http`;
    /// each construction gets independent immutable key/whitelist caches.
    fn from_http(
        apikey: String,
        channel_whitelist: Vec<String>,
        endpoints: Endpoints,
        http: HttpClients,
    ) -> Self {
        Self {
            state: Arc::new(ApiState {
                apikey,
                channel_whitelist,
                endpoints,
                http,
                searches: Mutex::new(LruCache::new(
                    NonZeroUsize::new(10).expect("nonzero capacity"),
                )),
                whitelist: Mutex::new(LruCache::new(
                    NonZeroUsize::new(100).expect("nonzero capacity"),
                )),
            }),
        }
    }

    /// Whitelisting bypasses SponsorBlock only; cache errors are never decisions.
    pub async fn is_whitelisted(&self, video_id: &str) -> Result<bool, ApiError> {
        if self.state.channel_whitelist.is_empty() {
            return Ok(false);
        }
        if let Some(result) = self
            .state
            .whitelist
            .lock()
            .map_err(|_| ApiError::CacheUnavailable)?
            .get(video_id)
            .copied()
        {
            return Ok(result);
        }
        let channel = self.channel_for_video(video_id).await?;
        let result = channel.is_some_and(|id| self.state.channel_whitelist.contains(&id));
        self.state
            .whitelist
            .lock()
            .map_err(|_| ApiError::CacheUnavailable)?
            .put(video_id.to_owned(), result);
        Ok(result)
    }
}

fn decode<T: DeserializeOwned>(
    response: HttpResponse,
    service: &'static str,
) -> Result<T, ApiError> {
    if response.status != reqwest::StatusCode::OK {
        return Err(ApiError::Status {
            service,
            status: response.status.as_u16(),
        });
    }
    serde_json::from_slice(&response.body).map_err(|_| ApiError::MalformedResponse { service })
}
