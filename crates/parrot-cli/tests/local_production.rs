//! Real executable + production composition; only the HTTP provider is synthetic.
//! Set PARROT_TEST_BINARY to exercise an independently built (including Nix) binary.

#![cfg(unix)]

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::{fd::OwnedFd, unix::net::UnixStream},
    path::PathBuf,
    process::{Child, Command, Output, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use parrot_domain::protocol::messages::{parrot as wire, web};
use parrot_host::grpc::{address::TransportAddress, transport::GrpcTransportClient};
use parrot_storage::diagnostics::FileDiagnosticLog;
use protobuf::Message;

const CHILD_TIMEOUT: Duration = Duration::from_secs(30);

static NEXT: AtomicU64 = AtomicU64::new(0);

type Captured = Arc<Mutex<Vec<u8>>>;
type Reader = thread::JoinHandle<std::io::Result<()>>;

/// Owns only the freshly spawned fixture binary, including both pipe readers.
/// Logs survive assertion failures and timeout cleanup underneath the fixture root.
struct BinaryChild {
    child: Child,
    readers: Vec<Reader>,
    stdout: Captured,
    stderr: Captured,
    deadline: Instant,
    logs: PathBuf,
    joined: bool,
}

impl BinaryChild {
    fn spawn(mut command: Command, logs: PathBuf) -> Self {
        std::fs::create_dir_all(&logs).unwrap();
        let deadline = Instant::now() + CHILD_TIMEOUT;
        // Socket-backed stdio lets each reader obey a deadline even if a faulty
        // child leaves a descendant holding an inherited output descriptor.
        let (stdout_pipe, stdout_writer) = UnixStream::pair().unwrap();
        let (stderr_pipe, stderr_writer) = UnixStream::pair().unwrap();
        let child = command
            .stdout(Stdio::from(OwnedFd::from(stdout_writer)))
            .stderr(Stdio::from(OwnedFd::from(stderr_writer)))
            .spawn()
            .unwrap();
        drop(command);
        let stdout = Arc::new(Mutex::new(Vec::new()));
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let readers = vec![
            capture(
                stdout_pipe,
                stdout.clone(),
                logs.join("stdout.log"),
                deadline,
            ),
            capture(
                stderr_pipe,
                stderr.clone(),
                logs.join("stderr.log"),
                deadline,
            ),
        ];
        Self {
            child,
            readers,
            stdout,
            stderr,
            deadline,
            logs,
            joined: false,
        }
    }

    fn line(&mut self, prefix: &str) -> String {
        loop {
            if let Some(line) = String::from_utf8_lossy(&self.stderr.lock().unwrap())
                .split_inclusive('\n')
                .filter_map(|line| line.strip_suffix('\n'))
                .find_map(|line| line.strip_prefix(prefix))
            {
                return line.to_owned();
            }
            if self.child.try_wait().unwrap().is_some() || Instant::now() >= self.deadline {
                let output = self.finish_inner();
                panic!(
                    "binary did not publish {prefix:?}; logs: {}\n{}",
                    self.logs.display(),
                    describe(&output)
                );
            }
            thread::park_timeout(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    fn terminate(&mut self) {
        unsafe extern "C" {
            fn kill(pid: i32, signal: i32) -> i32;
        }
        assert!(
            self.child.try_wait().unwrap().is_none(),
            "binary exited before cooperative shutdown"
        );
        // SAFETY: the retained, unreaped Child owns this exact PID. SIGTERM is
        // handled by parrot's normal process cancellation observer.
        let result = unsafe { kill(i32::try_from(self.child.id()).unwrap(), 15) };
        assert_eq!(
            result,
            0,
            "cannot cancel own fixture binary: {}",
            std::io::Error::last_os_error()
        );
    }

    fn finish(mut self) -> Output {
        self.finish_inner()
    }

    fn finish_inner(&mut self) -> Output {
        let mut timed_out = false;
        loop {
            if self.child.try_wait().unwrap().is_some() {
                break;
            }
            if Instant::now() >= self.deadline {
                timed_out = true;
                self.child
                    .kill()
                    .expect("terminate only the timed-out fixture child");
                break;
            }
            thread::park_timeout(Duration::from_millis(10));
        }
        let status = self.child.wait().unwrap();
        for reader in self.readers.drain(..) {
            reader
                .join()
                .expect("fixture output reader panicked")
                .expect("capture fixture output");
        }
        self.joined = true;
        let output = Output {
            status,
            stdout: self.stdout.lock().unwrap().clone(),
            stderr: self.stderr.lock().unwrap().clone(),
        };
        assert!(
            !timed_out,
            "fixture binary exceeded {CHILD_TIMEOUT:?}; logs: {}\n{}",
            self.logs.display(),
            describe(&output)
        );
        output
    }
}

impl Drop for BinaryChild {
    fn drop(&mut self) {
        if !self.joined {
            // An assertion/request failure may bypass finish. Reap only this
            // fixture's child and join both readers before unwinding continues.
            if matches!(self.child.try_wait(), Ok(None)) {
                let _ = self.child.kill();
            }
            let _ = self.child.wait();
            for reader in self.readers.drain(..) {
                let _ = reader.join();
            }
            eprintln!("fixture binary logs preserved at {}", self.logs.display());
        }
    }
}

fn capture(mut pipe: UnixStream, bytes: Captured, path: PathBuf, deadline: Instant) -> Reader {
    thread::spawn(move || {
        pipe.set_read_timeout(Some(Duration::from_millis(100)))?;
        let mut log = std::fs::File::create(path)?;
        let mut block = [0; 4096];
        loop {
            if Instant::now() >= deadline + Duration::from_secs(5) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "fixture output did not reach EOF",
                ));
            }
            match pipe.read(&mut block) {
                Ok(0) => return Ok(()),
                Ok(count) => {
                    log.write_all(&block[..count])?;
                    bytes.lock().unwrap().extend_from_slice(&block[..count]);
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) && Instant::now() < deadline + Duration::from_secs(5) => {}
                Err(error) => return Err(error),
            }
        }
    })
}

fn describe(output: &Output) -> String {
    format!(
        "status: {}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

struct Fixture {
    root: PathBuf,
    address: String,
    requests: Arc<Mutex<Vec<String>>>,
    stopped: Arc<AtomicBool>,
    server: Option<thread::JoinHandle<()>>,
}

impl Fixture {
    fn new(fail: bool) -> Self {
        // Short roots keep the real per-session Unix socket below sockaddr_un's limit.
        let root = PathBuf::from("/tmp").join(format!(
            "pl-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join("workspace")).unwrap();
        std::fs::create_dir_all(root.join("config/parrotrust")).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap().to_string();
        std::fs::write(root.join("config/parrotrust/config.yaml"), format!(
            "model: fixture/test\nproviders:\n  fixture:\n    type: compatible\n    base_url: http://{address}/v1\n    api_key_env: PARROT_FIXTURE_KEY\n    allow_insecure_localhost: true\n    header_timeout_max_retries: 0\n    models:\n      test:\n        context: 32768\n        max_output_tokens: 1024\n        tools: true\nskills:\n  enabled: false\n"
        )).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = stopped.clone();
        let server = thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let request = read_request(&mut stream);
                        let chat = request.starts_with("POST /v1/chat/completions ");
                        captured.lock().unwrap().push(request);
                        let (status, content_type, body) = if chat && fail {
                            (
                                "400 Bad Request",
                                "application/json",
                                r#"{"error":{"message":"synthetic provider failure","type":"invalid_request_error"}}"#,
                            )
                        } else if chat {
                            (
                                "200 OK",
                                "text/event-stream",
                                "data: {\"choices\":[{\"delta\":{\"content\":\"local production answer\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":8,\"completion_tokens\":3}}\n\ndata: [DONE]\n\n",
                            )
                        } else {
                            (
                                "200 OK",
                                "application/json",
                                r#"{"data":[{"id":"test","object":"model"}]}"#,
                            )
                        };
                        write!(stream, "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::park_timeout(Duration::from_millis(10));
                    }
                    Err(error) => panic!("mock provider accept: {error}"),
                }
            }
        });
        Self {
            root,
            address,
            requests,
            stopped,
            server: Some(server),
        }
    }

    fn chat(&self, prompt: &str) -> Output {
        self.spawn(&["chat", "--basic", prompt]).finish()
    }

    fn spawn(&self, arguments: &[&str]) -> BinaryChild {
        let binary = std::env::var_os("PARROT_TEST_BINARY")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_parrot")));
        let mut command = Command::new(binary);
        command
            .args(arguments)
            .current_dir(self.root.join("workspace"))
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("PARROT_FIXTURE_KEY", "synthetic-key")
            // Optional models.dev metadata must not contact a real remote host.
            .env("HTTPS_PROXY", "http://127.0.0.1:1")
            .env("HTTP_PROXY", "http://127.0.0.1:1")
            .env("NO_PROXY", "127.0.0.1,localhost")
            .stdin(Stdio::null());
        BinaryChild::spawn(
            command,
            self.root
                .join(format!("child-{}", NEXT.fetch_add(1, Ordering::Relaxed))),
        )
    }

    fn chat_requests(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.starts_with("POST /v1/chat/completions "))
            .cloned()
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        self.server.take().unwrap().join().unwrap();
    }
}

