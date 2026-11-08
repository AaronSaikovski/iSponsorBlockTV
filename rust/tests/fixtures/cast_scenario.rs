#[path = "cast_identity.rs"]
mod identity;
pub use identity::acceptor;

use isponsorblocktv::discovery::cast_wire::{
    self, CONNECTION, CastMessage, FrameDecoder, MAX_FRAME, MDX, RECEIVER,
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpListener,
    time::{Duration, timeout},
};
use tokio_util::sync::CancellationToken;

pub async fn receive<S: AsyncRead + Unpin>(stream: &mut S) -> CastMessage {
    let mut header = [0u8; 4];
    timeout(Duration::from_secs(5), stream.read_exact(&mut header))
        .await
        .unwrap()
        .unwrap();
    let size = u32::from_be_bytes(header) as usize;
    assert!(size <= MAX_FRAME);
    let mut payload = vec![0; size];
    stream.read_exact(&mut payload).await.unwrap();
    let mut decoder = FrameDecoder::default();
    decoder.push(&header).unwrap();
    decoder.push(&payload).unwrap();
    decoder.next_frame().unwrap().unwrap()
}
pub async fn send<S: AsyncWrite + Unpin>(
    stream: &mut S,
    source: &str,
    namespace: &str,
    payload: Value,
) {
    let frame = CastMessage::json(source, "sender-0", namespace, payload)
        .framed()
        .unwrap();
    // Native TLS records deliberately fragment the Cast length and payload.
    for chunk in frame.chunks(3) {
        stream.write_all(chunk).await.unwrap();
    }
}
pub async fn handshake<S: AsyncRead + Unpin>(stream: &mut S) {
    let connect = receive(stream).await;
    assert_eq!(connect.namespace, CONNECTION);
    assert_eq!(connect.destination_id, "receiver-0");
    assert_eq!(connect.payload().unwrap()["type"], "CONNECT");
    let status = receive(stream).await;
    assert_eq!(status.namespace, RECEIVER);
    assert_eq!(status.destination_id, "receiver-0");
    assert_eq!(
        status.payload().unwrap(),
        json!({"type":"GET_STATUS","requestId":1})
    );
}
pub fn status(transport: &str) -> Value {
    json!({"type":"RECEIVER_STATUS","status":{"applications":[{"appId":"233637DE","sessionId":"not-the-transport","transportId":transport,"namespaces":[{"name":MDX}]}]}})
}
pub async fn app_request<S: AsyncRead + Unpin>(stream: &mut S, transport: &str) -> u64 {
    let connect = receive(stream).await;
    assert_eq!(connect.namespace, CONNECTION);
    assert_eq!(connect.destination_id, transport);
    assert_eq!(connect.payload().unwrap()["type"], "CONNECT");
    let request = receive(stream).await;
    assert_eq!(request.destination_id, transport);
    assert_eq!(request.namespace, MDX);
    assert_eq!(request.payload().unwrap()["type"], "getMdxSessionStatus");
    request.payload().unwrap()["requestId"].as_u64().unwrap()
}

pub async fn run() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut stream = acceptor().accept(tcp).await.unwrap();
        handshake(&mut stream).await;
        send(&mut stream, "receiver-0", RECEIVER, status("existing")).await;
        let id = app_request(&mut stream, "existing").await;
        send(
            &mut stream,
            "existing",
            MDX,
            json!({"type":"mdxSessionStatus","requestId":id,"data":{"screenId":"passive-screen"}}),
        )
        .await;
        // Every preceding request is asserted above; no LAUNCH or other request may follow.
        let mut byte = [0];
        assert_eq!(
            timeout(Duration::from_secs(5), stream.read(&mut byte))
                .await
                .unwrap()
                .unwrap_or(0),
            0
        );
    });
    assert_eq!(
        cast_wire::probe(address, false, CancellationToken::new())
            .await
            .unwrap(),
        Some("passive-screen".into())
    );
    server.await.unwrap();
}
