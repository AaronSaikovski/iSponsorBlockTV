//! Direct SponsorBlock adapter on the shared HTTP stack. The published
//! sponsor-block 0.6.1 client's closed categories, f32 timestamps and private
//! reqwest client cannot preserve this application's configuration contract.
use std::{
    collections::{HashMap, HashSet},
    num::NonZeroUsize,
    sync::{Arc, Mutex},
    time::Duration,
};

use futures_util::{
    FutureExt,
    future::{BoxFuture, WeakShared},
};
use lru::LruCache;
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::time::Instant;

use super::{ApiClient, ApiError, ApiOptions};
use crate::{
    config::RuntimeConfig,
    http::{Endpoints, HttpClients},
};

const SERVICE: &str = "SponsorBlock";
const TTL: Duration = Duration::from_secs(300);

#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    pub start: f64,
    pub end: f64,
    pub uuids: Vec<String>,
}

struct Cached {
    segments: Arc<[Segment]>,
    expiry: Option<Instant>,
}

type SegmentLookup = BoxFuture<'static, Result<Arc<[Segment]>, ApiError>>;

struct Lookups {
    cache: LruCache<Arc<str>, Cached>,
    in_flight: HashMap<Arc<str>, WeakShared<SegmentLookup>>,
}

struct State {
    api: ApiClient,
    categories: Vec<String>,
    minimum: f64,
    tracking: bool,
    lookups: Mutex<Lookups>,
}

/// Clones share immutable options, bounded caches, and active per-video lookups.
/// Construct a fresh service when configuration changes. No mutex is held during
/// network work; dropping the last waiter cancels that video's lookup.
#[derive(Clone)]
pub struct ApiService {
    state: Arc<State>,
}

struct InFlight {
    service: ApiService,
    video_id: Arc<str>,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        if let Ok(mut lookups) = self.service.state.lookups.lock()
            && lookups
                .in_flight
                .get_key_value(self.video_id.as_ref())
                .is_some_and(|(video_id, _)| Arc::ptr_eq(video_id, &self.video_id))
        {
            // A cancelled generation may finish dropping after a new lookup
            // registered; its cleanup must never remove the replacement.
            lookups.in_flight.remove(self.video_id.as_ref());
        }
    }
}

impl ApiService {
    /// Injected endpoints take precedence; production composition supplies the
    /// configured SponsorBlock base in `endpoints.sponsorblock`.
    pub fn new(
        config: &RuntimeConfig,
        endpoints: Endpoints,
        http_tracing: bool,
    ) -> Result<Self, ApiError> {
        let api = ApiClient::new(
            ApiOptions {
                apikey: config.apikey.clone(),
                use_proxy: config.use_proxy,
                http_tracing,
                channel_whitelist: config
                    .channel_whitelist
                    .iter()
                    .map(|channel| channel.id.clone())
                    .collect(),
            },
            endpoints,
        )?;
        Ok(Self::from_api(config, api))
    }

    /// Reuse the supplied transport policy; only API/service settings are read
    /// from `config`, never its proxy setting. Each service gets fresh caches.
    pub(crate) fn with_http(
        config: &RuntimeConfig,
        endpoints: Endpoints,
        http: HttpClients,
    ) -> Result<Self, ApiError> {
        let api = ApiClient::from_http(
            config.apikey.clone(),
            config
                .channel_whitelist
                .iter()
                .map(|channel| channel.id.clone())
                .collect(),
            endpoints.normalized()?,
            http,
        );
        Ok(Self::from_api(config, api))
    }

    fn from_api(config: &RuntimeConfig, api: ApiClient) -> Self {
        Self {
            state: Arc::new(State {
                api,
                categories: config.skip_categories.clone(),
                minimum: config.minimum_skip_length,
                tracking: config.skip_count_tracking,
                lookups: Mutex::new(Lookups {
                    cache: LruCache::new(NonZeroUsize::new(10).expect("nonzero capacity")),
                    in_flight: HashMap::new(),
                }),
            }),
        }
    }

