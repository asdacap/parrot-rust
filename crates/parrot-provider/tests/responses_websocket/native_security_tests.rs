//! Independent supplemental RFC6455 invariants. This author did not inspect native.rs.
//! These are protocol safety checks, not claims that .NET rejects every invalid frame identically.
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_provider::http_stream::ProviderHttpClient;
use parrot_provider::responses_websocket::{
    Connector, FrameKind, NativeConnector, Socket, SocketState,
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

#[derive(Clone, Default)]
struct Token(Arc<(AtomicBool, tokio::sync::Notify)>);
impl Token {
    fn cancel(&self) {
        self.0.0.store(true, Ordering::SeqCst);
        self.0.1.notify_waiters();
    }
}
impl Cancellation for Token {
    fn is_cancelled(&self) -> bool {
        self.0.0.load(Ordering::SeqCst)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            loop {
                let notified = self.0.1.notified();
                if self.is_cancelled() {
                    return;
                }
                notified.await;
            }
        })
    }
}
async fn upgrade(peer: TcpStream, suffix: &[u8]) -> TcpStream {
    upgrade_mode(peer, suffix, "valid").await
}
async fn upgrade_mode(mut peer: TcpStream, suffix: &[u8], mode: &str) -> TcpStream {
    let mut request = Vec::new();
    while !request.ends_with(b"\r\n\r\n") {
        request.push(peer.read_u8().await.unwrap());
        assert!(request.len() < 16384);
    }
    let request = String::from_utf8(request).unwrap();
    let key = request
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(name, _)| name.eq_ignore_ascii_case("sec-websocket-key"))
                .map(|(_, v)| v.trim())
        })
        .unwrap();
    let accept = if mode == "invalid-accept" {
        "invalid".to_owned()
    } else {
        tungstenite::handshake::derive_accept_key(key.as_bytes())
    };
    let status = match mode {
        "status200" => "200 OK",
        "status404" => "404 Not Found",
        _ => "101 Switching Protocols",
    };
    let connection = if mode == "bad-connection" {
        "close"
    } else {
        "Upgrade"
    };
    let upgrade = if mode == "missing-upgrade" {
        ""
    } else {
        "Upgrade: websocket\r\n"
    };
    let mut response=format!("HTTP/1.1 {status}\r\nConnection: {connection}\r\n{upgrade}Sec-WebSocket-Accept: {accept}\r\nx-native-test: retained\r\nContent-Length: 0\r\n\r\n").into_bytes();
    response.extend_from_slice(suffix);
    peer.write_all(&response).await.unwrap();
    peer
}
async fn pair(bytes: Vec<u8>) -> (Box<dyn Socket>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (peer, _) = listener.accept().await.unwrap();
        let _peer = upgrade(peer, &bytes).await;
        std::future::pending::<()>().await;
    });
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let connector = NativeConnector::new(ProviderHttpClient::standard(client));
    let connection = connector
        .connect(
            &format!("ws://{address}/responses"),
            &vec![],
            Some(Duration::from_secs(2)),
            &Token::default(),
        )
        .await
        .unwrap();
    (connection.socket, server)
}
fn text(bytes: &[u8], final_frame: bool, opcode: u8) -> Vec<u8> {
    assert!(bytes.len() < 126);
    let mut frame = vec![
        (if final_frame { 0x80 } else { 0 }) | opcode,
        bytes.len() as u8,
    ];
    frame.extend_from_slice(bytes);
    frame
}
async fn collect(socket: &mut dyn Socket) -> Vec<u8> {
    let mut all = vec![];
    loop {
        let mut buffer = [0; 3];
        let frame = tokio::time::timeout(
            Duration::from_secs(2),
            socket.receive(&mut buffer, &Token::default()),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(frame.kind, FrameKind::Text);
        all.extend_from_slice(&buffer[..frame.count]);
        if frame.end_of_message {
            return all;
        }
    }
}

#[tokio::test]
async fn independent_fragmented_utf8_with_interleaved_control_and_empty_frames() {
    let mut wire = text(&[], false, 1);
    wire.extend(text(&[0xc3], false, 0));
    wire.extend(text(b"ping", true, 9));
    wire.extend(text(b"ignored", true, 10));
    wire.extend(text(&[], false, 0));
    wire.extend(text(&[0xa9], true, 0));
    let (mut socket, server) = pair(wire).await;
    assert_eq!(collect(socket.as_mut()).await, "é".as_bytes());
    server.abort();
}

#[tokio::test]
async fn independent_invalid_rfc_frames_are_rejected() {
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("rsv1", vec![0xc1, 0]),
        ("rsv2", vec![0xa1, 0]),
        ("reserved-opcode", vec![0x83, 0]),
        ("masked-server", vec![0x81, 0x80, 1, 2, 3, 4]),
        ("orphan-continuation", vec![0x80, 0]),
        ("second-data-during-fragment", vec![0x01, 0, 0x81, 0]),
        ("fragmented-ping", vec![0x09, 0]),
        ("oversized-control", vec![0x89, 126, 0, 126]),
        ("nonminimal-16", vec![0x81, 126, 0, 1, b'a']),
        (
            "nonminimal-64",
            vec![0x81, 127, 0, 0, 0, 0, 0, 0, 0, 1, b'a'],
        ),
        (
            "high-bit-length",
            vec![0x81, 127, 0x80, 0, 0, 0, 0, 0, 0, 0],
        ),
        ("close-one-byte", vec![0x88, 1, 0]),
        ("close-reserved-1005", vec![0x88, 2, 3, 237]),
        ("close-invalid-utf8", vec![0x88, 3, 3, 232, 0xff]),
        ("text-invalid-utf8", vec![0x81, 1, 0xff]),
        ("text-truncated-utf8", vec![0x81, 1, 0xc3]),
        ("fragment-final-invalid-utf8", vec![0x01, 1, 0xc3, 0x80, 0]),
    ];
    for (name, bytes) in cases {
        let (mut socket, server) = pair(bytes).await;
        let result = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let mut buffer = [0; 8];
                if let Err(error) = socket.receive(&mut buffer, &Token::default()).await {
                    break Err::<(), _>(error);
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{name}: parser waited instead of rejecting invalid frame"));
        assert!(result.is_err(), "{name}");
        server.abort();
    }
}

#[tokio::test]
async fn independent_close_status_reason_and_payload_accounting() {
    let (mut socket, server) = pair(vec![0x88, 5, 3, 232, b'b', b'y', b'e']).await;
    let mut buffer = [0; 16];
    let frame = socket
        .receive(&mut buffer, &Token::default())
        .await
        .unwrap();
    assert_eq!(frame.kind, FrameKind::Close);
    assert_eq!(frame.count, 0);
    assert_eq!(frame.close_status, Some(1000));
    assert_eq!(frame.close_description.as_deref(), Some("bye"));
    server.abort();
}

#[tokio::test]
async fn independent_cancel_after_payload_prefix_aborts_native_socket() {
    let (mut socket, server) = pair(vec![0x01, 3, b'a', b'b', b'c']).await;
    let token = Token::default();
    let mut buffer = [0; 8192];
    let prefix = tokio::time::timeout(Duration::from_secs(2), socket.receive(&mut buffer, &token))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&buffer[..prefix.count], b"abc");
    assert!(!prefix.end_of_message);
    token.cancel();
    assert!(socket.receive(&mut buffer, &token).await.is_err());
    assert_eq!(socket.state(), SocketState::Aborted);
    server.abort();
}

