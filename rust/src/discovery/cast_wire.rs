//! Minimal Cast v2 framing and YouTube MDX negotiation. No persistent playback connection.
// CastMessage schema derived from Chromium's cast_channel.proto:
// Copyright 2019 The Chromium Authors. Redistribution and use in source and binary
// forms, with or without modification, are permitted provided that the following
// conditions are met: Redistributions of source code must retain the above copyright
// notice, this list of conditions and the following disclaimer. Redistributions in
// binary form must reproduce the above copyright notice, this list of conditions
// and the following disclaimer in the documentation and/or other materials provided
// with the distribution. Neither the name of Google LLC nor the names of its
// contributors may be used to endorse or promote products derived from this
// software without specific prior written permission.
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND
// ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
// WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE DISCLAIMED.
// IN NO EVENT SHALL THE COPYRIGHT OWNER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT,
// INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING,
// BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
// DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
// LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE
// OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED
// OF THE POSSIBILITY OF SUCH DAMAGE.
use bytes::{Buf, BytesMut};
use prost::Message;
use serde_json::{Value, json};
use std::{net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    time::{Instant, interval_at, timeout},
};
use tokio_util::sync::CancellationToken;

pub const MAX_FRAME: usize = 1024 * 1024;
pub const CONNECTION: &str = "urn:x-cast:com.google.cast.tp.connection";
pub const HEARTBEAT: &str = "urn:x-cast:com.google.cast.tp.heartbeat";
pub const RECEIVER: &str = "urn:x-cast:com.google.cast.receiver";
pub const MDX: &str = "urn:x-cast:com.google.youtube.mdx";

#[derive(Clone, PartialEq, Message)]
pub struct CastMessage {
    #[prost(int32, required, tag = "1")]
    pub protocol_version: i32,
    #[prost(string, required, tag = "2")]
    pub source_id: String,
    #[prost(string, required, tag = "3")]
    pub destination_id: String,
    #[prost(string, required, tag = "4")]
    pub namespace: String,
    #[prost(int32, required, tag = "5")]
    pub payload_type: i32,
    #[prost(string, optional, tag = "6")]
    pub payload_utf8: Option<String>,
    #[prost(bytes = "vec", optional, tag = "7")]
    pub payload_binary: Option<Vec<u8>>,
}

#[derive(Debug, thiserror::Error)]
pub enum CastError {
    #[error("Cast transport closed or failed")]
    Transport,
    #[error("Cast TLS connection failed")]
    Tls,
    #[error("Cast probe timed out")]
    Timeout,
    #[error("Cast probe cancelled")]
    Cancelled,
    #[error("invalid or oversized Cast frame")]
    Frame,
    #[error("invalid Cast JSON payload")]
    Json,
    #[error("Cast receiver refused application launch")]
    Launch,
}

impl CastMessage {
    pub fn json(source: &str, destination: &str, namespace: &str, payload: Value) -> Self {
        Self {
            protocol_version: 0,
            source_id: source.into(),
            destination_id: destination.into(),
            namespace: namespace.into(),
            payload_type: 0,
            payload_utf8: Some(payload.to_string()),
            payload_binary: None,
        }
    }
    pub fn payload(&self) -> Result<Value, CastError> {
        if self.protocol_version != 0
            || self.payload_type != 0
            || self.payload_binary.is_some()
            || self.source_id.is_empty()
            || self.destination_id.is_empty()
            || self.namespace.is_empty()
        {
            return Err(CastError::Frame);
        }
        let value: Value =
            serde_json::from_str(self.payload_utf8.as_deref().ok_or(CastError::Frame)?)
                .map_err(|_| CastError::Json)?;
        if !value.is_object() {
            return Err(CastError::Json);
        }
        Ok(value)
    }
    pub fn framed(&self) -> Result<Vec<u8>, CastError> {
        let length = self.encoded_len();
        if length == 0 || length > MAX_FRAME {
            return Err(CastError::Frame);
        }
        let mut bytes = Vec::with_capacity(4 + length);
        bytes.extend_from_slice(&(length as u32).to_be_bytes());
        self.encode(&mut bytes).map_err(|_| CastError::Frame)?;
        Ok(bytes)
    }
}

