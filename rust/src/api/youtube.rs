use reqwest::Method;
use serde::{Deserialize, de::DeserializeOwned};

use super::{ApiClient, ApiError, ChannelResult, decode};

#[derive(Deserialize)]
struct Envelope<T> {
    items: Option<Vec<T>>,
    error: Option<serde::de::IgnoredAny>,
}

#[derive(Deserialize)]
struct Video {
    snippet: VideoSnippet,
}

#[derive(Deserialize)]
struct VideoSnippet {
    #[serde(rename = "channelId")]
    channel_id: String,
}

#[derive(Deserialize)]
struct SearchItem {
    snippet: SearchSnippet,
}

#[derive(Deserialize)]
struct SearchSnippet {
    #[serde(rename = "channelId")]
    channel_id: String,
    #[serde(rename = "channelTitle")]
    channel_title: Option<String>,
    title: Option<String>,
}

#[derive(Deserialize)]
struct Channel {
    statistics: Statistics,
}

#[derive(Deserialize)]
struct Statistics {
    #[serde(rename = "hiddenSubscriberCount")]
    hidden: bool,
    #[serde(rename = "subscriberCount")]
    subscribers: Option<String>,
}

fn malformed() -> ApiError {
    ApiError::MalformedResponse { service: "YouTube" }
}

fn group_count(value: &str) -> Result<String, ApiError> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(malformed());
    }
    // Avoid integer conversion/overflow; the protocol encodes counts as decimal strings.
    let value = value.trim_start_matches('0');
    if value.is_empty() {
        return Ok("0".to_owned());
    }
    let mut result = String::with_capacity(value.len() + (value.len() - 1) / 3);
    for (index, digit) in value.bytes().enumerate() {
        if index > 0 && (value.len() - index).is_multiple_of(3) {
            result.push('_');
        }
        result.push(char::from(digit));
    }
    Ok(result)
}

impl ApiClient {
    async fn youtube_items<T: DeserializeOwned>(
        &self,
        route: &str,
        params: &[(&str, &str)],
    ) -> Result<Vec<T>, ApiError> {
        if self.state.options.apikey.is_empty() {
            return Err(ApiError::MissingApiKey);
        }
        let url = self
            .state
            .endpoints
            .youtube
            .join(route)
            .map_err(|_| crate::http::HttpError::InvalidEndpoint)?;
        let request = self
            .state
            .http
            .cloud
            .request(Method::GET, url)
            .query(params)
            .query(&[("key", self.state.options.apikey.as_str())]);
        let response = self.state.http.cloud.execute(request).await?;
        let envelope: Envelope<T> = decode(response, "YouTube")?;
        if envelope.error.is_some() {
            return Err(ApiError::YoutubeRejected);
        }
        envelope.items.ok_or_else(malformed)
    }

    pub async fn channel_for_video(&self, video_id: &str) -> Result<Option<String>, ApiError> {
        let items: Vec<Video> = self
            .youtube_items("videos", &[("id", video_id), ("part", "snippet")])
            .await?;
        match items.into_iter().next() {
            None => Ok(None),
            Some(item) if !item.snippet.channel_id.is_empty() => Ok(Some(item.snippet.channel_id)),
            Some(_) => Err(malformed()),
        }
    }

    pub async fn search_channels(&self, query: &str) -> Result<Vec<ChannelResult>, ApiError> {
        if let Some(result) = self
            .state
            .searches
            .lock()
            .map_err(|_| ApiError::CacheUnavailable)?
            .get(query)
            .cloned()
        {
            return Ok(result);
        }
        let items: Vec<SearchItem> = self
            .youtube_items(
                "search",
                &[
                    ("q", query),
                    ("part", "snippet"),
                    ("type", "channel"),
                    ("maxResults", "5"),
                ],
            )
            .await?;
        if items.len() > 5 {
            return Err(malformed());
        }
        let mut results = Vec::with_capacity(items.len());
        for item in items {
            let id = item.snippet.channel_id;
            if id.is_empty() {
                return Err(malformed());
            }
            let name = item
                .snippet
                .channel_title
                .or(item.snippet.title)
                .ok_or_else(malformed)?;
            let details: Vec<Channel> = self
                .youtube_items("channels", &[("id", id.as_str()), ("part", "statistics")])
                .await?;
            // A channel can disappear between search and detail lookup. Keep the
            // usable search result, but do not pretend a missing count is zero.
            let subscribers = match details.into_iter().next() {
                None => "Hidden".to_owned(),
                Some(channel) if channel.statistics.hidden => "Hidden".to_owned(),
                Some(channel) => group_count(
                    channel
                        .statistics
                        .subscribers
                        .as_deref()
                        .ok_or_else(malformed)?,
                )?,
            };
            results.push(ChannelResult {
                id,
                name,
                subscribers,
            });
        }
        self.state
            .searches
            .lock()
            .map_err(|_| ApiError::CacheUnavailable)?
            .put(query.to_owned(), results.clone());
        Ok(results)
    }
}
