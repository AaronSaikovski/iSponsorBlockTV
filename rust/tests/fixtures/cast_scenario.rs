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
pub fn acceptor() -> tokio_native_tls::TlsAcceptor {
    let identity = native_tls::Identity::from_pkcs8(CERT.as_bytes(), KEY.as_bytes()).unwrap();
    tokio_native_tls::TlsAcceptor::from(native_tls::TlsAcceptor::new(identity).unwrap())
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

// Deliberately public synthetic identity; generated solely for these loopback tests.
const CERT: &str = r#"-----BEGIN CERTIFICATE-----
MIIC/jCCAeagAwIBAgIULHLZms9U4ZMxRNwPbXdO5wYNcgcwDQYJKoZIhvcNAQEL
BQAwHzEdMBsGA1UEAwwUY2FzdC1maXh0dXJlLmludmFsaWQwHhcNMjYxMTA4MDAx
MTQ2WhcNMzYxMTA1MDAxMTQ2WjAfMR0wGwYDVQQDDBRjYXN0LWZpeHR1cmUuaW52
YWxpZDCCASIwDQYJKoZIhvcNAQEBBQADggEPADCCAQoCggEBAL0cTZlVoFuF3iUP
Eg3Gd8YRITOzX/NV/K61Kp8Nx+39iI42x4gOM9zdkpFORCM4jL2M0tDGW373eC+A
nfAsTK+Be7Q+r0d7FYCp2gwK2UziN+CqABrMO+hvwTdV10RTYqqJCT6LMfDw7TpP
jBXeRo1/WOtccikiGWp7EeHLx1PEZP8+Yd3loJtSTi9CM/gWqxWQWadJhJEthBoq
KIvx5MYmAxafAKrQmpPmF4vX38zNU/4xpwmwOPPLl8R3XH1FhRMrVdHDMXd9fEv4
V9k31x5/zlqq9a0nioZUcAFPD6Z8526dFHlY0eUBS5SxdmBE650D/Q3EYQhAmrpM
Snfbo8kCAwEAAaMyMDAwHQYDVR0OBBYEFH94j9qByb5SQJdVRWjkcTvE/KIQMA8G
A1UdEwEB/wQFMAMBAf8wDQYJKoZIhvcNAQELBQADggEBAKDuzoiEPa41+o41fd54
fZ/2rvRgjeUlAJHTH+0RcHeapr6U4R1/CsFdPOZfuNMbT/qkkhFWdGVsjRfHTaPR
mIodzd+RrvyF3Mp5o/P6gZCfUr3OUpbn5LGKl4In0emm+eHZk5nMvDI3jAu0v8gE
Kg8STWCGofQGlwz+MaRrwsMRO4S+ij8dAilBl3RKdsGjoS089PpYqG6XArWHJXfh
pZ6ELwrI4pnYkTYfqLN1018W3DbmiAoaKinwhWQAoAf42HxPgSk1YdE9g99Bawin
FVUcZzxe+gwzZI38w3lkkOxovPeKwabd4cCzZLVCPEcvGmpi+EwWm6ugbUsqWZva
0dU=
-----END CERTIFICATE-----
"#;
const KEY: &str = r#"-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC9HE2ZVaBbhd4l
DxINxnfGESEzs1/zVfyutSqfDcft/YiONseIDjPc3ZKRTkQjOIy9jNLQxlt+93gv
gJ3wLEyvgXu0Pq9HexWAqdoMCtlM4jfgqgAazDvob8E3VddEU2KqiQk+izHw8O06
T4wV3kaNf1jrXHIpIhlqexHhy8dTxGT/PmHd5aCbUk4vQjP4FqsVkFmnSYSRLYQa
KiiL8eTGJgMWnwCq0JqT5heL19/MzVP+MacJsDjzy5fEd1x9RYUTK1XRwzF3fXxL
+FfZN9cef85aqvWtJ4qGVHABTw+mfOdunRR5WNHlAUuUsXZgROudA/0NxGEIQJq6
TEp326PJAgMBAAECggEABoCSgPkPG1KAI79PyDo4nE5yuS272PiuCF5JjcA6LXpM
D5K929VcLmYOb/FUWa8sNKjCx2OomfYdk2mNVKibRuYReC43FeK7J6o/aW5WkCSP
lgmWWRWIYkq0ZqgMj6h70iAF2bkMwatxBrIdP5O90ofsSWRrdvNZkx4R7HT2oLBH
BLOA240YKuJcuyarao2ekCObcE4vZKT95yW0P8CDmur2ZbcI3R1GPVorKcz9QCq4
IO5xqiDUM+hh0FAXzZ2Blvj5O0x14TlHBBd4mccDpuZV9vo3OPOSoBj49ifyGJPU
b0RN3jR0Yv+XAgcZSCb9RuPhWbopnLASTz7vnuB8wQKBgQDdDUmKLcUoVfzGgWnn
7Z6VDPweUNuptbxUK5xRmu/9Mjvo4jklqyo5ND8pEJ9h7Ols7R0JZygeLQzyzGB4
m1sMHE3mZD2/bWxEuyBj7eGX+r85///+B+22yt3kllQAbYM1GJq6+0n3ntgZC/l/
8T0Xs426jIKCZGV0GbJ9fgbniQKBgQDbAj4OSMrMmSxHBVHVx1n7KhaLeBmB3Xxm
jAe1oY1CoKxkV3tcZQsCroUsmYITNToceTUnQxLlK1zf0u+rEcCHMjBx4c4TvQ1Q
Hq8Pik2bNshra4D7Hq5kyh8aUkR2uv5EmC/OPLYZxY9fT3ZrKkgRC+Geq762y4p9
bg9p5zOKQQKBgQDNZshbxnX6KhRs29nt8eW2eE9ZWCcVBlMOH0vspDy2GFSE7JjP
gMIGT58U6GZBVyP73ERr9VfY2mG6xuApkapDkZVtLN4Xn3TIVN0fHe7PCudf3Joh
kXp4rP8Mpgc+MoSF9OvqLRApMId6lp7rHN14xZY/rsqioOmIvUwWweFmiQKBgHIp
PxPPGfeAuLXkrf3TxrHIBKx61ED/pUMHJ1WkerAKdRJxD5rgCNdDBaRz+HvuuPXy
sKRgsE7JRcZefBQeUs+md95FKFjdqTzkYbimDUIlUl+8UCWWnq1nNsNPsuR5IOW8
tt64guv4MX4rO70s8xf/q4I8YFOE0hID9wDrRdCBAoGAXG/zmaTuNJDhr/b8Z/5Q
vuvLhNoUKAweFTrwbzx4W/8W5SWOQbmUEn+xtt2Pd+RpYloA5VLArMQZDfBhNBGE
GThjr77xex34YenxNUMh8BWMUV3tPwsX2g+1HrAhzLesIOfzYha7EDdCFNl1WO8j
OXC/WdquyB4IwRAFbU/2b6k=
-----END PRIVATE KEY-----
"#;
