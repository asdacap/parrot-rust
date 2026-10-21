use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_provider::{http_stream::ProviderHttpClient, responses_websocket::*};
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
use tungstenite::handshake::derive_accept_key;

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
fn connector() -> NativeConnector {
    NativeConnector::new(ProviderHttpClient::standard(
        reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
    ))
}
async fn request(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    loop {
        let mut byte = [0];
        stream.read_exact(&mut byte).await.unwrap();
        bytes.push(byte[0]);
        if bytes.ends_with(b"\r\n\r\n") {
            return String::from_utf8(bytes).unwrap();
        }
        assert!(bytes.len() < 16384);
    }
}
fn upgrade(request: &str) -> Vec<u8> {
    let key = request
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(name, _)| name.eq_ignore_ascii_case("sec-websocket-key"))
                .map(|(_, value)| value.trim())
        })
        .unwrap();
    assert_eq!(key.len(), 24);
    format!("HTTP/1.1 101 Switching Protocols\r\nConnection: keep-alive, Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {}\r\nx-codex-turn-state: native\r\n\r\n", derive_accept_key(key.as_bytes())).into_bytes()
}
async fn listener() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}/v1/responses", listener.local_addr().unwrap());
    (listener, endpoint)
}
fn frame(opcode: u8, final_frame: bool, data: &[u8]) -> Vec<u8> {
    let mut result = vec![opcode | if final_frame { 128 } else { 0 }];
    if data.len() < 126 {
        result.push(data.len() as u8);
    } else if data.len() <= 65535 {
        result.push(126);
        result.extend_from_slice(&(data.len() as u16).to_be_bytes());
    } else {
        result.push(127);
        result.extend_from_slice(&(data.len() as u64).to_be_bytes());
    }
    result.extend_from_slice(data);
    result
}
async fn scripted(bytes: Vec<u8>) -> (Box<dyn Socket>, tokio::task::JoinHandle<()>) {
    let (listener, endpoint) = listener().await;
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let req = request(&mut stream).await;
        let mut response = upgrade(&req);
        response.extend_from_slice(&bytes);
        stream.write_all(&response).await.unwrap();
    });
    let connected = connector()
        .connect(
            &endpoint,
            &vec![],
            Some(Duration::from_secs(2)),
            &Token::default(),
        )
        .await
        .unwrap();
    assert!(
        connected
            .response_headers
            .iter()
            .any(|(name, value)| name == "x-codex-turn-state" && value == "native")
    );
    (connected.socket, server)
}
async fn receive(socket: &mut dyn Socket) -> Result<(ReceivedFrame, Vec<u8>), WebSocketError> {
    let mut buffer = [0; 8192];
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        socket.receive(&mut buffer, &Token::default()),
    )
    .await
    .expect("bounded receive")?;
    let bytes = buffer[..result.count].to_vec();
    Ok((result, bytes))
}

#[tokio::test]
async fn coalesced_upgrade_bytes_and_empty_data_frame_are_preserved() {
    let mut bytes = frame(1, true, b"");
    bytes.extend_from_slice(&frame(1, true, b"coalesced"));
    let (mut socket, server) = scripted(bytes).await;
    let (empty, _) = receive(&mut *socket).await.unwrap();
    assert_eq!(empty.count, 0);
    assert!(empty.end_of_message);
    let (received, bytes) = receive(&mut *socket).await.unwrap();
    assert_eq!(bytes, b"coalesced");
    assert!(received.end_of_message);
    server.await.unwrap();
}

#[tokio::test]
async fn complete_nonfinal_fragment_is_returned_before_cancellation_and_poisoning() {
    let (listener, endpoint) = listener().await;
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let req = request(&mut stream).await;
        let mut response = upgrade(&req);
        response.extend_from_slice(&[0x01, 6]);
        response.extend_from_slice(b"prefix");
        stream.write_all(&response).await.unwrap();
        let mut rest = [0];
        assert_eq!(stream.read(&mut rest).await.unwrap(), 0);
    });
    let mut connected = connector()
        .connect(&endpoint, &vec![], None, &Token::default())
        .await
        .unwrap();
    let (prefix, bytes) = receive(&mut *connected.socket).await.unwrap();
    assert_eq!(bytes, b"prefix");
    assert!(!prefix.end_of_message);
    let token = Token::default();
    let later = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        later.cancel();
    });
    let mut buffer = [0; 8192];
    assert_eq!(
        connected
            .socket
            .receive(&mut buffer, &token)
            .await
            .unwrap_err(),
        WebSocketError::Cancelled
    );
    assert_eq!(connected.socket.state(), SocketState::Aborted);
    server.await.unwrap();
}

