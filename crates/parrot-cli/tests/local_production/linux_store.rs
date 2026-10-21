use super::*;

fn persisted_certificate(
    fixture: &Fixture,
    store: &str,
    name: &str,
    certificate: &X509,
    key: bool,
) -> PathBuf {
    let directory = fixture
        .root
        .join("home/.dotnet/corefx/cryptography/x509stores")
        .join(store);
    std::fs::create_dir_all(&directory).unwrap();
    let mut archive = Pkcs12::builder();
    archive.cert(certificate);
    if key {
        archive.pkey(&PKey::private_key_from_pem(TLS_KEY).unwrap());
    }
    let path = directory.join(format!("{name}.pfx"));
    std::fs::write(&path, archive.build2("").unwrap().to_der().unwrap()).unwrap();
    path
}

fn assert_store_server(
    fixture: &Fixture,
    token_path: &std::path::Path,
    environment: &[(&str, &str)],
    selected: &X509,
    root: &[u8],
) {
    let mut child = fixture.spawn_with_environment(
        &[
            "serve",
            "--listen",
            "https://127.0.0.1:0",
            "--token-file",
            token_path.to_str().unwrap(),
        ],
        environment,
    );
    let address = child.line("parrot: listening on ");
    let socket = TransportAddress::parse(&address)
        .unwrap()
        .socket_addr()
        .unwrap();
    let stream = TcpStream::connect_timeout(&socket, Duration::from_secs(5)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut context = openssl::ssl::SslContextBuilder::new(openssl::ssl::SslMethod::tls()).unwrap();
    context.set_verify(openssl::ssl::SslVerifyMode::PEER);
    context
        .cert_store_mut()
        .add_cert(X509::from_pem(root).unwrap())
        .unwrap();
    context.set_alpn_protos(b"\x02h2").unwrap();
    let mut ssl = openssl::ssl::Ssl::new(&context.build()).unwrap();
    ssl.set_hostname("localhost").unwrap();
    ssl.param_mut().set_host("localhost").unwrap();
    let peer = ssl.connect(stream).unwrap();
    assert_eq!(
        peer.ssl().peer_certificate().unwrap().to_der().unwrap(),
        selected.to_der().unwrap(),
        "wrong store certificate selected"
    );
    drop(peer);
    runtime().block_on(async {
        tokio::time::timeout(Duration::from_secs(5), async {
            let log = Arc::new(
                FileDiagnosticLog::open_global(
                    fixture.root.join("store-client-logs"),
                    "store-client",
                    |_| {},
                )
                .unwrap(),
            );
            let token = TransportToken::load(token_path).unwrap();
            let tls = ClientTlsConfig::new()
                .ca_certificate(Certificate::from_pem(root))
                .domain_name("localhost");
            let mut client = GrpcTransportClient::connect(
                &TransportAddress::parse(&address).unwrap(),
                Some(&token),
                Some(tls),
                log,
            )
            .unwrap();
            assert!(
                !client
                    .client()
                    .unwrap()
                    .list_models(wire::ListModelsRequest::new())
                    .await
                    .unwrap()
                    .into_inner()
                    .models
                    .is_empty()
            );
            client.close();
        })
        .await
        .expect("store-backed HTTPS endpoint timed out");
    });
    child.terminate();
    assert_clean_exit(&child.finish());
}

fn assert_store_failure(
    fixture: &Fixture,
    token: &std::path::Path,
    environment: &[(&str, &str)],
    expected: &str,
) {
    let output = fixture
        .spawn_with_environment(
            &[
                "serve",
                "--listen",
                "https://127.0.0.1:0",
                "--token-file",
                token.to_str().unwrap(),
            ],
            environment,
        )
        .finish();
    assert!(!output.status.success(), "{}", describe(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(expected), "{}", describe(&output));
    assert!(!stderr.contains("parrot: listening on "));
    assert!(!stderr.contains("PRIVATE KEY"));
}

#[test]
fn executable_https_linux_current_user_subject_store_requires_real_trust_unless_allow_invalid() {
    let fixture = Fixture::new(false);
    let token = tls_token(&fixture);
    let certificate = X509::from_pem(TLS_CERTIFICATE).unwrap();
    persisted_certificate(&fixture, "my", "server", &certificate, true);
    let root = persisted_certificate(
        &fixture,
        "root",
        "synthetic-root",
        &X509::from_pem(TLS_ROOT).unwrap(),
        false,
    );
    let system = fixture.root.join("empty-system-trust");
    std::fs::create_dir_all(&system).unwrap();
    let missing_bundle = fixture.root.join("absent-system-roots.pem");
    let mut environment = vec![
        ("Kestrel__Certificates__Default__Subject", "LOCALhost"),
        ("Kestrel__Certificates__Default__Store", "MY"),
        ("Kestrel__Certificates__Default__Location", "1"),
        ("SSL_CERT_FILE", missing_bundle.to_str().unwrap()),
        ("SSL_CERT_DIR", system.to_str().unwrap()),
    ];
    assert_store_server(&fixture, &token, &environment, &certificate, TLS_ROOT);
    std::fs::remove_file(root).unwrap();
    assert_store_failure(
        &fixture,
        &token,
        &environment,
        "no matching HTTPS certificate",
    );
    let root_directory = fixture
        .root
        .join("home/.dotnet/corefx/cryptography/x509stores/root");
    std::fs::remove_dir(&root_directory).unwrap();
    std::fs::write(
        &root_directory,
        b"not a readable certificate-store directory",
    )
    .unwrap();
    environment.push(("Kestrel__Certificates__Default__AllowInvalid", "true"));
    assert_store_server(&fixture, &token, &environment, &certificate, TLS_ROOT);
    environment.push(("Kestrel__Certificates__Default__AllowInvalid", "false"));
    assert_store_failure(
        &fixture,
        &token,
        &environment,
        "cannot read Linux CurrentUser certificate store",
    );
    environment.pop();
    environment.push(("Kestrel__Certificates__Default__Location", "2"));
    assert_store_failure(
        &fixture,
        &token,
        &environment,
        "Linux LocalMachine certificate stores support only read-only Root and CA",
    );
    std::fs::remove_dir_all(&fixture.root).unwrap();
}

#[test]
fn executable_https_linux_development_store_follows_file_fallback_and_selects_highest_usable_version()
 {
    let fixture = Fixture::new(false);
    let token = tls_token(&fixture);
    let oid = Some("1.3.6.1.4.1.311.84.1.1");
    let (version4, archive4) = developer_archive_version("localhost", oid, 4);
    let (version5, _) = developer_archive_version("other-subject", oid, 5);
    let (version3, _) = developer_archive_version("localhost", oid, 3);
    let first = persisted_certificate(&fixture, "my", "version4", &version4, true);
    let selected = persisted_certificate(&fixture, "my", "version5", &version5, true);
    persisted_certificate(&fixture, "my", "obsolete-version3", &version3, true);
    persisted_certificate(
        &fixture,
        "my",
        "not-a-developer-certificate",
        &X509::from_pem(TLS_CERTIFICATE).unwrap(),
        true,
    );
    std::fs::write(
        selected.with_file_name("malformed.pfx"),
        b"malformed persisted certificate",
    )
    .unwrap();
    let root = version5.to_pem().unwrap();
    assert_store_server(&fixture, &token, &[], &version5, &root);

    let developer_path = fixture.root.join("home/.aspnet/https");
    std::fs::create_dir_all(&developer_path).unwrap();
    std::fs::write(developer_path.join("parrot.pfx"), b"invalid developer file").unwrap();
    let environment = [("Kestrel__Certificates__Development__Password", TLS_PASSWORD)];
    assert_store_server(&fixture, &token, &environment, &version5, &root);

    // A valid file would win without Default. Empty Default must suppress only
    // that file, not the later persisted-development-store fallback.
    std::fs::write(developer_path.join("parrot.pfx"), archive4).unwrap();
    std::fs::write(
        fixture.root.join("workspace/appsettings.json"),
        r#"{"Kestrel":{"Certificates":{"Default":{}}}}"#,
    )
    .unwrap();
    assert_store_server(&fixture, &token, &environment, &version5, &root);
    let invalid_file = [("Kestrel__Certificates__Default__Path", "absent.pfx")];
    assert_store_failure(
        &fixture,
        &token,
        &invalid_file,
        "cannot read HTTPS certificate",
    );

    std::fs::remove_file(first).unwrap();
    std::fs::remove_file(selected).unwrap();
    assert_store_failure(
        &fixture,
        &token,
        &[],
        "no usable certificate file or certificate-store identity was found",
    );
    std::fs::remove_dir_all(&fixture.root).unwrap();
}