/// Incremental state survives cancellation of reads; length is checked before
/// reserving payload memory. Consumers drain frames between pushes.
#[derive(Default)]
pub struct FrameDecoder {
    bytes: BytesMut,
}
impl FrameDecoder {
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), CastError> {
        // One bounded socket read may contain the end of a maximum-size frame
        // followed by the next frame. The frame itself still cannot exceed 1 MiB.
        if bytes.len() > MAX_FRAME + 4 + 8192 - self.bytes.len() {
            return Err(CastError::Frame);
        }
        self.bytes.extend_from_slice(bytes);
        if self.bytes.len() >= 4 {
            let length = u32::from_be_bytes(self.bytes[..4].try_into().unwrap()) as usize;
            if length == 0 || length > MAX_FRAME {
                return Err(CastError::Frame);
            }
        }
        Ok(())
    }
    pub fn next_frame(&mut self) -> Result<Option<CastMessage>, CastError> {
        if self.bytes.len() < 4 {
            return Ok(None);
        }
        let length = u32::from_be_bytes(self.bytes[..4].try_into().unwrap()) as usize;
        if length == 0 || length > MAX_FRAME {
            return Err(CastError::Frame);
        }
        if self.bytes.len() < 4 + length {
            return Ok(None);
        }
        self.bytes.advance(4);
        let frame = CastMessage::decode(self.bytes.split_to(length).freeze())
            .map_err(|_| CastError::Frame)?;
        Ok(Some(frame))
    }
    pub fn finish(&self) -> Result<(), CastError> {
        if self.bytes.is_empty() {
            Ok(())
        } else {
            Err(CastError::Frame)
        }
    }
}

async fn send<S: AsyncWrite + Unpin>(
    stream: &mut S,
    dest: &str,
    namespace: &str,
    payload: Value,
) -> Result<(), CastError> {
    let frame = CastMessage::json("sender-0", dest, namespace, payload).framed()?;
    timeout(Duration::from_secs(10), stream.write_all(&frame))
        .await
        .map_err(|_| CastError::Timeout)?
        .map_err(|_| CastError::Transport)
}

/// Pin the resolved socket address; TLS is encrypted but deliberately does not
/// authenticate receivers on the trusted LAN. Cloud certificate policy is untouched.
pub async fn probe(
    address: SocketAddr,
    active: bool,
    cancel: CancellationToken,
) -> Result<Option<String>, CastError> {
    probe_with_permission(address, active, cancel, || true).await
}

pub(crate) async fn probe_with_permission<F: FnMut() -> bool>(
    address: SocketAddr,
    active: bool,
    cancel: CancellationToken,
    permission: F,
) -> Result<Option<String>, CastError> {
    let connector = native_tls::TlsConnector::builder()
        .danger_accept_invalid_certs(true)
        .danger_accept_invalid_hostnames(true)
        .build()
        .map_err(|_| CastError::Tls)?;
    let connector = tokio_native_tls::TlsConnector::from(connector);
    let connect = async {
        let tcp = TcpStream::connect(address)
            .await
            .map_err(|_| CastError::Transport)?;
        connector
            .connect(&address.ip().to_string(), tcp)
            .await
            .map_err(|_| CastError::Tls)
    };
    let mut stream = tokio::select! {
        _ = cancel.cancelled() => return Err(CastError::Cancelled),
        result = timeout(Duration::from_secs(5), connect) => result.map_err(|_| CastError::Timeout)??,
    };
    tokio::select! {
        _ = cancel.cancelled() => Err(CastError::Cancelled),
        result = negotiate_inner(&mut stream, active, permission) => result,
    }
}

/// Shared native protocol engine used by the real TLS probe and synthetic peers.
/// The owner enforces the admission-time overall deadline.
pub async fn negotiate<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    active: bool,
    cancel: CancellationToken,
) -> Result<Option<String>, CastError> {
    tokio::select! {
        _ = cancel.cancelled() => Err(CastError::Cancelled),
        result = negotiate_inner(stream, active, || true) => result,
    }
}