#[tokio::test]
async fn partial_application_bytes_are_recorded_before_outer_idle_timeout() {
    let (listener, endpoint) = listener().await;
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let req = request(&mut stream).await;
        let mut response = upgrade(&req);
        response.extend_from_slice(&[0x01, 6]);
        response.extend_from_slice(b"prefix");
        stream.write_all(&response).await.unwrap();
        let mut buffer = [0; 1024];
        while stream.read(&mut buffer).await.unwrap() != 0 {}
    });
    let connected = connector()
        .connect(&endpoint, &vec![], None, &Token::default())
        .await
        .unwrap();
    let mut transport = ResponsesWebSocket::new(
        connected.socket,
        connected.response_headers,
        Some(Duration::from_millis(50)),
        1024,
    );
    let mut exchange = transport.send(b"{}".to_vec(), "test").unwrap();
    assert!(
        matches!(exchange.next(&Token::default()).await.unwrap_err(),WebSocketError::Wire(message) if message.contains("idle timeout"))
    );
    assert_eq!(exchange.stats().response_bytes, Some(6));
    drop(exchange);
    drop(transport);
    server.await.unwrap();
}

#[tokio::test]
async fn partial_application_bytes_are_recorded_before_cancellation() {
    let (listener, endpoint) = listener().await;
    let (sent, ready) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let req = request(&mut stream).await;
        let mut response = upgrade(&req);
        response.extend_from_slice(&[0x01, 6]);
        response.extend_from_slice(b"prefix");
        stream.write_all(&response).await.unwrap();
        sent.send(()).unwrap();
        let mut buffer = [0; 1024];
        while stream.read(&mut buffer).await.unwrap() != 0 {}
    });
    let connected = connector()
        .connect(&endpoint, &vec![], None, &Token::default())
        .await
        .unwrap();
    let token = Token::default();
    let later = token.clone();
    let cancel = tokio::spawn(async move {
        ready.await.unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        later.cancel();
    });
    let mut transport = ResponsesWebSocket::new(connected.socket, vec![], None, 1024);
    let mut exchange = transport.send(vec![], "test").unwrap();
    assert_eq!(
        exchange.next(&token).await.unwrap_err(),
        WebSocketError::Cancelled
    );
    assert_eq!(exchange.stats().response_bytes, Some(6));
    drop(exchange);
    drop(transport);
    cancel.await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn successive_complete_fragments_reset_outer_idle_without_waiting_for_complete_message() {
    let (listener, endpoint) = listener().await;
    let payload = b"{\"type\":\"response.completed\",\"response\":{\"output\":[]}}";
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let req = request(&mut stream).await;
        stream.write_all(&upgrade(&req)).await.unwrap();
        let count = payload.chunks(10).len();
        for (index, part) in payload.chunks(10).enumerate() {
            let bytes = frame(if index == 0 { 1 } else { 0 }, index + 1 == count, part);
            stream.write_all(&bytes).await.unwrap();
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    });
    let connected = connector()
        .connect(&endpoint, &vec![], None, &Token::default())
        .await
        .unwrap();
    let mut transport = ResponsesWebSocket::new(
        connected.socket,
        vec![],
        Some(Duration::from_millis(100)),
        1024,
    );
    let mut exchange = transport.send(vec![], "test").unwrap();
    assert!(exchange.next(&Token::default()).await.unwrap().is_some());
    while exchange.next(&Token::default()).await.unwrap().is_some() {}
    assert_eq!(exchange.stats().response_bytes, Some(payload.len() as u64));
    server.await.unwrap();
}

