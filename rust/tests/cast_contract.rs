use isponsorblocktv::discovery::{
    cast::{Subnet, device_metadata, group_endpoints, polling_allowed},
    cast_wire::{
        self, CONNECTION, CastError, CastMessage, CastTransport, FrameDecoder, HEARTBEAT,
        MAX_FRAME, MDX, RECEIVER,
    },
};
use serde_json::json;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    time::{Duration, timeout},
};
use tokio_util::sync::CancellationToken;

#[path = "fixtures/cast_scenario.rs"]
mod cast_fixture;
use cast_fixture::{acceptor, app_request, handshake, receive, send, status};

#[tokio::test]
async fn tls_active_launch_retry_heartbeat_and_transport_replacement() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut stream = acceptor().accept(tcp).await.unwrap();
        handshake(&mut stream).await;
        send(
            &mut stream,
            "receiver-0",
            RECEIVER,
            json!({"type":"RECEIVER_STATUS","requestId":1,"status":{}}),
        )
        .await;
        let launch = receive(&mut stream).await.payload().unwrap();
        assert_eq!(
            launch,
            json!({"type":"LAUNCH","appId":"233637DE","requestId":2})
        );
        send(
            &mut stream,
            "receiver-0",
            RECEIVER,
            json!({"type":"LAUNCH_ERROR","reason":"CANCELLED","requestId":2}),
        )
        .await;
        assert_eq!(
            receive(&mut stream).await.payload().unwrap()["requestId"],
            3
        );
        send(&mut stream, "receiver-0", HEARTBEAT, json!({"type":"PING"})).await;
        let pong = receive(&mut stream).await;
        assert_eq!(pong.namespace, HEARTBEAT);
        assert_eq!(pong.payload().unwrap()["type"], "PONG");
        send(&mut stream, "receiver-0", RECEIVER, status("transport-old")).await;
        let old = app_request(&mut stream, "transport-old").await;
        send(&mut stream, "receiver-0", RECEIVER, status("transport-new")).await;
        let new = app_request(&mut stream, "transport-new").await;
        send(
            &mut stream,
            "transport-old",
            MDX,
            json!({"type":"mdxSessionStatus","requestId":old,"data":{"screenId":"stale"}}),
        )
        .await;
        send(
            &mut stream,
            "transport-new",
            MDX,
            json!({"type":"mdxSessionStatus","requestId":new+1,"data":{"screenId":"uncorrelated"}}),
        )
        .await;
        send(
            &mut stream,
            "transport-new",
            MDX,
            json!({"type":"mdxSessionStatus","requestId":new,"data":{"screenId":"current-screen"}}),
        )
        .await;
    });
    assert_eq!(
        cast_wire::probe(
            &CastTransport::default(),
            address,
            true,
            CancellationToken::new(),
        )
        .await
        .unwrap(),
        Some("current-screen".into())
    );
    server.await.unwrap();
}

#[tokio::test]
async fn tls_passive_never_launches_absent_application() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut stream = acceptor().accept(tcp).await.unwrap();
        handshake(&mut stream).await;
        send(
            &mut stream,
            "receiver-0",
            RECEIVER,
            json!({"type":"RECEIVER_STATUS","requestId":1,"status":{}}),
        )
        .await;
        let mut byte = [0];
        assert_eq!(stream.read(&mut byte).await.unwrap_or(0), 0);
    });
    assert_eq!(
        cast_wire::probe(
            &CastTransport::default(),
            address,
            false,
            CancellationToken::new(),
        )
        .await
        .unwrap(),
        None
    );
    server.await.unwrap();
}

#[tokio::test]
async fn tls_passive_queries_existing_mdx() {
    cast_fixture::run().await;
}