async fn negotiate_inner<S: AsyncRead + AsyncWrite + Unpin, F: FnMut() -> bool>(
    stream: &mut S,
    active: bool,
    mut permission: F,
) -> Result<Option<String>, CastError> {
    send(stream, "receiver-0", CONNECTION, json!({"type":"CONNECT"})).await?;
    let mut request = 1u64;
    send(
        stream,
        "receiver-0",
        RECEIVER,
        json!({"type":"GET_STATUS", "requestId":request}),
    )
    .await?;
    let status_request = request;
    let mut launch_request = None;
    let mut launch_count = 0;
    let mut mdx_request = None;
    let mut transport: Option<String> = None;
    let mut decoder = FrameDecoder::default();
    let mut chunk = [0u8; 8192];
    let mut last_pong = Instant::now();
    let mut heartbeat = interval_at(
        Instant::now() + Duration::from_secs(10),
        Duration::from_secs(10),
    );
    loop {
        while let Some(frame) = decoder.next_frame()? {
            let payload = frame.payload()?;
            if frame.destination_id != "sender-0" && frame.destination_id != "*" {
                continue;
            }
            let kind = payload.get("type").and_then(Value::as_str).unwrap_or("");
            if frame.namespace == HEARTBEAT {
                if kind == "PING" {
                    send(stream, &frame.source_id, HEARTBEAT, json!({"type":"PONG"})).await?;
                }
                if kind == "PONG" {
                    last_pong = Instant::now();
                }
            } else if frame.namespace == CONNECTION && kind == "CLOSE" {
                return Err(CastError::Transport);
            } else if frame.namespace == RECEIVER && frame.source_id == "receiver-0" {
                let response_id = payload.get("requestId").and_then(Value::as_u64);
                if kind == "LAUNCH_ERROR" && response_id.is_some() && response_id == launch_request
                {
                    if payload.get("reason").and_then(Value::as_str) == Some("CANCELLED")
                        && launch_count == 1
                    {
                        request += 1;
                        launch_request = Some(request);
                        launch_count += 1;
                        send(
                            stream,
                            "receiver-0",
                            RECEIVER,
                            json!({"type":"LAUNCH","appId":"233637DE","requestId":request}),
                        )
                        .await?;
                    } else {
                        return Err(CastError::Launch);
                    }
                } else if kind == "RECEIVER_STATUS" {
                    if response_id.is_some()
                        && response_id != Some(status_request)
                        && response_id != launch_request
                    {
                        continue;
                    }
                    let apps = payload
                        .pointer("/status/applications")
                        .and_then(Value::as_array);
                    let next = apps
                        .and_then(|apps| {
                            apps.iter().find(|app| {
                                app.get("namespaces")
                                    .and_then(Value::as_array)
                                    .is_some_and(|ns| {
                                        ns.iter().any(|n| {
                                            n.get("name").and_then(Value::as_str) == Some(MDX)
                                        })
                                    })
                            })
                        })
                        .and_then(|app| app.get("transportId"))
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty());
                    if transport.as_deref() != next {
                        transport = next.map(str::to_owned);
                        mdx_request = None;
                    }
                    if let Some(dest) = transport.as_deref() {
                        if mdx_request.is_none() {
                            send(stream, dest, CONNECTION, json!({"type":"CONNECT"})).await?;
                            request += 1;
                            mdx_request = Some(request);
                            send(
                                stream,
                                dest,
                                MDX,
                                json!({"type":"getMdxSessionStatus","requestId":request}),
                            )
                            .await?;
                        }
                    } else if !active {
                        return Ok(None);
                    } else if launch_count == 0 {
                        if !permission() {
                            return Ok(None);
                        }
                        request += 1;
                        launch_count = 1;
                        launch_request = Some(request);
                        send(
                            stream,
                            "receiver-0",
                            RECEIVER,
                            json!({"type":"LAUNCH","appId":"233637DE","requestId":request}),
                        )
                        .await?;
                    }
                }
            } else if frame.namespace == MDX
                && transport.as_deref() == Some(frame.source_id.as_str())
                && kind == "mdxSessionStatus"
            {
                let response_id = payload.get("requestId").and_then(Value::as_u64);
                if response_id.is_some() && response_id != mdx_request {
                    continue;
                }
                if let Some(id) = payload
                    .pointer("/data/screenId")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    return Ok(Some(id.to_owned()));
                }
            }
        }
        tokio::select! {
            _ = heartbeat.tick() => {
                if last_pong.elapsed() >= Duration::from_secs(20) { return Err(CastError::Timeout); }
                send(stream, "receiver-0", HEARTBEAT, json!({"type":"PING"})).await?;
            }
            result = stream.read(&mut chunk) => {
                let count = result.map_err(|_| CastError::Transport)?;
                if count == 0 { decoder.finish()?; return Err(CastError::Transport); }
                decoder.push(&chunk[..count])?;
            }
        }
    }
}