    pub async fn segments(&self, video_id: &str) -> Result<Arc<[Segment]>, ApiError> {
        if self.state.categories.is_empty() {
            return Ok(Arc::from([]));
        }
        let lookup = {
            // Cache inspection and registration share one lock, so a completed
            // request cannot slip between a cache miss and a new lookup.
            let mut lookups = self
                .state
                .lookups
                .lock()
                .map_err(|_| ApiError::CacheUnavailable)?;
            if let Some(entry) = lookups.cache.get(video_id) {
                if entry.expiry.is_none_or(|expiry| Instant::now() < expiry) {
                    return Ok(Arc::clone(&entry.segments));
                }
                lookups.cache.pop(video_id);
            }
            if let Some(lookup) = lookups
                .in_flight
                .get(video_id)
                .and_then(WeakShared::upgrade)
            {
                lookup
            } else {
                // Weak entries never keep HTTP work alive without a waiter.
                // Remove a dead generation first: HashMap::insert would retain
                // its key allocation and defeat identity-based drop cleanup.
                lookups.in_flight.remove(video_id);
                let video_id: Arc<str> = Arc::from(video_id);
                let registration = InFlight {
                    service: self.clone(),
                    video_id: Arc::clone(&video_id),
                };
                let lookup = async move {
                    registration
                        .service
                        .fetch_segments(&registration.video_id)
                        .await
                }
                .boxed()
                .shared();
                lookups.in_flight.insert(
                    video_id,
                    lookup.downgrade().expect("new lookup has not completed"),
                );
                lookup
            }
        };
        lookup.await
    }

    async fn fetch_segments(&self, video_id: &Arc<str>) -> Result<Arc<[Segment]>, ApiError> {
        let (segments, locked) = if self.state.api.is_whitelisted(video_id).await? {
            (Vec::new(), false)
        } else {
            let hash = Sha256::digest(video_id.as_bytes());
            let path = format!("skipSegments/{:02x}{:02x}", hash[0], hash[1]);
            let api = &self.state.api.state;
            let mut url = api
                .endpoints
                .sponsorblock
                .join(&path)
                .map_err(|_| crate::http::HttpError::InvalidEndpoint)?;
            {
                let mut query = url.query_pairs_mut();
                for category in &self.state.categories {
                    query.append_pair("category", category);
                }
                query
                    .append_pair("actionType", "skip")
                    .append_pair("service", "youtube");
            }
            let response = api
                .http
                .cloud
                .execute(
                    api.http
                        .cloud
                        .request(Method::GET, url)
                        .header(reqwest::header::ACCEPT, "application/json"),
                )
                .await?;
            match response.status {
                StatusCode::NOT_FOUND => (Vec::new(), false),
                StatusCode::OK => process(&response.body, video_id, self.state.minimum)?,
                status => {
                    return Err(ApiError::Status {
                        service: SERVICE,
                        status: status.as_u16(),
                    });
                }
            }
        };
        let segments: Arc<[Segment]> = segments.into();
        // Even an all-locked response filtered down to nothing expires.
        let expiry = if locked && !segments.is_empty() {
            None
        } else {
            Some(Instant::now() + TTL)
        };
        self.state
            .lookups
            .lock()
            .map_err(|_| ApiError::CacheUnavailable)?
            .cache
            .put(
                Arc::clone(video_id),
                Cached {
                    segments: Arc::clone(&segments),
                    expiry,
                },
            );
        Ok(segments)
    }