fn read_request(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut block = [0; 4096];
    loop {
        let count = stream.read(&mut block).unwrap();
        assert_ne!(count, 0, "provider request ended early");
        bytes.extend_from_slice(&block[..count]);
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..end]);
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            if bytes.len() >= end + 4 + length {
                break;
            }
        }
    }
    String::from_utf8(bytes).unwrap()
}

fn assert_clean_exit(output: &Output) {
    assert!(output.status.success(), "{}", describe(output));
    let rendered = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !rendered.contains("local agent execution is unavailable"),
        "{rendered}"
    );
    assert!(
        !rendered.contains("LOCAL_CAPABILITY_UNAVAILABLE"),
        "{rendered}"
    );
}

fn assert_success(output: &Output) {
    assert_clean_exit(output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("local production answer"));
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[cfg(unix)]
#[test]
fn executable_serve_binds_default_uds_serves_real_catalog_and_joins_on_shutdown() {
    let fixture = Fixture::new(false);
    let mut child = fixture.spawn(&["serve"]);
    let socket = fixture.root.join("state/parrotrust/control/parrot.sock");
    let address = child.line("parrot: listening on ");
    assert_eq!(address, format!("unix:{}", socket.display()));
    assert!(socket.exists());
    runtime().block_on(async {
        tokio::time::timeout(Duration::from_secs(5), async {
            let diagnostics = Arc::new(
                FileDiagnosticLog::open_global(
                    fixture.root.join("client-logs"),
                    "serve-smoke",
                    |_| {},
                )
                .unwrap(),
            );
            let mut connection = GrpcTransportClient::connect(
                &TransportAddress::Unix(socket.clone()),
                None,
                None,
                diagnostics,
            )
            .unwrap();
            let mut client = connection.client().unwrap().clone();
            let models = client
                .list_models(wire::ListModelsRequest::new())
                .await
                .unwrap()
                .into_inner();
            assert!(
                models
                    .models
                    .iter()
                    .any(|model| model.provider_id == "fixture" && model.id == "test")
            );
            let modes = client
                .list_modes(wire::ListModesRequest::new())
                .await
                .unwrap()
                .into_inner();
            assert!(!modes.modes.is_empty());
            drop(client);
            connection.close();
        })
        .await
        .expect("default UDS service endpoint timed out");
    });
    child.terminate();
    assert_clean_exit(&child.finish());
    assert!(
        !socket.exists(),
        "default UDS was not removed after orderly shutdown"
    );
}

#[cfg(unix)]
#[test]
fn executable_web_binds_ephemeral_loopback_serves_assets_and_real_slash_then_joins() {
    let fixture = Fixture::new(false);
    let mut child = fixture.spawn(&["web", "--port", "0"]);
    let url = child.line("parrot: web UI at ");
    let address: std::net::SocketAddr = url.strip_prefix("http://").unwrap().parse().unwrap();
    assert!(address.ip().is_loopback());
    assert_ne!(address.port(), 0);
    runtime().block_on(async {
        tokio::time::timeout(Duration::from_secs(10), async {
            let http = reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap();
            let assets = http.get(format!("{url}/")).send().await.unwrap();
            assert!(assets.status().is_success());
            assert!(
                assets
                    .headers()
                    .get("content-type")
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .contains("text/html")
            );
            let html = assets.text().await.unwrap();
            assert!(
                html.contains("<html") && html.contains("<script"),
                "missing embedded browser assets: {html}"
            );
            let described: web::DescribeHostResponse =
                web_unary(&http, &url, "parrot.web.v1.ParrotWeb/DescribeHost").await;
            assert_eq!(
                PathBuf::from(described.working_directory),
                fixture.root.join("workspace")
            );
            let listed: web::ListSlashCommandsResponse =
                web_unary(&http, &url, "parrot.web.v1.ParrotWeb/ListSlashCommands").await;
            assert!(
                listed
                    .commands
                    .iter()
                    .any(|command| command.name == "/help")
            );
            let session: wire::UserSession =
                web_unary(&http, &url, "parrot.web.v1.ParrotWeb/OpenDefaultSession").await;
            assert!(!session.id.is_empty());
            assert_eq!(session.model, "fixture/test");
            let models: wire::ListModelsResponse =
                web_unary(&http, &url, "parrot.v1.Parrot/ListModels").await;
            assert!(
                models
                    .models
                    .iter()
                    .any(|model| model.provider_id == "fixture" && model.id == "test")
            );
        })
        .await
        .expect("web assets/service endpoints timed out");
    });
    let sockets = socket_paths(&fixture.root.join("state"));
    assert!(
        !sockets.is_empty(),
        "web did not create its actual local/session transports"
    );
    child.terminate();
    assert_clean_exit(&child.finish());
    for socket in sockets {
        assert!(!socket.exists(), "web left transport {}", socket.display());
    }
    assert!(
        TcpStream::connect_timeout(&address, Duration::from_secs(1)).is_err(),
        "web listener survived shutdown"
    );
}

async fn web_unary<M: Message>(http: &reqwest::Client, url: &str, method: &str) -> M {
    // All smoke calls take an empty protobuf request in a binary gRPC-Web frame.
    let response = http
        .post(format!("{url}/{method}"))
        .header("content-type", "application/grpc-web+proto")
        .body(vec![0; 5])
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let bytes = response.bytes().await.unwrap();
    let mut rest = bytes.as_ref();
    let mut message = None;
    let mut ok = false;
    while !rest.is_empty() {
        assert!(rest.len() >= 5, "truncated gRPC-Web header");
        let length = u32::from_be_bytes(rest[1..5].try_into().unwrap()) as usize;
        assert!(rest.len() >= 5 + length, "truncated gRPC-Web payload");
        let payload = &rest[5..5 + length];
        if rest[0] == 0x80 {
            let trailers = String::from_utf8_lossy(payload);
            assert!(
                trailers.lines().any(|line| line == "grpc-status: 0"),
                "{method}: {trailers}"
            );
            ok = true;
        } else {
            assert_eq!(rest[0], 0, "unsupported gRPC-Web frame");
            assert!(message.is_none(), "multiple unary responses");
            message = Some(M::parse_from_bytes(payload).unwrap());
        }
        rest = &rest[5 + length..];
    }
    assert!(ok, "{method}: missing successful gRPC-Web trailers");
    message.expect("missing unary response message")
}

#[cfg(unix)]
fn socket_paths(directory: &std::path::Path) -> Vec<PathBuf> {
    use std::os::unix::fs::FileTypeExt;
    let mut sockets = Vec::new();
    for entry in std::fs::read_dir(directory).unwrap() {
        let entry = entry.unwrap();
        let kind = entry.file_type().unwrap();
        if kind.is_dir() {
            sockets.extend(socket_paths(&entry.path()));
        } else if kind.is_socket() {
            sockets.push(entry.path());
        }
    }
    sockets
}

#[test]
fn executable_creates_then_resumes_real_local_session_and_joins_owners() {
    let fixture = Fixture::new(false);
    let first = fixture.chat("first synthetic prompt");
    assert_success(&first);
    assert!(String::from_utf8_lossy(&first.stderr).contains("created user session"));
    let second = fixture.chat("second synthetic prompt");
    assert_success(&second);
    assert!(String::from_utf8_lossy(&second.stderr).contains("loaded existing user session"));
    let requests = fixture.chat_requests();
    assert_eq!(requests.len(), 2, "provider at {}", fixture.address);
    assert!(requests[0].contains("first synthetic prompt"));
    assert!(requests[1].contains("first synthetic prompt"));
    assert!(requests[1].contains("second synthetic prompt"));
    assert!(!String::from_utf8_lossy(&second.stderr).contains("LOCAL_CAPABILITY_UNAVAILABLE"));
}

#[test]
fn executable_uploads_an_image_through_real_authorized_workspace_reads() {
    let fixture = Fixture::new(false);
    let png = [
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8,
        0xcf, 0xc0, 0xf0, 0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x89, 0x99, 0x3d, 0x1d, 0x00, 0x00,
        0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];
    std::fs::write(fixture.root.join("workspace/pixel.png"), png).unwrap();
    let output = fixture.chat("inspect @pixel.png");
    assert_success(&output);
    let requests = fixture.chat_requests();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].contains("data:image/png;base64,"),
        "{}",
        requests[0]
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("cannot attach"));
}

#[test]
fn executable_reports_real_provider_failure_and_exits_after_owned_shutdown() {
    let fixture = Fixture::new(true);
    let output = fixture.chat("synthetic failure prompt");
    assert!(
        !output.status.success(),
        "unexpected success: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let rendered = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        rendered.contains("synthetic provider failure"),
        "{rendered}"
    );
    assert_eq!(fixture.chat_requests().len(), 1);
}