#[tokio::test]
async fn cancelled_launch_retries_exactly_once() {
    let (mut client, mut server) = tokio::io::duplex(8192);
    let fixture = tokio::spawn(async move {
        handshake(&mut server).await;
        send(
            &mut server,
            "receiver-0",
            RECEIVER,
            json!({"type":"RECEIVER_STATUS","status":{}}),
        )
        .await;
        for expected in [2, 3] {
            let request = receive(&mut server).await.payload().unwrap();
            assert_eq!(request["requestId"], expected);
            send(
                &mut server,
                "receiver-0",
                RECEIVER,
                json!({"type":"LAUNCH_ERROR","requestId":expected,"reason":"CANCELLED"}),
            )
            .await;
        }
        let mut byte = [0];
        assert_eq!(server.read(&mut byte).await.unwrap(), 0);
    });
    assert!(matches!(
        cast_wire::negotiate(&mut client, true, CancellationToken::new()).await,
        Err(CastError::Launch)
    ));
    drop(client);
    fixture.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn heartbeat_timeout_and_cancellation_release_stream() {
    let (mut client, mut server) = tokio::io::duplex(8192);
    let token = CancellationToken::new();
    let worker_token = token.clone();
    let worker =
        tokio::spawn(async move { cast_wire::negotiate(&mut client, true, worker_token).await });
    handshake(&mut server).await;
    tokio::time::advance(Duration::from_secs(10)).await;
    assert_eq!(
        receive(&mut server).await.payload().unwrap()["type"],
        "PING"
    );
    tokio::time::advance(Duration::from_secs(10)).await;
    assert!(matches!(worker.await.unwrap(), Err(CastError::Timeout)));
    let (mut client, mut server) = tokio::io::duplex(8192);
    let worker_token = token.clone();
    let worker =
        tokio::spawn(async move { cast_wire::negotiate(&mut client, true, worker_token).await });
    handshake(&mut server).await;
    token.cancel();
    assert!(matches!(worker.await.unwrap(), Err(CastError::Cancelled)));
    let mut byte = [0];
    assert_eq!(server.read(&mut byte).await.unwrap(), 0);
}

#[test]
fn framing_all_byte_splits_coalescing_and_limits() {
    let message = CastMessage::json(
        "receiver-0",
        "sender-0",
        HEARTBEAT,
        json!({"type":"PING","unicode":"é🦀"}),
    );
    let frame = message.framed().unwrap();
    for split in 0..=frame.len() {
        let mut decoder = FrameDecoder::default();
        decoder.push(&frame[..split]).unwrap();
        if split < frame.len() {
            assert!(decoder.next_frame().unwrap().is_none());
        }
        decoder.push(&frame[split..]).unwrap();
        assert_eq!(decoder.next_frame().unwrap().unwrap(), message);
        decoder.finish().unwrap();
    }
    let mut decoder = FrameDecoder::default();
    decoder
        .push(&[frame.clone(), frame.clone()].concat())
        .unwrap();
    assert!(decoder.next_frame().unwrap().is_some());
    assert!(decoder.next_frame().unwrap().is_some());
    decoder.finish().unwrap();
    for length in [0, MAX_FRAME as u32 + 1, u32::MAX] {
        assert!(FrameDecoder::default().push(&length.to_be_bytes()).is_err());
    }
    let mut decoder = FrameDecoder::default();
    decoder.push(&frame[..frame.len() - 1]).unwrap();
    assert!(decoder.finish().is_err());
    let mut bad = message.clone();
    bad.payload_binary = Some(vec![1]);
    assert!(bad.payload().is_err());
    bad = message.clone();
    bad.payload_utf8 = Some("[".into());
    assert!(bad.payload().is_err());
    bad = message;
    bad.protocol_version = 1;
    assert!(bad.payload().is_err());
}

#[test]
fn metadata_shapes_polling_and_group_subnet_rules() {
    let metadata = device_metadata(&json!({"name":"fallback","device_info":{"name":"literal [red] 🦀","ssdp_udn":"uuid:12345678-1234-1234-1234-123456789abc","model_name":"Nest Audio","capabilities":{"display_supported":false}}})).unwrap();
    assert!(metadata.audio && metadata.multizone);
    assert_eq!(metadata.name, "literal [red] 🦀");
    assert!(metadata.uuid.is_some());
    assert!(polling_allowed(true, "Nest Audio"));
    assert!(!polling_allowed(false, "TV"));
    assert!(!polling_allowed(true, "HK Citation"));
    assert!(!polling_allowed(true, "JBL Link"));
    let subnet = Subnet {
        address: "192.168.2.2".parse().unwrap(),
        mask: "255.255.255.0".parse().unwrap(),
        index: 7,
    };
    let groups = group_endpoints(
        &json!({"multizone":{"groups":[{"elected_leader":"self","cast_port":12345,"name":"group"},{"elected_leader":"192.168.2.9:1234"},{"elected_leader":"192.168.3.9:8009"},{"elected_leader":"attacker.example:8009"},{"elected_leader":"127.0.0.1:8009"}],"dynamic_groups":[{"elected_leader":"192.168.2.10:8008"}]}}),
        "192.168.2.5:8009".parse().unwrap(),
        &[subnet],
    );
    assert_eq!(groups.len(), 3);
    assert_eq!(groups[0].address.port(), 8009);
    assert!(!groups[0].known_port);
    assert!(
        groups
            .iter()
            .any(|g| g.address.port() == 12345 && g.known_port)
    );
    let subnet = Subnet {
        address: "fe80::1".parse().unwrap(),
        mask: "ffff:ffff:ffff:ffff::".parse().unwrap(),
        index: 7,
    };
    let groups = group_endpoints(
        &json!({"multizone":{"groups":[{"elected_leader":"[fe80::9]:8008"}]}}),
        "[fe80::2%7]:8009".parse().unwrap(),
        &[subnet],
    );
    assert_eq!(groups[0].address, "[fe80::9%7]:8009".parse().unwrap());
}

#[tokio::test]
async fn tls_handshake_cancellation_closes_native_socket() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let token = CancellationToken::new();
    let client_token = token.clone();
    let transport = CastTransport::default();
    let worker_transport = transport.clone();
    let worker = tokio::spawn(async move {
        cast_wire::probe(&worker_transport, address, true, client_token).await
    });
    let (mut server, _) = listener.accept().await.unwrap();
    token.cancel();
    assert!(matches!(worker.await.unwrap(), Err(CastError::Cancelled)));
    let mut bytes = Vec::new();
    timeout(Duration::from_secs(5), server.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    // Cancelling a socket must not poison the shared TLS policy for later probes.
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut stream = acceptor().accept(tcp).await.unwrap();
        handshake(&mut stream).await;
        send(
            &mut stream,
            "receiver-0",
            RECEIVER,
            json!({"type":"RECEIVER_STATUS","requestId":1,"status":{}}),
        )
        .await;
        let mut byte = [0];
        assert_eq!(stream.read(&mut byte).await.unwrap_or(0), 0);
    });
    assert_eq!(
        cast_wire::probe(&transport, address, false, CancellationToken::new())
            .await
            .unwrap(),
        None
    );
    server.await.unwrap();
}

