use isponsorblocktv::{
    api::{ApiClient, ApiOptions},
    discovery::{Discovery, DiscoveryEvent, DiscoveryMode},
    http::{Endpoints, HttpClients},
};
use std::{net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, UdpSocket},
};

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}/", listener.local_addr()?);
    let udp = UdpSocket::bind("127.0.0.1:0").await?;
    let SocketAddr::V4(target) = udp.local_addr()? else {
        unreachable!()
    };
    let endpoints = Endpoints {
        lounge: base.parse()?,
        youtube: base.parse()?,
        sponsorblock: base.parse()?,
    };
    let discovery = Discovery::loopback(
        ApiClient::new(ApiOptions::default(), endpoints)?,
        HttpClients::new(false, false)?,
        target,
    );
    let server_base = base.clone();
    let server = tokio::spawn(async move {
        for (path, headers, body) in [
            (
                "/description",
                format!("Application-URL: {server_base}apps/\r\n"),
                "<root><device><friendlyName>Native DIAL fixture</friendlyName></device></root>",
            ),
            (
                "/apps/YouTube",
                String::new(),
                "<service><additionalData><screenId>native-dial-screen</screenId></additionalData></service>",
            ),
        ] {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let byte = socket.read_u8().await.unwrap();
                request.push(byte);
                assert!(request.len() < 16384);
                if request.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            assert!(
                String::from_utf8(request)
                    .unwrap()
                    .starts_with(&format!("GET {path} HTTP/1.1\r\n"))
            );
            let reply = format!(
                "HTTP/1.1 200 OK\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(reply.as_bytes()).await.unwrap();
        }
    });
    let mut session = discovery.start(DiscoveryMode::PassiveContinuous);
    let mut buffer = [0; 4096];
    let (size, peer) =
        tokio::time::timeout(Duration::from_secs(3), udp.recv_from(&mut buffer)).await??;
    assert_eq!(&buffer[..size], b"M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 3\r\nST: urn:dial-multiscreen-org:service:dial:1\r\n\r\n");
    udp.send_to(format!("HTTP/1.1 200 OK\r\nLOCATION: {base}description\r\nST: urn:dial-multiscreen-org:service:dial:1\r\nUSN: uuid:native-fixture::urn:dial-multiscreen-org:service:dial:1\r\n\r\n").as_bytes(), peer).await?;
    let found = tokio::time::timeout(Duration::from_secs(3), session.next()).await?;
    let Some(DiscoveryEvent::Device(device)) = found else {
        panic!("expected native DIAL result: {found:?}")
    };
    assert_eq!(device.screen_id, "native-dial-screen");
    assert_eq!(device.name, "Native DIAL fixture");
    session.stop().await;
    server.await?;
    Ok(())
}