#[tokio::test]
async fn independent_client_frames_are_masked_with_continuation_and_fresh_keys() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (peer, _) = listener.accept().await.unwrap();
        let mut peer = upgrade(peer, &[]).await;
        let mut masks = vec![];
        for (opcode, final_frame, expected) in [
            (1, false, b"hello".as_slice()),
            (0, true, b"world".as_slice()),
        ] {
            let first = peer.read_u8().await.unwrap();
            let second = peer.read_u8().await.unwrap();
            assert_eq!(first, opcode | if final_frame { 0x80 } else { 0 });
            assert_ne!(second & 0x80, 0);
            assert_eq!((second & 0x7f) as usize, expected.len());
            let mut mask = [0; 4];
            peer.read_exact(&mut mask).await.unwrap();
            masks.push(mask);
            let mut body = vec![0; expected.len()];
            peer.read_exact(&mut body).await.unwrap();
            for (i, b) in body.iter_mut().enumerate() {
                *b ^= mask[i % 4];
            }
            assert_eq!(body, expected);
        }
        assert_ne!(
            masks[0], masks[1],
            "independent fresh random masks collided"
        );
    });
    let connector = NativeConnector::new(ProviderHttpClient::standard(
        reqwest::Client::builder().no_proxy().build().unwrap(),
    ));
    let mut connected = connector
        .connect(
            &format!("ws://{address}/"),
            &vec![],
            Some(Duration::from_secs(2)),
            &Token::default(),
        )
        .await
        .unwrap();
    connected
        .socket
        .send(b"hello", false, &Token::default())
        .await
        .unwrap();
    connected
        .socket
        .send(b"world", true, &Token::default())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
}

