//! Direct SponsorBlock adapter on the shared HTTP stack. The published
//! sponsor-block 0.6.1 client's closed categories, f32 timestamps and private
//! reqwest client cannot preserve this application's configuration contract.
use std::{
    collections::HashSet,
    num::NonZeroUsize,
    sync::{Arc, Mutex},
    time::Duration,
};

use lru::LruCache;
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::time::Instant;

use super::{ApiClient, ApiError, ApiOptions};
use crate::{config::RuntimeConfig, http::Endpoints};

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

struct State {
    api: ApiClient,
    categories: Vec<String>,
    minimum: f64,
    tracking: bool,
    cache: Mutex<LruCache<String, Cached>>,
}

/// Clones share immutable options and bounded caches. Construct a fresh service
/// when configuration changes. No cache mutex is held during network work.
#[derive(Clone)]
pub struct ApiService {
    state: Arc<State>,
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
        Ok(Self {
            state: Arc::new(State {
                api,
                categories: config.skip_categories.clone(),
                minimum: config.minimum_skip_length,
                tracking: config.skip_count_tracking,
                cache: Mutex::new(LruCache::new(
                    NonZeroUsize::new(10).expect("nonzero capacity"),
                )),
            }),
        })
    }

    pub async fn segments(&self, video_id: &str) -> Result<Arc<[Segment]>, ApiError> {
        if self.state.categories.is_empty() {
            return Ok(Arc::from([]));
        }
        {
            let mut cache = self
                .state
                .cache
                .lock()
                .map_err(|_| ApiError::CacheUnavailable)?;
            if let Some(entry) = cache.get(video_id) {
                if entry.expiry.is_none_or(|expiry| Instant::now() < expiry) {
                    return Ok(Arc::clone(&entry.segments));
                }
                cache.pop(video_id);
            }
        }
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
            .cache
            .lock()
            .map_err(|_| ApiError::CacheUnavailable)?
            .put(
                video_id.to_owned(),
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
