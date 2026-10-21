use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_provider::{
    http_stream::{ProviderHttpClient, ProviderHttpClientCatalog, get},
    responses_websocket::WebSocketError,
    routing::builder::{CatalogHttpClients, HttpClients},
};
use std::{io::Cursor, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};
use tokio_rustls::{TlsAcceptor, rustls};
use tungstenite::handshake::derive_accept_key;
use url::Url;

const TIMEOUT: Duration = Duration::from_secs(5);
struct Active;
impl Cancellation for Active {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}

fn clients() -> CatalogHttpClients {
    CatalogHttpClients::new(ProviderHttpClientCatalog::new(
        ProviderHttpClient::standard(
            reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
        ),
    ))
}

async fn tls_server(connections: usize) -> (String, JoinHandle<Vec<String>>) {
    let certificates = rustls_pemfile::certs(&mut Cursor::new(include_bytes!(
        "../http_stream/fixtures/tls-fixture/cert.pem"
    )))
    .collect::<Result<Vec<_>, _>>()
    .unwrap();
    let key = rustls_pemfile::private_key(&mut Cursor::new(include_bytes!(
        "../http_stream/fixtures/tls-fixture/key.pem"
    )))
    .unwrap()
    .unwrap();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(certificates, key)
    .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("https://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for _ in 0..connections {
            let (stream, _) = tokio::time::timeout(TIMEOUT, listener.accept())
                .await
                .expect("expected loopback TLS connection")
                .unwrap();
            let Ok(mut stream) = tokio::time::timeout(TIMEOUT, acceptor.accept(stream))
                .await
                .expect("bounded TLS handshake")
            else {
                continue;
            };
            let mut request = Vec::new();
            tokio::time::timeout(TIMEOUT, async {
                while !request.ends_with(b"\r\n\r\n") {
                    request.push(stream.read_u8().await.unwrap());
                    assert!(request.len() < 16384);
                }
            })
            .await
            .expect("bounded request headers");
            let request = String::from_utf8(request).unwrap();
            let key = request.lines().find_map(|line| {
                line.split_once(':')
                    .filter(|(name, _)| name.eq_ignore_ascii_case("sec-websocket-key"))
                    .map(|(_, value)| value.trim())
            });
            let response = match key {
                Some(key) => format!(
                    "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {}\r\n\r\n",
                    derive_accept_key(key.as_bytes())
                ),
                None => {
                    "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".into()
                }
            };
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.flush().await.unwrap();
            requests.push(request);
        }
        requests
    });
    (base, server)
}

#[tokio::test]
async fn configured_provider_selects_the_same_relaxed_tls_policy_for_http_and_websocket() {
    let (base, server) = tls_server(5).await;
    let clients = clients();
    let strict = clients.resolve(&base, false).unwrap();
    let websocket = base.replacen("https:", "wss:", 1);
    assert!(
        get(
            strict.streaming.as_ref(),
            Url::parse(&base).unwrap(),
            vec![],
            TIMEOUT,
            16,
            &Active,
        )
        .await
        .is_err()
    );
    assert!(matches!(
        strict
            .connector
            .connect(&websocket, &vec![], Some(TIMEOUT), &Active)
            .await,
        Err(WebSocketError::Transport(_))
    ));

    let relaxed = clients.resolve(&base, true).unwrap();
    assert_eq!(
        get(
            relaxed.streaming.as_ref(),
            Url::parse(&base).unwrap(),
            vec![],
            TIMEOUT,
            16,
            &Active,
        )
        .await
        .unwrap(),
        "ok"
    );
    let mut connected = relaxed
        .connector
        .connect(
            &websocket,
            &vec![
                (
                    "Authorization".into(),
                    "Bearer synthetic-oauth-token".into(),
                ),
                ("ChatGPT-Account-Id".into(), "synthetic-account".into()),
            ],
            Some(TIMEOUT),
            &Active,
        )
        .await
        .unwrap();
    connected.socket.dispose();
    let still_strict = clients.resolve(&base, false).unwrap();
    assert!(matches!(
        still_strict
            .connector
            .connect(&websocket, &vec![], Some(TIMEOUT), &Active)
            .await,
        Err(WebSocketError::Transport(_))
    ));
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 2);
    let websocket_request = requests[1].to_ascii_lowercase();
    assert!(websocket_request.contains("authorization: bearer synthetic-oauth-token\r\n"));
    assert!(websocket_request.contains("chatgpt-account-id: synthetic-account\r\n"));
}

#[tokio::test]
async fn relaxed_websocket_client_cannot_relax_another_authority_or_provider() {
    let authority = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let configured = format!("https://{}", authority.local_addr().unwrap());
    let (other, server) = tls_server(3).await;
    let clients = clients();
    let relaxed = clients.resolve(&configured, true).unwrap();
    let other_websocket = other.replacen("https:", "wss:", 1);
    assert!(
        get(
            relaxed.streaming.as_ref(),
            Url::parse(&other).unwrap(),
            vec![],
            TIMEOUT,
            16,
            &Active,
        )
        .await
        .is_err()
    );
    assert!(matches!(
        relaxed
            .connector
            .connect(&other_websocket, &vec![], Some(TIMEOUT), &Active)
            .await,
        Err(WebSocketError::Transport(_))
    ));
    let strict_other = clients.resolve(&other, false).unwrap();
    assert!(matches!(
        strict_other
            .connector
            .connect(&other_websocket, &vec![], Some(TIMEOUT), &Active)
            .await,
        Err(WebSocketError::Transport(_))
    ));
    assert!(server.await.unwrap().is_empty());
}

#[tokio::test]
async fn invalid_certificate_opt_in_on_http_does_not_relax_websocket_tls() {
    let (base, server) = tls_server(1).await;
    let clients = clients();
    let selected = clients
        .resolve(&base.replacen("https:", "http:", 1), true)
        .unwrap();
    assert!(matches!(
        selected
            .connector
            .connect(
                &base.replacen("https:", "wss:", 1),
                &vec![],
                Some(TIMEOUT),
                &Active
            )
            .await,
        Err(WebSocketError::Transport(_))
    ));
    assert!(server.await.unwrap().is_empty());
}
