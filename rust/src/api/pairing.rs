use reqwest::{Method, StatusCode};
use serde::Deserialize;

use super::{ApiClient, ApiError, PairingResult, decode};

#[derive(Deserialize)]
struct PairingResponse {
    screen: Screen,
}

#[derive(Deserialize)]
struct Screen {
    #[serde(rename = "screenId")]
    screen_id: String,
    name: Option<String>,
    #[serde(rename = "loungeToken")]
    lounge_token: Option<String>,
}

impl ApiClient {
    /// Normalize only ASCII spaces/hyphens, keeping all leading zeros. Interactive
    /// callers validate twelve ASCII digits; discovery also uses this operation.
    /// HTTP 400/404 reject a code. Other statuses indicate service/auth failures.
    pub async fn pair_with_code(&self, code: &str) -> Result<Option<PairingResult>, ApiError> {
        let code = code.replace([' ', '-'], "");
        let url = self
            .state
            .endpoints
            .lounge
            .join("pairing/get_screen")
            .map_err(|_| crate::http::HttpError::InvalidEndpoint)?;
        let request = self
            .state
            .http
            .cloud
            .request(Method::POST, url)
            .form(&[("pairing_code", code.as_str())]);
        let response = self.state.http.cloud.execute(request).await?;
        if matches!(
            response.status,
            StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND
        ) {
            return Ok(None);
        }
        let response: PairingResponse = decode(response, "pairing")?;
        if response.screen.screen_id.is_empty() {
            return Err(ApiError::MalformedResponse { service: "pairing" });
        }
        Ok(Some(PairingResult {
            screen_id: response.screen.screen_id,
            name: response.screen.name,
            lounge_token: response.screen.lounge_token,
        }))
    }
}