struct CancelAfterRead {
    inner: Box<dyn Socket>,
    token: Token,
}
impl Socket for CancelAfterRead {
    fn state(&self) -> SocketState {
        self.inner.state()
    }
    fn abort(&mut self) {
        self.inner.abort();
    }
    fn dispose(&mut self) {
        self.inner.dispose();
    }
    fn send<'a>(
        &'a mut self,
        bytes: &'a [u8],
        end: bool,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), parrot_provider::responses_websocket::WebSocketError>> {
        self.inner.send(bytes, end, cancellation)
    }
    fn receive<'a>(
        &'a mut self,
        bytes: &'a mut [u8],
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<
        'a,
        Result<
            parrot_provider::responses_websocket::ReceivedFrame,
            parrot_provider::responses_websocket::WebSocketError,
        >,
    > {
        Box::pin(async move {
            let result = self.inner.receive(bytes, cancellation).await?;
            if result.count > 0 {
                self.token.cancel();
            }
            Ok(result)
        })
    }
}

// Expectations come only from the immutable actual .NET native captures.
#[tokio::test]
async fn independent_native_source_receive_boundaries_match_observations() {
    use parrot_provider::responses_websocket::{ResponsesWebSocket, WebSocketError};
    let first: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/native/expected.json")).unwrap();
    let probes: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/native/probes/expected.json")).unwrap();
    let inputs: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/native/inputs.json")).unwrap();
    let more: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/native/probes/inputs.json")).unwrap();
    let all_inputs = inputs["cases"]
        .as_array()
        .unwrap()
        .iter()
        .chain(more["cases"].as_array().unwrap());
    let all_expected = first["cases"]
        .as_array()
        .unwrap()
        .iter()
        .chain(probes["cases"].as_array().unwrap());
    for (input, expected) in all_inputs.zip(all_expected) {
        let mode = input["mode"].as_str().unwrap().to_owned();
        if matches!(mode.as_str(), "status404" | "invalid-accept") {
            continue;
        }
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_mode = mode.clone();
        let server = tokio::spawn(async move {
            let (peer, _) = listener.accept().await.unwrap();
            let body = br#"{"type":"response.completed","response":{"output":[]}}"#;
            let frame = text(body, true, 1);
            let mut wire = vec![];
            match server_mode.as_str() {
                "prefix-cancel" | "prefix-idle" | "delayed" => wire.extend_from_slice(&frame[..9]),
                "coalesced" => wire.extend_from_slice(&frame),
                "fragment-cancel" => wire.extend(text(&body[..7], false, 1)),
                "fragment-delayed" => wire.extend(text(&body[..10], false, 1)),
                m if m.starts_with("large-") => {
                    let n = if m.contains("8191") {
                        8191
                    } else if m.contains("8193") {
                        8193
                    } else {
                        8192
                    };
                    wire.extend_from_slice(&[0x81, 126, 0x4e, 0x20]);
                    wire.resize(4 + n, b' ');
                }
                "control" => {}
                _ => panic!("unknown native source input"),
            }
            let mut peer = upgrade(peer, &wire).await;
            if server_mode == "delayed" {
                for part in frame[9..].chunks(10) {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    if peer.write_all(part).await.is_err() {
                        return;
                    }
                }
            } else if server_mode == "fragment-delayed" {
                for (index, part) in body[10..].chunks(10).enumerate() {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    let end = 10 + (index + 1) * 10 >= body.len();
                    if peer.write_all(&text(part, end, 0)).await.is_err() {
                        return;
                    }
                }
            } else if server_mode == "control" {
                loop {
                    if peer.write_all(&[0x89, 1, b'a']).await.is_err() {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(75)).await;
                }
            }
            std::future::pending::<()>().await;
        });
        let token = Token::default();
        let connector = NativeConnector::new(ProviderHttpClient::standard(
            reqwest::Client::builder().no_proxy().build().unwrap(),
        ));
        let connected = connector
            .connect(
                &format!("ws://{address}/responses"),
                &vec![],
                Some(Duration::from_secs(2)),
                &token,
            )
            .await
            .unwrap();
        let cancel_on_read = matches!(
            mode.as_str(),
            "prefix-cancel" | "large-8192-cancel" | "fragment-cancel"
        );
        let socket = if cancel_on_read {
            Box::new(CancelAfterRead {
                inner: connected.socket,
                token: token.clone(),
            }) as Box<dyn Socket>
        } else {
            connected.socket
        };
        let mut connection = ResponsesWebSocket::new(
            socket,
            connected.response_headers,
            Some(Duration::from_millis(500)),
            1024,
        );
        let mut events = 0;
        let error = {
            let mut exchange = connection.send(b"{}".to_vec(), "native-oracle").unwrap();
            loop {
                match tokio::time::timeout(Duration::from_secs(3), exchange.next(&token))
                    .await
                    .unwrap()
                {
                    Ok(Some(_)) => events += 1,
                    Ok(None) => break None,
                    Err(e) => break Some(e),
                }
            }
        };
        assert_eq!(
            connection.stats().unwrap().request_bytes,
            expected["request_bytes"].as_u64().unwrap(),
            "{mode}"
        );
        assert_eq!(
            connection.stats().unwrap().response_bytes,
            expected["response_bytes"].as_u64(),
            "{mode}"
        );
        assert_eq!(events, expected["events"].as_u64().unwrap_or(0), "{mode}");
        if expected["failure"].is_null() {
            assert!(error.is_none(), "{mode}: {error:?}");
        } else if expected["failure"]["caller_cancelled"] == true {
            assert!(
                matches!(error, Some(WebSocketError::Cancelled)),
                "{mode}: {error:?}"
            );
        } else {
            assert!(
                matches!(error,Some(WebSocketError::Wire(ref m)) if m=="responses: idle timeout waiting for websocket"),
                "{mode}: {error:?}"
            );
        }
        server.abort();
    }
}

