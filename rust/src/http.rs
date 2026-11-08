//! Shared transport policies. Only Cast-local metadata accepts self-signed TLS;
//! callers must additionally enforce discovery's responder/subnet admission rules.
use std::{
    net::{IpAddr, SocketAddr},
    time::{Duration, Instant},
};

use bytes::{Bytes, BytesMut};
use reqwest::{Client, ClientBuilder, Method, RequestBuilder, StatusCode, header::HeaderMap};
use thiserror::Error;
use url::Url;

pub const MAX_BODY_BYTES: usize = 1024 * 1024;
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
pub struct Endpoints {
    pub lounge: Url,
    pub youtube: Url,
    pub sponsorblock: Url,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            lounge: Url::parse("https://www.youtube.com/api/lounge/").expect("fixed URL"),
            youtube: Url::parse("https://www.googleapis.com/youtube/v3/").expect("fixed URL"),
            sponsorblock: Url::parse("https://sponsor.ajay.app/api/").expect("fixed URL"),
        }
    }
}

impl Endpoints {
    pub fn normalized(mut self) -> Result<Self, HttpError> {
        for url in [&mut self.lounge, &mut self.youtube, &mut self.sponsorblock] {
            validate_base(url)?;
            if !url.path().ends_with('/') {
                let path = format!("{}/", url.path());
                url.set_path(&path);
            }
        }
        Ok(self)
    }
}

pub fn validate_base(url: &Url) -> Result<(), HttpError> {
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(HttpError::InvalidEndpoint);
    }
    Ok(())
}

/// Errors deliberately retain no reqwest source, URL, headers, or response body.
#[derive(Clone, Debug, Error)]
pub enum HttpError {
    #[error("invalid HTTP endpoint; use an HTTP(S) base without credentials, query, or fragment")]
    InvalidEndpoint,
    #[error("unable to construct HTTP client; check TLS and proxy configuration")]
    Configuration,
    #[error("HTTP request timed out")]
    Timeout,
    #[error("HTTP transport failed; check the connection and proxy settings")]
    Transport,
    #[error("HTTP response exceeds the 1 MiB body limit")]
    BodyTooLarge,
    #[error("Cast-local requests require a literal unicast LAN address")]
    InvalidCastAddress,
}

fn transport_error(error: reqwest::Error) -> HttpError {
    if error.is_timeout() {
        HttpError::Timeout
    } else {
        HttpError::Transport
    }
}

fn base_builder(stream: bool) -> ClientBuilder {
    let builder = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(CONNECT_TIMEOUT);
    if stream {
        builder
    } else {
        builder.timeout(REQUEST_TIMEOUT)
    }
}

/// Capture proxy environment once per client. Explicit proxy support does not
/// enable reqwest's system-proxy feature (or platform-specific proxy discovery).
fn cloud_builder(stream: bool, use_proxy: bool) -> Result<ClientBuilder, HttpError> {
    let mut builder = base_builder(stream);
    if !use_proxy {
        return Ok(builder);
    }
    let env = |upper: &str, lower: &str| {
        std::env::var(upper)
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var(lower).ok().filter(|s| !s.is_empty()))
    };
    // Capture uppercase precedence, including an explicitly empty value.
    let no_proxy = std::env::var("NO_PROXY")
        .or_else(|_| std::env::var("no_proxy"))
        .ok();
    // hyper-util's matcher applies "*" only to DNS names, not literal IPs.
    // A global bypass means no proxy for either kind of destination.
    if no_proxy
        .as_deref()
        .is_some_and(|value| value.split(',').any(|entry| entry.trim() == "*"))
    {
        return Ok(builder);
    }
    let bypass = no_proxy.as_deref().and_then(reqwest::NoProxy::from_string);
    let all = env("ALL_PROXY", "all_proxy");
    for (scheme, value) in [
        (
            "http",
            env("HTTP_PROXY", "http_proxy").or_else(|| all.clone()),
        ),
        ("https", env("HTTPS_PROXY", "https_proxy").or(all)),
    ] {
        if let Some(value) = value {
            let proxy = if scheme == "http" {
                reqwest::Proxy::http(value)
            } else {
                reqwest::Proxy::https(value)
            }
            .map_err(|_| HttpError::Configuration)?;
            builder = builder.proxy(proxy.no_proxy(bypass.clone()));
        }
    }
    Ok(builder)
}

#[derive(Clone)]
pub struct HttpClient {
    inner: Client,
    tracing: bool,
}

pub struct HttpResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl HttpClient {
    fn build(builder: ClientBuilder, tracing: bool) -> Result<Self, HttpError> {
        Ok(Self {
            inner: builder.build().map_err(|_| HttpError::Configuration)?,
            tracing,
        })
    }