#[tokio::test]
async fn eof_close_and_truncated_frames_end_negotiation() {
    for payload in [
        Vec::new(),
        vec![0, 0, 0, 10, 1],
        CastMessage::json(
            "receiver-0",
            "sender-0",
            CONNECTION,
            json!({"type":"CLOSE"}),
        )
        .framed()
        .unwrap(),
    ] {
        let (mut client, mut server) = tokio::io::duplex(8192);
        let fixture = tokio::spawn(async move {
            handshake(&mut server).await;
            server.write_all(&payload).await.unwrap();
        });
        assert!(
            cast_wire::negotiate(&mut client, false, CancellationToken::new())
                .await
                .is_err()
        );
        fixture.await.unwrap();
    }
}

#[tokio::test]
async fn mdx_screen_identity_limit_is_bytes_not_characters() {
    use isponsorblocktv::discovery::MAX_SCREEN_ID_BYTES;
    for screen_id in [
        "x".repeat(MAX_SCREEN_ID_BYTES),
        "x".repeat(MAX_SCREEN_ID_BYTES + 1),
        "é".repeat(MAX_SCREEN_ID_BYTES),
    ] {
        let expected = screen_id.clone();
        let (mut client, mut server) = tokio::io::duplex(8192);
        let peer = tokio::spawn(async move {
            handshake(&mut server).await;
            send(&mut server, "receiver-0", RECEIVER, status("fixture")).await;
            let request = app_request(&mut server, "fixture").await;
            send(
                &mut server,
                "fixture",
                MDX,
                json!({"type":"mdxSessionStatus","requestId":request,
                    "data":{"screenId":screen_id}}),
            )
            .await;
        });
        let result = timeout(
            Duration::from_secs(3),
            cast_wire::negotiate(&mut client, false, CancellationToken::new()),
        )
        .await
        .unwrap();
        if expected.len() <= MAX_SCREEN_ID_BYTES {
            assert_eq!(result.unwrap().as_deref(), Some(expected.as_str()));
        } else {
            assert!(matches!(result, Err(CastError::Identity)));
        }
        peer.await.unwrap();
    }
}