/// Closes the six source-native observations not selected by the receive-boundary test.
/// Compares exception category/status and source-owned close diagnostics, not CLR wording.
#[tokio::test]
async fn independent_native_source_handshake_and_empty_close_match_observations() {
    use parrot_provider::responses_websocket::{ResponsesWebSocket, WebSocketError};
    let original_inputs: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/native/inputs.json")).unwrap();
    let original: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/native/expected.json")).unwrap();
    let extra_inputs: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/native/handshake/inputs.json")).unwrap();
    let extra: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/native/handshake/expected.json")).unwrap();
    let original_cases = original_inputs["cases"]
        .as_array()
        .unwrap()
        .iter()
        .zip(original["cases"].as_array().unwrap())
        .filter(|(input, _)| {
            matches!(input["mode"].as_str(), Some("status404" | "invalid-accept"))
        });
    let extra_cases = extra_inputs["cases"]
        .as_array()
        .unwrap()
        .iter()
        .zip(extra["cases"].as_array().unwrap());
    let mut compared = Vec::new();
    for (input, expected) in original_cases.chain(extra_cases) {
        assert_eq!(input["case_id"], expected["case_id"]);
        let mode = input["mode"].as_str().unwrap().to_owned();
        let id = input["case_id"].as_str().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_mode = mode.clone();
        let server = tokio::spawn(async move {
            let (peer, _) = listener.accept().await.unwrap();
            let suffix: &[u8] = if server_mode == "empty-close" {
                &[0x88, 0]
            } else {
                &[]
            };
            let _peer = upgrade_mode(peer, suffix, &server_mode).await;
            std::future::pending::<()>().await;
        });
        let token = Token::default();
        let connector = NativeConnector::new(ProviderHttpClient::standard(
            reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
        ));
        let result = connector
            .connect(
                &format!("ws://{address}/responses"),
                &vec![],
                Some(Duration::from_secs(2)),
                &token,
            )
            .await;
        if let Some(status) = expected["failure"]["upgrade_status"].as_u64() {
            assert_eq!(
                expected["failure"]["type"],
                "ResponsesWebSocketUpgradeException"
            );
            match result {
                Err(WebSocketError::Upgrade { status: actual, .. }) => {
                    assert_eq!(u64::from(actual), status, "{id}")
                }
                Err(error) => panic!("{id}: expected Upgrade({status}), got {error:?}"),
                Ok(_) => panic!("{id}: invalid handshake unexpectedly accepted"),
            }
            assert!(expected["request_bytes"].is_null(), "{id}");
            assert!(expected["response_bytes"].is_null(), "{id}");
        } else {
            assert_eq!(mode, "empty-close");
            assert_eq!(expected["failure"]["type"], "WireProtocolException");
            let connected = result.unwrap_or_else(|error| panic!("{id}: {error:?}"));
            let header = connected
                .response_headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("x-native-test"))
                .map(|(_, value)| value.as_str());
            assert_eq!(header, expected["upgrade_header"].as_str(), "{id}");
            let mut connection = ResponsesWebSocket::new(
                connected.socket,
                connected.response_headers,
                Some(Duration::from_millis(500)),
                1024,
            );
            let error = {
                let mut exchange = connection.send(b"{}".to_vec(), "native-oracle").unwrap();
                tokio::time::timeout(Duration::from_secs(2), exchange.next(&token))
                    .await
                    .unwrap()
                    .unwrap_err()
            };
            match error {
                WebSocketError::Wire(message) => assert_eq!(
                    message,
                    expected["failure"]["message"].as_str().unwrap(),
                    "{id}: source-owned close status/reason diagnostic"
                ),
                error => panic!("{id}: expected Wire, got {error:?}"),
            }
            let stats = connection.stats().unwrap();
            assert_eq!(
                Some(stats.request_bytes),
                expected["request_bytes"].as_u64(),
                "{id}"
            );
            assert_eq!(
                stats.response_bytes,
                expected["response_bytes"].as_u64(),
                "{id}: close payload must not count as application bytes"
            );
            assert_eq!(
                stats.close_status.map(u64::from),
                expected["close_status"].as_u64(),
                "{id}"
            );
        }
        assert_eq!(
            token.is_cancelled(),
            expected["failure"]["caller_cancelled"].as_bool().unwrap(),
            "{id}"
        );
        server.abort();
        compared.push(id);
    }
    assert_eq!(
        compared,
        [
            "native-upgrade-404",
            "native-invalid-accept",
            "native-status200",
            "native-bad-connection",
            "native-missing-upgrade",
            "native-empty-close"
        ]
    );
}