    pub fn request(&self, method: Method, url: Url) -> RequestBuilder {
        self.inner.request(method, url)
    }

    pub async fn execute(&self, request: RequestBuilder) -> Result<HttpResponse, HttpError> {
        let request = request.build().map_err(transport_error)?;
        let mut trace = Trace::new(&request, self.tracing);
        let mut response = self.inner.execute(request).await.map_err(transport_error)?;
        let status = response.status();
        if let Some(trace) = &mut trace {
            trace.status = Some(status);
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_BODY_BYTES as u64)
        {
            return Err(HttpError::BodyTooLarge);
        }
        let headers = std::mem::take(response.headers_mut());
        let mut body = BytesMut::new();
        while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
            if let Some(trace) = &mut trace {
                trace.received = trace.received.saturating_add(chunk.len());
            }
            if chunk.len() > MAX_BODY_BYTES - body.len() {
                return Err(HttpError::BodyTooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        Ok(HttpResponse {
            status,
            headers,
            body: body.freeze(),
        })
    }

    /// No total deadline: the Lounge reader owns its inactivity timer. The
    /// response accounts for streamed bytes without collecting or capping it.
    pub async fn execute_stream(
        &self,
        request: RequestBuilder,
    ) -> Result<HttpStreamResponse, HttpError> {
        let request = request.build().map_err(transport_error)?;
        let mut trace = Trace::new(&request, self.tracing);
        let response = self.inner.execute(request).await.map_err(transport_error)?;
        if let Some(trace) = &mut trace {
            trace.status = Some(response.status());
        }
        Ok(HttpStreamResponse { response, trace })
    }
}

pub struct HttpStreamResponse {
    response: reqwest::Response,
    trace: Option<Trace>,
}

impl HttpStreamResponse {
    pub fn status(&self) -> StatusCode {
        self.response.status()
    }
    pub fn headers(&self) -> &HeaderMap {
        self.response.headers()
    }

    pub async fn chunk(&mut self) -> Result<Option<Bytes>, HttpError> {
        let chunk = self.response.chunk().await.map_err(transport_error)?;
        if let (Some(chunk), Some(trace)) = (&chunk, &mut self.trace) {
            trace.received = trace.received.saturating_add(chunk.len());
        }
        Ok(chunk)
    }
}

struct Trace {
    method: Method,
    url: String,
    sent: usize,
    start: Instant,
    status: Option<StatusCode>,
    received: usize,
}

impl Trace {
    fn new(request: &reqwest::Request, enabled: bool) -> Option<Self> {
        enabled.then(|| Self {
            method: request.method().clone(),
            url: redacted_url(request.url()),
            sent: request
                .body()
                .and_then(|body| body.as_bytes())
                .map_or(0, |body| body.len()),
            start: Instant::now(),
            status: None,
            received: 0,
        })
    }
}

impl Drop for Trace {
    fn drop(&mut self) {
        tracing::debug!(target: "isponsorblocktv::http", method = %self.method,
            url = %self.url, status = self.status.map(|status| status.as_u16()),
            duration_ms = self.start.elapsed().as_millis() as u64,
            request_bytes = self.sent, response_bytes = self.received, "HTTP request");
    }
}

/// Never copy user-controlled path segments, authority, query, or fragment into
/// traces. Static operation names are enough to identify requests safely.
pub fn redacted_url(url: &Url) -> String {
    let route = match url.path().trim_end_matches('/').rsplit('/').next() {
        Some("get_screen") => "pairing/get_screen",
        Some("get_lounge_token_batch") => "pairing/get_lounge_token_batch",
        Some("get_screen_availability") => "pairing/get_screen_availability",
        Some("bind") => "bc/bind",
        Some("videos") => "videos",
        Some("search") => "search",
        Some("channels") => "channels",
        Some("viewedVideoSponsorTime") => "viewedVideoSponsorTime",
        _ => "[redacted]",
    };
    format!("{}://[redacted]/{route}", url.scheme())
}

#[derive(Clone)]
pub struct CastLocalClient {
    inner: HttpClient,
    #[cfg(test)]
    metadata_destination: Option<SocketAddr>,
}

impl CastLocalClient {
    #[cfg(test)]
    fn with_metadata_destination(address: SocketAddr) -> Result<Self, HttpError> {
        if !address.ip().is_loopback() {
            return Err(HttpError::InvalidCastAddress);
        }
        Ok(Self {
            inner: HttpClient::build(cast_builder(), false)?,
            metadata_destination: Some(address),
        })
    }

    /// Fetch Eureka metadata at an address already admitted against discovery's
    /// interface subnets. Globally numbered on-link receivers are supported.
    /// DNS is never consulted: IPv6 scope stays intact in the pinned SocketAddr.
    pub async fn get_metadata(
        &self,
        address: SocketAddr,
        path: &str,
        empty_host: bool,
    ) -> Result<HttpResponse, HttpError> {
        let unicast = match address {
            SocketAddr::V4(address) => {
                let ip = address.ip();
                !ip.is_unspecified()
                    && !ip.is_loopback()
                    && !ip.is_multicast()
                    && !ip.is_broadcast()
            }
            SocketAddr::V6(address) => {
                let ip = address.ip();
                !ip.is_unspecified()
                    && !ip.is_loopback()
                    && !ip.is_multicast()
                    && ip.to_ipv4().is_none()
                    && (!ip.is_unicast_link_local() || address.scope_id() != 0)
            }
        };
        if !unicast {
            return Err(HttpError::InvalidCastAddress);
        }
        let scheme = match address.port() {
            8443 => "https",
            8008 => "http",
            _ => return Err(HttpError::InvalidCastAddress),
        };
        if !matches!(
            path,
            "/setup/eureka_info?params=device_info,name" | "/setup/eureka_info?params=multizone"
        ) {
            return Err(HttpError::InvalidEndpoint);
        }
        let destination = address;
        #[cfg(test)]
        let destination = self.metadata_destination.unwrap_or(destination);
        // A fixed synthetic authority forces reqwest through the pinned DNS
        // override rather than reparsing a scope-less IPv6 URL as an address.
        let url = Url::parse(&format!(
            "{scheme}://cast-local.invalid:{}{path}",
            destination.port()
        ))
        .map_err(|_| HttpError::InvalidEndpoint)?;
        let client = HttpClient::build(
            cast_builder().resolve_to_addrs("cast-local.invalid", &[destination]),
            self.inner.tracing,
        )?;
        let host = if empty_host {
            String::new()
        } else {
            // Scope is a local routing property, not part of HTTP authority.
            SocketAddr::new(address.ip(), address.port()).to_string()
        };
        let request = client
            .request(Method::GET, url)
            .header(reqwest::header::HOST, host);
        client.execute(request).await
    }

    pub fn metadata_request(&self, method: Method, url: Url) -> Result<RequestBuilder, HttpError> {
        validate_cast_url(&url)?;
        Ok(self.inner.request(method, url))
    }

    pub async fn execute(&self, request: RequestBuilder) -> Result<HttpResponse, HttpError> {
        // Recheck after builder modifications; do not allow a verified request
        // builder from another client to bypass this client's LAN boundary.
        let request = request.build().map_err(transport_error)?;
        validate_cast_url(request.url())?;
        self.inner
            .execute(RequestBuilder::from_parts(
                self.inner.inner.clone(),
                request,
            ))
            .await
    }
}

fn validate_cast_url(url: &Url) -> Result<(), HttpError> {
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(HttpError::InvalidCastAddress);
    }
    let ip = match url.host() {
        Some(url::Host::Ipv4(ip)) => IpAddr::V4(ip),
        Some(url::Host::Ipv6(ip)) => IpAddr::V6(ip),
        _ => return Err(HttpError::InvalidCastAddress),
    };
    validate_cast_ip(ip)
}

fn validate_cast_ip(ip: IpAddr) -> Result<(), HttpError> {
    let local = match ip {
        IpAddr::V4(ip) => ip.is_private() || ip.is_link_local(),
        IpAddr::V6(ip) => ip.is_unique_local() || ip.is_unicast_link_local(),
    };
    if !local {
        return Err(HttpError::InvalidCastAddress);
    }
    Ok(())
}

fn cast_builder() -> ClientBuilder {
    base_builder(false)
        .danger_accept_invalid_certs(true)
        .danger_accept_invalid_hostnames(true)
}

#[derive(Clone)]
pub struct HttpClients {
    pub cloud: HttpClient,
    pub lounge_stream: HttpClient,
    pub dial: HttpClient,
    pub cast_local: CastLocalClient,
}

impl HttpClients {
    pub fn new(use_proxy: bool, http_tracing: bool) -> Result<Self, HttpError> {
        Ok(Self {
            cloud: HttpClient::build(cloud_builder(false, use_proxy)?, http_tracing)?,
            lounge_stream: HttpClient::build(cloud_builder(true, use_proxy)?, http_tracing)?,
            dial: HttpClient::build(base_builder(false), http_tracing)?,
            cast_local: CastLocalClient {
                inner: HttpClient::build(cast_builder(), http_tracing)?,
                #[cfg(test)]
                metadata_destination: None,
            },
        })
    }
}

#[cfg(test)]
mod metadata_tests {
    use super::*;
    use std::{
        collections::BTreeMap,
        net::{Ipv6Addr, SocketAddrV6},
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    #[tokio::test]
    async fn scoped_metadata_pins_transport_and_preserves_http_host_retry() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client =
            CastLocalClient::with_metadata_destination(listener.local_addr().unwrap()).unwrap();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for status in [403, 200] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                while !bytes.windows(4).any(|w| w == b"\r\n\r\n") {
                    let mut chunk = [0; 1024];
                    let count = socket.read(&mut chunk).await.unwrap();
                    assert_ne!(count, 0);
                    bytes.extend_from_slice(&chunk[..count]);
                    assert!(bytes.len() < 16384);
                }
                requests.push(String::from_utf8(bytes).unwrap());
                socket.write_all(format!(
                    "HTTP/1.1 {status} Fixture\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
                ).as_bytes()).await.unwrap();
            }
            requests
        });
        let address = SocketAddr::V6(SocketAddrV6::new(
            "fe80::1".parse::<Ipv6Addr>().unwrap(),
            8008,
            0,
            7,
        ));
        let path = "/setup/eureka_info?params=device_info,name";
        assert_eq!(
            client
                .get_metadata(address, path, false)
                .await
                .unwrap()
                .status
                .as_u16(),
            403
        );
        assert_eq!(
            client
                .get_metadata(address, path, true)
                .await
                .unwrap()
                .status
                .as_u16(),
            200
        );
        let requests = server.await.unwrap();
        for (request, expected_host) in requests.iter().zip(["[fe80::1]:8008", ""]) {
            let mut lines = request.split("\r\n");
            assert_eq!(
                lines.next().unwrap(),
                "GET /setup/eureka_info?params=device_info,name HTTP/1.1"
            );
            let headers: BTreeMap<_, _> = lines
                .filter_map(|line| line.split_once(':'))
                .map(|(key, value)| (key.to_ascii_lowercase(), value.trim()))
                .collect();
            assert_eq!(headers["host"], expected_host);
            assert!(!request.contains("cast-local.invalid"));
            assert!(!request.contains("%7"));
        }
    }

    #[tokio::test]
    async fn metadata_keeps_response_cap_and_does_not_follow_redirects() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client =
            CastLocalClient::with_metadata_destination(listener.local_addr().unwrap()).unwrap();
        let server = tokio::spawn(async move {
            for response in [
                "HTTP/1.1 302 Fixture\r\nContent-Length: 0\r\nLocation: http://127.0.0.1:1/\r\nConnection: close\r\n\r\n".to_owned(),
                format!("HTTP/1.1 200 Fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", MAX_BODY_BYTES + 1),
            ] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = [0; 4096];
                assert!(socket.read(&mut bytes).await.unwrap() > 0);
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        // A globally numbered on-link receiver is valid; test-only constructor
        // injection pins its transport to this loopback fixture.
        let address = "192.0.2.42:8008".parse().unwrap();
        let path = "/setup/eureka_info?params=multizone";
        assert_eq!(
            client
                .get_metadata(address, path, false)
                .await
                .unwrap()
                .status
                .as_u16(),
            302
        );
        assert!(matches!(
            client.get_metadata(address, path, false).await,
            Err(HttpError::BodyTooLarge)
        ));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn metadata_rejects_unadmitted_addresses_ports_scopes_and_resources() {
        let client = HttpClients::new(false, false).unwrap().cast_local;
        let path = "/setup/eureka_info?params=multizone";
        for address in [
            "127.0.0.1:8008",
            "0.0.0.0:8008",
            "224.0.0.1:8008",
            "255.255.255.255:8008",
            "[::1]:8008",
            "[fe80::1]:8008",
            "[::]:8008",
            "[ff02::1]:8008",
            "[::ffff:192.168.1.2]:8008",
            "192.168.1.2:80",
            "192.168.1.2:443",
        ] {
            assert!(matches!(
                client
                    .get_metadata(address.parse().unwrap(), path, false)
                    .await,
                Err(HttpError::InvalidCastAddress)
            ));
        }
        for path in [
            "/",
            "//example.test/",
            "/setup/eureka_info?params=private",
            "/setup/eureka_info?params=multizone#fragment",
        ] {
            assert!(matches!(
                client
                    .get_metadata("192.168.1.2:8008".parse().unwrap(), path, false)
                    .await,
                Err(HttpError::InvalidEndpoint)
            ));
        }
    }
}
