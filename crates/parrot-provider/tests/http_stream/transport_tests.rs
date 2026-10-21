use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_provider::http_stream::*;
use std::{
    future::pending,
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use url::Url;

struct Never;
impl Cancellation for Never {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(pending())
    }
}
struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn real_tls_relaxation_is_authority_scoped_and_never_follows_redirects() {
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/http_stream/fixtures/tls-fixture");
    let python = r#"
import http.server,ssl,sys
class Handler(http.server.BaseHTTPRequestHandler):
    count=0
    def log_message(self,*args): pass
    def do_GET(self):
        Handler.count+=1
        if self.path=='/redirect':
            self.send_response(302)
            self.send_header('Location','https://localhost:%s/escaped'%self.server.server_port)
            body=b''
        else:
            self.send_response(200)
            body=str(Handler.count).encode() if self.path=='/count' else b'ok'
        self.send_header('Content-Length',str(len(body)))
        self.end_headers()
        self.wfile.write(body)
server=http.server.ThreadingHTTPServer(('127.0.0.1',0),Handler)
ctx=ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
ctx.load_cert_chain(sys.argv[1],sys.argv[2])
server.socket=ctx.wrap_socket(server.socket,server_side=True)
print(server.server_port,flush=True)
server.serve_forever()
"#;
    let child = Command::new("python3")
        .arg("-u")
        .arg("-c")
        .arg(python)
        .arg(fixture.join("cert.pem"))
        .arg(fixture.join("key.pem"))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut server = ChildGuard(child);
    let mut line = String::new();
    BufReader::new(server.0.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let port: u16 = line.trim().parse().expect("loopback TLS server started");
    let base = format!("https://127.0.0.1:{port}");
    let standard = ProviderHttpClient::standard(
        reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
    );
    let strict = ReqwestTransport::new(standard.clone());
    assert!(matches!(
        get(
            &strict,
            Url::parse(&base).unwrap(),
            vec![],
            Duration::from_secs(5),
            100,
            &Never
        )
        .await,
        Err(HttpError::Transport(_))
    ));
    let mut catalog = ProviderHttpClientCatalog::new(standard.clone());
    let relaxed = catalog.resolve(&base, true).unwrap();
    let transport = ReqwestTransport::new(relaxed.clone());
    assert_eq!(
        get(
            &transport,
            Url::parse(&base).unwrap(),
            vec![],
            Duration::from_secs(5),
            100,
            &Never
        )
        .await
        .unwrap(),
        "ok"
    );
    let other_authority = Url::parse(&format!("https://localhost:{port}")).unwrap();
    assert!(!relaxed.allows_invalid_tls_for(&other_authority));
    assert!(matches!(
        get(
            &transport,
            other_authority,
            vec![],
            Duration::from_secs(5),
            100,
            &Never
        )
        .await,
        Err(HttpError::Transport(_))
    ));
    // The transport still addresses loopback. A Unicode hostname after a
    // backslash is path text, never authority for the invalid-certificate scope.
    let unicode_base = format!("https://bücher.example:{port}");
    let unicode_scoped = catalog.resolve(&unicode_base, true).unwrap();
    assert!(Endpoint::parse(&format!("{base}\\@bücher.example:{port}/spoof")).is_err());
    let forged = Endpoint::parse(&format!("{base}/@bücher.example:{port}/spoof")).unwrap();
    assert_eq!(forged.as_url().host_str(), Some("127.0.0.1"));
    assert!(!unicode_scoped.allows_invalid_tls_for(&forged));
    assert!(matches!(
        get(
            &ReqwestTransport::new(unicode_scoped),
            forged,
            vec![],
            Duration::from_secs(5),
            100,
            &Never
        )
        .await,
        Err(HttpError::Transport(_))
    ));
    let Err(HttpError::Response(failure)) = get(
        &transport,
        Url::parse(&format!("{base}/redirect")).unwrap(),
        vec![],
        Duration::from_secs(5),
        100,
        &Never,
    )
    .await
    else {
        panic!("redirect was not followed");
    };
    assert_eq!(failure.status, 302);
    assert_eq!(
        get(
            &transport,
            Url::parse(&format!("{base}/count")).unwrap(),
            vec![],
            Duration::from_secs(5),
            100,
            &Never
        )
        .await
        .unwrap(),
        "3"
    );
    catalog.dispose();
    assert!(matches!(
        get(
            &transport,
            Url::parse(&base).unwrap(),
            vec![],
            Duration::from_secs(5),
            100,
            &Never
        )
        .await,
        Err(HttpError::Disposed)
    ));
    assert!(!standard.is_disposed());
}

#[tokio::test]
async fn real_post_preserves_payload_headers_and_owning_sse_stream() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let header_end = loop {
            let mut byte = [0; 1];
            assert_eq!(socket.read(&mut byte).await.unwrap(), 1);
            request.push(byte[0]);
            if request.ends_with(b"\r\n\r\n") {
                break request.len();
            }
        };
        let header = String::from_utf8(request.clone()).unwrap().to_lowercase();
        let size: usize = header
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        request.resize(header_end + size, 0);
        socket.read_exact(&mut request[header_end..]).await.unwrap();
        let bytes = "data: hé\n\n".as_bytes();
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nX-Codex-Turn-State: first\r\nConnection: close\r\n\r\n",bytes.len()).as_bytes()).await.unwrap();
        for byte in bytes {
            socket.write_all(&[*byte]).await.unwrap();
            tokio::task::yield_now().await;
        }
        request
    });
    let transport = ReqwestTransport::new(ProviderHttpClient::standard(
        reqwest::Client::builder().no_proxy().build().unwrap(),
    ));
    let counter = ResponseByteCounter::default();
    let response = open_stream(
        &transport,
        Url::parse(&format!("http://{address}/responses")).unwrap(),
        br#"{"x":1}"#.to_vec(),
        vec![("X-Custom".into(), "custom".into())],
        StreamOptions {
            header_timeout: Duration::from_secs(5),
            idle_timeout: Duration::from_secs(5),
            maximum_request_bytes: 100,
        },
        counter.clone(),
        &Never,
    )
    .await
    .unwrap();
    assert_eq!(response.turn_state(), Some("first"));
    let mut events = SseStream::new(response.content, 100);
    assert_eq!(events.next(&Never).await.unwrap().unwrap().data, "hé");
    assert!(events.next(&Never).await.unwrap().is_none());
    assert_eq!(counter.get(), "data: hé\n\n".len() as u64);
    let request = server.await.unwrap();
    let text = String::from_utf8(request).unwrap();
    let lower = text.to_lowercase();
    assert!(text.starts_with("POST /responses HTTP/1.1\r\n"));
    assert!(lower.contains("content-type: application/json\r\n"));
    assert!(lower.contains("accept: text/event-stream\r\n"));
    assert!(lower.contains("x-custom: custom\r\n"));
    assert!(text.ends_with("\r\n\r\n{\"x\":1}"));
}

#[tokio::test]
async fn explicitly_injected_proxy_stays_loopback_and_does_not_contact_origin() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        loop {
            let mut byte = [0; 1];
            assert_eq!(socket.read(&mut byte).await.unwrap(), 1);
            request.push(byte[0]);
            if request.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await
            .unwrap();
        String::from_utf8(request).unwrap()
    });
    let client = reqwest::Client::builder()
        .no_proxy()
        .proxy(reqwest::Proxy::http(format!("http://{address}")).unwrap())
        .build()
        .unwrap();
    let transport = ReqwestTransport::new(ProviderHttpClient::standard(client));
    let endpoint = endpoint_url("http://offline-origin.invalid/v1", "models", false, true)
        .unwrap()
        .into_url();
    assert_eq!(
        get(
            &transport,
            endpoint,
            vec![],
            Duration::from_secs(5),
            10,
            &Never
        )
        .await
        .unwrap(),
        "ok"
    );
    let request = server.await.unwrap();
    assert!(request.starts_with("GET http://offline-origin.invalid/v1/models HTTP/1.1\r\n"));
    assert!(!request.to_lowercase().contains("authorization:"));
}