#[tokio::test]
async fn control_only_traffic_does_not_refresh_application_idle() {
    let (listener, endpoint) = listener().await;
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let req = request(&mut stream).await;
        stream.write_all(&upgrade(&req)).await.unwrap();
        for _ in 0..50 {
            if stream.write_all(&frame(10, true, b"alive")).await.is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    let connected = connector()
        .connect(&endpoint, &vec![], None, &Token::default())
        .await
        .unwrap();
    let mut transport = ResponsesWebSocket::new(
        connected.socket,
        vec![],
        Some(Duration::from_millis(50)),
        1024,
    );
    let mut exchange = transport.send(vec![], "test").unwrap();
    let result = tokio::time::timeout(Duration::from_millis(180), exchange.next(&Token::default()))
        .await
        .unwrap();
    assert!(
        matches!(result,Err(WebSocketError::Wire(message)) if message.contains("idle timeout"))
    );
    assert_eq!(exchange.stats().response_bytes, None);
    drop(exchange);
    drop(transport);
    server.await.unwrap();
}

#[tokio::test]
async fn fragmented_utf8_ping_and_close_are_handled_without_control_bytes() {
    let mut bytes = frame(1, false, &[0xe2]);
    bytes.extend_from_slice(&frame(9, true, b"ping"));
    bytes.extend_from_slice(&frame(0, true, &[0x82, 0xac]));
    bytes.extend_from_slice(&frame(8, true, &[3, 232, b'o', b'k']));
    let (mut socket, server) = scripted(bytes).await;
    let (first, bytes) = receive(&mut *socket).await.unwrap();
    assert_eq!(bytes, [0xe2]);
    assert!(!first.end_of_message);
    let (last, bytes) = receive(&mut *socket).await.unwrap();
    assert_eq!(bytes, [0x82, 0xac]);
    assert!(last.end_of_message);
    let (close, bytes) = receive(&mut *socket).await.unwrap();
    assert!(bytes.is_empty());
    assert_eq!(close.kind, FrameKind::Close);
    assert_eq!(close.close_status, Some(1000));
    assert_eq!(close.close_description.as_deref(), Some("ok"));
    server.await.unwrap();
}

#[tokio::test]
async fn malformed_frame_matrix_aborts_without_allocating_advertised_payload() {
    let cases = vec![
        vec![0xc1, 0],
        vec![0x81, 0x80],
        vec![0x80, 0],
        vec![0x83, 0],
        vec![0x09, 0],
        vec![0x89, 126, 0, 126],
        vec![0x81, 126, 0, 1],
        vec![0x81, 127, 0x80, 0, 0, 0, 0, 0, 0, 0],
        vec![0x81, 1, 0xff],
        vec![0x81, 1, 0xe2],
        vec![0x88, 1, 0],
        vec![0x88, 2, 3, 237],
        vec![0x88, 3, 3, 232, 0xff],
        vec![0x01, 0, 0x81, 0],
        vec![0x01, 0],
        vec![0x81, 5, b'x'],
        vec![0x81],
    ];
    for bytes in cases {
        let (mut socket, server) = scripted(bytes.clone()).await;
        let result = loop {
            match receive(&mut *socket).await {
                Ok((frame, _)) if !frame.end_of_message => {}
                result => break result,
            }
        };
        assert!(result.is_err(), "accepted malformed {bytes:?}");
        assert_eq!(socket.state(), SocketState::Aborted);
        server.await.unwrap();
    }
    // A valid huge advertised length is not buffered: one full caller-sized unit returns.
    let mut bytes = vec![0x81, 127, 0, 0, 0, 1, 0, 0, 0, 0];
    bytes.extend_from_slice(&[b'a'; 8192]);
    let (mut socket, server) = scripted(bytes).await;
    let (received, bytes) = receive(&mut *socket).await.unwrap();
    assert_eq!(bytes, vec![b'a'; 8192]);
    assert!(!received.end_of_message);
    server.await.unwrap();
}

async fn read_client_frame(stream: &mut TcpStream) -> (u8, bool, [u8; 4], Vec<u8>) {
    let mut header = [0; 2];
    stream.read_exact(&mut header).await.unwrap();
    assert_ne!(header[1] & 128, 0);
    let length = match header[1] & 127 {
        126 => {
            let mut bytes = [0; 2];
            stream.read_exact(&mut bytes).await.unwrap();
            u16::from_be_bytes(bytes) as usize
        }
        127 => {
            let mut bytes = [0; 8];
            stream.read_exact(&mut bytes).await.unwrap();
            u64::from_be_bytes(bytes) as usize
        }
        value => value as usize,
    };
    let mut mask = [0; 4];
    stream.read_exact(&mut mask).await.unwrap();
    let mut payload = vec![0; length];
    stream.read_exact(&mut payload).await.unwrap();
    for (index, byte) in payload.iter_mut().enumerate() {
        *byte ^= mask[index % 4];
    }
    (header[0] & 15, header[0] & 128 != 0, mask, payload)
}
#[tokio::test]
async fn writes_mask_every_frame_and_use_continuation_opcodes_and_all_length_forms() {
    let (listener, endpoint) = listener().await;
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let req = request(&mut stream).await;
        stream.write_all(&upgrade(&req)).await.unwrap();
        let first = read_client_frame(&mut stream).await;
        let middle = read_client_frame(&mut stream).await;
        let last = read_client_frame(&mut stream).await;
        assert_eq!((first.0, first.1), (1, false));
        assert_eq!((middle.0, middle.1), (0, false));
        assert_eq!((last.0, last.1), (0, true));
        assert_eq!(first.3, b"first");
        assert_eq!(middle.3, vec![b'm'; 126]);
        assert_eq!(last.3, vec![b'z'; 65536]);
        assert_ne!(first.2, middle.2);
        assert_ne!(middle.2, last.2);
    });
    let mut connected = connector()
        .connect(&endpoint, &vec![], None, &Token::default())
        .await
        .unwrap();
    connected
        .socket
        .send(b"first", false, &Token::default())
        .await
        .unwrap();
    connected
        .socket
        .send(&[b'm'; 126], false, &Token::default())
        .await
        .unwrap();
    connected
        .socket
        .send(&vec![b'z'; 65536], true, &Token::default())
        .await
        .unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn handshake_status_headers_and_unsolicited_negotiation_are_validated() {
    for variant in [
        "404",
        "405",
        "426",
        "accept",
        "connection",
        "extension",
        "protocol",
    ] {
        let (listener, endpoint) = listener().await;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let req = request(&mut stream).await;
            let mut response = String::from_utf8(upgrade(&req)).unwrap();
            match variant {
                "404" | "405" | "426" => {
                    response = format!("HTTP/1.1 {variant} Rejected\r\nContent-Length: 0\r\n\r\n")
                }
                "accept" => response = response.replace("Sec-WebSocket-Accept:", "x-wrong:"),
                "connection" => {
                    response =
                        response.replace("Connection: keep-alive, Upgrade", "Connection: close")
                }
                "extension" => {
                    response = response.replace(
                        "\r\n\r\n",
                        "\r\nSec-WebSocket-Extensions: permessage-deflate\r\n\r\n",
                    )
                }
                "protocol" => {
                    response = response.replace(
                        "\r\n\r\n",
                        "\r\nSec-WebSocket-Protocol: unsolicited\r\n\r\n",
                    )
                }
                _ => unreachable!(),
            }
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let result = connector()
            .connect(
                &endpoint,
                &vec![],
                Some(Duration::from_secs(2)),
                &Token::default(),
            )
            .await;
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("accepted invalid {variant}"),
        };
        if let Ok(status) = variant.parse::<u16>() {
            assert!(matches!(error,WebSocketError::Upgrade {status: actual,..} if actual==status));
        } else {
            assert!(matches!(error, WebSocketError::Upgrade { status: 101, .. }));
        }
        server.await.unwrap();
    }
}