    /// Call only after an accepted seek for the still-current video/session.
    /// Failures are returned without retry; all distinct UUIDs are attempted.
    pub async fn report_viewed(&self, uuids: &[String]) -> Result<(), ApiError> {
        if !self.state.tracking {
            return Ok(());
        }
        let api = &self.state.api.state;
        let mut seen = HashSet::with_capacity(uuids.len());
        let mut failure = None;
        for uuid in uuids {
            if !seen.insert(uuid) {
                continue;
            }
            let mut url = api
                .endpoints
                .sponsorblock
                .join("viewedVideoSponsorTime/")
                .map_err(|_| crate::http::HttpError::InvalidEndpoint)?;
            url.query_pairs_mut().append_pair("UUID", uuid);
            let result = api
                .http
                .cloud
                .execute(api.http.cloud.request(Method::POST, url))
                .await;
            let error = match result {
                Ok(response) if response.status.is_success() => None,
                Ok(response) => Some(ApiError::Status {
                    service: SERVICE,
                    status: response.status.as_u16(),
                }),
                Err(error) => Some(error.into()),
            };
            if let Some(error) = error {
                tracing::warn!(%error, "SponsorBlock viewed reporting failed; not retrying");
                if failure.is_none() {
                    failure = Some(error);
                }
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

#[derive(Deserialize)]
struct Bucket {
    #[serde(rename = "videoID")]
    video_id: String,
    segments: serde_json::Value,
}

#[derive(Deserialize)]
struct RawSegment {
    segment: [f64; 2],
    #[serde(rename = "UUID")]
    uuid: String,
    locked: u8,
}

fn malformed() -> ApiError {
    ApiError::MalformedResponse { service: SERVICE }
}

fn process(body: &[u8], video_id: &str, minimum: f64) -> Result<(Vec<Segment>, bool), ApiError> {
    let buckets: Vec<Bucket> = serde_json::from_slice(body).map_err(|_| malformed())?;
    let mut raw = Vec::new();
    for bucket in buckets
        .into_iter()
        .filter(|bucket| bucket.video_id == video_id)
    {
        let segments: Vec<RawSegment> =
            serde_json::from_value(bucket.segments).map_err(|_| malformed())?;
        raw.extend(segments);
    }
    let locked = !raw.is_empty() && raw.iter().all(|segment| segment.locked == 1);
    if raw.iter().any(|segment| {
        let [start, end] = segment.segment;
        !start.is_finite()
            || !end.is_finite()
            || start < 0.0
            || end <= start
            || segment.uuid.is_empty()
    }) {
        return Err(malformed());
    }
    raw.sort_by(|a, b| {
        a.segment[0]
            .total_cmp(&b.segment[0])
            .then(a.segment[1].total_cmp(&b.segment[1]))
    });
    let mut merged: Vec<Segment> = Vec::with_capacity(raw.len());
    for item in raw {
        let [start, end] = item.segment;
        if let Some(previous) = merged.last_mut()
            && start - previous.end < 1.0
        {
            previous.end = previous.end.max(end);
            if !previous.uuids.contains(&item.uuid) {
                previous.uuids.push(item.uuid);
            }
            continue;
        }
        merged.push(Segment {
            start,
            end,
            uuids: vec![item.uuid],
        });
    }
    merged.retain(|segment| segment.end - segment.start > minimum);
    Ok((merged, locked))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(segments: serde_json::Value, minimum: f64) -> Result<(Vec<Segment>, bool), ApiError> {
        process(
            &serde_json::to_vec(&json!([{ "videoID": "v", "segments": segments }])).unwrap(),
            "v",
            minimum,
        )
    }
    fn segment(start: f64, end: f64, uuid: &str, locked: u8) -> serde_json::Value {
        json!({"segment": [start, end], "UUID": uuid, "locked": locked})
    }

    #[test]
    fn merges_before_filter_and_keeps_all_uuids() {
        let (segments, locked) = run(
            json!([segment(1.1, 1.7, "b", 1), segment(0.0, 0.6, "a", 1)]),
            1.0,
        )
        .unwrap();
        assert_eq!(
            segments,
            vec![Segment {
                start: 0.0,
                end: 1.7,
                uuids: vec!["a".into(), "b".into()]
            }]
        );
        assert!(locked);
    }

    #[test]
    fn gap_and_duration_boundaries_are_strict() {
        let (merged, _) = run(
            json!([segment(0.0, 1.0, "a", 0), segment(1.999, 3.0, "b", 1)]),
            1.0,
        )
        .unwrap();
        assert_eq!(merged.len(), 1);
        let (separate, locked) = run(
            json!([segment(0.0, 1.0, "a", 0), segment(2.0, 4.0, "b", 1)]),
            1.0,
        )
        .unwrap();
        assert_eq!(separate.len(), 1);
        assert_eq!(separate[0].start, 2.0);
        assert!(!locked); // Original filtered segment still determines locking.
    }

    #[test]
    fn nesting_overlap_permutation_and_uuid_dedup() {
        let input = vec![
            segment(0.0, 10.0, "a", 1),
            segment(2.0, 3.0, "b", 1),
            segment(3.0, 12.0, "a", 1),
        ];
        let expected = run(json!(input), 0.0).unwrap();
        let reverse: Vec<_> = input.into_iter().rev().collect();
        assert_eq!(run(json!(reverse), 0.0).unwrap(), expected);
        assert_eq!(expected.0[0].end, 12.0);
        assert_eq!(expected.0[0].uuids, ["a", "b"]);
    }

    #[test]
    fn invalid_selected_segment_fails_whole_response() {
        for invalid in [
            json!({"segment":[1],"UUID":"b","locked":1}),
            segment(-1.0, 2.0, "b", 1),
            segment(1.0, 1.0, "b", 1),
            segment(1.0, 0.0, "b", 1),
        ] {
            assert!(run(json!([segment(0.0, 2.0, "a", 1), invalid]), 0.0).is_err());
        }
        assert!(run(json!([]), 0.0).unwrap().0.is_empty());
        assert!(!run(json!([]), 0.0).unwrap().1);
        assert!(
            process(br#"[{"videoID":"other","segments":null}]"#, "v", 0.0)
                .unwrap()
                .0
                .is_empty()
        );
    }
}