#[tokio::test]
async fn connect_timeout_cancellation_and_disposal_interrupt_upgrade() {
    for mode in ["timeout", "cancel", "dispose"] {
        let (listener, endpoint) = listener().await;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = request(&mut stream).await;
            let mut buffer = [0];
            assert_eq!(stream.read(&mut buffer).await.unwrap(), 0);
        });
        let client =
            ProviderHttpClient::standard(reqwest::Client::builder().no_proxy().build().unwrap());
        let connector = NativeConnector::new(client.clone());
        let token = Token::default();
        let later = token.clone();
        let action = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if mode == "cancel" {
                later.cancel();
            }
            if mode == "dispose" {
                client.dispose();
            }
        });
        let result = connector
            .connect(&endpoint, &vec![], Some(Duration::from_millis(80)), &token)
            .await;
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("unexpected upgrade"),
        };
        match mode {
            "timeout" => assert_eq!(
                error,
                WebSocketError::Transport(
                    "provider: websocket upgrade did not return headers within 00:00:00.0800000"
                        .into()
                )
            ),
            "cancel" => assert_eq!(error, WebSocketError::Cancelled),
            "dispose" => assert_eq!(error, WebSocketError::Disposed),
            _ => unreachable!(),
        }
        action.await.unwrap();
        server.await.unwrap();
    }
}
