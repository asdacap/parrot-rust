use super::*;
use openssl::{pkey::PKey, rsa::Rsa, symm::Cipher, x509::X509};
use parrot_domain::protocol::bindings::parrot::parrot_client::ParrotClient;
use parrot_host::grpc::token::TransportToken;
use std::os::unix::fs::PermissionsExt;
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint};

const TLS_CERTIFICATE: &[u8] =
    include_bytes!("../../../parrot-host/tests/grpc/fixtures/server.pem");
const TLS_KEY: &[u8] = include_bytes!("../../../parrot-host/tests/grpc/fixtures/server-key.pem");
const TLS_ROOT: &[u8] = include_bytes!("../../../parrot-host/tests/grpc/fixtures/root.pem");
const TLS_PASSWORD: &str = "synthetic-certificate-password";

fn tls_token(fixture: &Fixture) -> PathBuf {
    let path = fixture.root.join("token");
    std::fs::write(&path, "ab".repeat(32)).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    path
}

fn tls_identity_files(fixture: &Fixture) {
    let directory = fixture.root.join("workspace");
    std::fs::write(
        directory.join("server.pem"),
        [TLS_CERTIFICATE, TLS_ROOT].concat(),
    )
    .unwrap();
    std::fs::write(directory.join("key.pem"), TLS_KEY).unwrap();
}

fn trusted_tls() -> ClientTlsConfig {
    ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(TLS_ROOT))
        .domain_name("localhost")
}

#[test]
fn executable_https_explicit_pem_preserves_chain_trust_authentication_and_shutdown() {
    for absolute_paths in [false, true] {
        let fixture = Fixture::new(false);
        tls_identity_files(&fixture);
        let token_path = tls_token(&fixture);
        let token = TransportToken::load(&token_path).unwrap();
        let directory = if absolute_paths {
            fixture.root.join("workspace")
        } else {
            PathBuf::new()
        };
        let certificate = directory.join("server.pem");
        let key = directory.join("key.pem");
        std::fs::write(
            fixture.root.join("workspace/appsettings.json"),
            "malformed legacy configuration",
        )
        .unwrap();
        let mut child = fixture.spawn_with_environment(
            &[
                "serve",
                "--listen",
                "https://127.0.0.1:0",
                "--token-file",
                token_path.to_str().unwrap(),
                "--tls-cert-file",
                certificate.to_str().unwrap(),
                "--tls-key-file",
                key.to_str().unwrap(),
            ],
            &[
                ("ASPNETCORE_CONTENTROOT", "/absent-aspnetcore-root"),
                ("DOTNET_CONTENTROOT", "/absent-dotnet-root"),
                ("DOTNET_ENVIRONMENT", "Staging"),
                (
                    "ASPNETCORE_Kestrel__Certificates__Default__Path",
                    "absent.pem",
                ),
                ("DOTNET_Kestrel__Certificates__Default__Path", "absent.pem"),
                ("Kestrel__Certificates__Default__Path", "absent.pem"),
                ("Kestrel__Certificates__Default__KeyPath", "absent-key.pem"),
            ],
        );
        let address = child.line("parrot: listening on ");
        assert!(address.starts_with("https://127.0.0.1:"), "{address}");
        // An independent verified TLS peer observes the whole supplied chain,
        // including the synthetic root (not merely a usable leaf certificate).
        let mut context =
            openssl::ssl::SslContextBuilder::new(openssl::ssl::SslMethod::tls()).unwrap();
        context.set_verify(openssl::ssl::SslVerifyMode::PEER);
        context
            .cert_store_mut()
            .add_cert(X509::from_pem(TLS_ROOT).unwrap())
            .unwrap();
        context.set_alpn_protos(b"\x02h2").unwrap();
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
        let mut ssl = openssl::ssl::Ssl::new(&context.build()).unwrap();
        ssl.set_hostname("localhost").unwrap();
        ssl.param_mut().set_host("localhost").unwrap();
        let peer = ssl.connect(stream).unwrap();
        let chain = peer.ssl().peer_cert_chain().unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(
            chain[1].to_der().unwrap(),
            X509::from_pem(TLS_ROOT).unwrap().to_der().unwrap()
        );
        drop(peer);
        runtime().block_on(async {
            tokio::time::timeout(Duration::from_secs(10), async {
                let diagnostics = Arc::new(
                    FileDiagnosticLog::open_global(
                        fixture.root.join("client-logs"),
                        "https-provisioning",
                        |_| {},
                    )
                    .unwrap(),
                );
                let address = TransportAddress::parse(&address).unwrap();
                let mut trusted = GrpcTransportClient::connect(
                    &address,
                    Some(&token),
                    Some(trusted_tls()),
                    diagnostics.clone(),
                )
                .unwrap();
                let models = trusted
                    .client()
                    .unwrap()
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
                trusted.close();

                let mut untrusted =
                    GrpcTransportClient::connect(&address, Some(&token), None, diagnostics)
                        .unwrap();
                assert_eq!(
                    untrusted
                        .client()
                        .unwrap()
                        .list_models(wire::ListModelsRequest::new())
                        .await
                        .unwrap_err()
                        .code(),
                    tonic::Code::Unavailable
                );
                untrusted.close();

                // Bypass only the client's local token requirement, not server auth.
                let channel = Endpoint::from_shared(address.to_string())
                    .unwrap()
                    .tls_config(trusted_tls())
                    .unwrap()
                    .connect()
                    .await
                    .unwrap();
                let mut client = ParrotClient::new(channel);
                assert_eq!(
                    client
                        .list_models(wire::ListModelsRequest::new())
                        .await
                        .unwrap_err()
                        .code(),
                    tonic::Code::Unauthenticated
                );
                let mut wrong = tonic::Request::new(wire::ListModelsRequest::new());
                wrong.metadata_mut().insert(
                    "authorization",
                    format!("Bearer {}", "cd".repeat(32)).parse().unwrap(),
                );
                assert_eq!(
                    client.list_models(wrong).await.unwrap_err().code(),
                    tonic::Code::Unauthenticated
                );
            })
            .await
            .expect("configured HTTPS endpoint timed out");
        });
        child.terminate();
        assert_clean_exit(&child.finish());
        std::fs::remove_dir_all(&fixture.root).unwrap();
    }
}

#[test]
fn executable_https_rejects_missing_unreadable_and_invalid_pem_safely() {
    let fixture = Fixture::new(false);
    tls_identity_files(&fixture);
    let token_path = tls_token(&fixture);
    let directory = fixture.root.join("workspace");
    std::fs::write(
        directory.join("malformed.pem"),
        "synthetic-invalid-pem-secret",
    )
    .unwrap();
    std::fs::write(directory.join("empty.pem"), []).unwrap();
    let key = PKey::private_key_from_pem(TLS_KEY).unwrap();
    std::fs::write(
        directory.join("encrypted-key.pem"),
        key.private_key_to_pem_pkcs8_passphrase(Cipher::aes_256_cbc(), TLS_PASSWORD.as_bytes())
            .unwrap(),
    )
    .unwrap();
    let wrong_key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
    std::fs::write(
        directory.join("wrong-key.pem"),
        wrong_key.private_key_to_pem_pkcs8().unwrap(),
    )
    .unwrap();
    let cases = [
        (vec![], "HTTPS requires --tls-cert-file"),
        (
            vec!["--tls-cert-file", "server.pem"],
            "HTTPS requires --tls-cert-file",
        ),
        (
            vec!["--tls-key-file", "key.pem"],
            "HTTPS requires --tls-cert-file",
        ),
        (
            vec!["--tls-cert-file", "absent.pem", "--tls-key-file", "key.pem"],
            "cannot read HTTPS certificate",
        ),
        (
            vec![
                "--tls-cert-file",
                "server.pem",
                "--tls-key-file",
                "absent-key.pem",
            ],
            "cannot read HTTPS private key",
        ),
        (
            vec![
                "--tls-cert-file",
                "malformed.pem",
                "--tls-key-file",
                "key.pem",
            ],
            "transport error",
        ),
        (
            vec!["--tls-cert-file", "empty.pem", "--tls-key-file", "key.pem"],
            "transport error",
        ),
        (
            vec![
                "--tls-cert-file",
                "server.pem",
                "--tls-key-file",
                "malformed.pem",
            ],
            "transport error",
        ),
        (
            vec![
                "--tls-cert-file",
                "server.pem",
                "--tls-key-file",
                "empty.pem",
            ],
            "transport error",
        ),
        (
            vec![
                "--tls-cert-file",
                "server.pem",
                "--tls-key-file",
                "encrypted-key.pem",
            ],
            "transport error",
        ),
        (
            vec![
                "--tls-cert-file",
                "server.pem",
                "--tls-key-file",
                "wrong-key.pem",
            ],
            "transport error",
        ),
    ];
    for (flags, expected) in cases {
        let mut arguments = vec![
            "serve",
            "--listen",
            "https://127.0.0.1:0",
            "--token-file",
            token_path.to_str().unwrap(),
        ];
        arguments.extend(flags);
        let output = fixture.spawn(&arguments).finish();
        assert!(!output.status.success(), "{}", describe(&output));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(expected), "{}", describe(&output));
        assert!(
            !stderr.contains("parrot: listening on "),
            "{}",
            describe(&output)
        );
        assert!(!stderr.contains(TLS_PASSWORD));
        assert!(!stderr.contains("synthetic-invalid-pem-secret"));
        assert!(!stderr.contains("PRIVATE KEY"));
    }
    std::fs::remove_dir_all(&fixture.root).unwrap();
}

#[test]
fn executable_https_legacy_configuration_does_not_supply_an_identity() {
    let fixture = Fixture::new(false);
    tls_identity_files(&fixture);
    let token_path = tls_token(&fixture);
    std::fs::write(
        fixture.root.join("workspace/appsettings.json"),
        r#"{"Kestrel":{"Certificates":{"Default":{"Path":"server.pem","KeyPath":"key.pem"}}}}"#,
    )
    .unwrap();
    let output = fixture
        .spawn_with_environment(
            &[
                "serve",
                "--listen",
                "https://127.0.0.1:0",
                "--token-file",
                token_path.to_str().unwrap(),
            ],
            &[
                (
                    "ASPNETCORE_Kestrel__Certificates__Default__Path",
                    "server.pem",
                ),
                (
                    "ASPNETCORE_Kestrel__Certificates__Default__KeyPath",
                    "key.pem",
                ),
                ("DOTNET_Kestrel__Certificates__Default__Path", "server.pem"),
                ("DOTNET_Kestrel__Certificates__Default__KeyPath", "key.pem"),
                ("Kestrel__Certificates__Default__Path", "server.pem"),
                ("Kestrel__Certificates__Default__KeyPath", "key.pem"),
            ],
        )
        .finish();
    assert!(!output.status.success(), "{}", describe(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("HTTPS requires --tls-cert-file"),
        "{}",
        describe(&output)
    );
    assert!(!stderr.contains("parrot: listening on "));
    std::fs::remove_dir_all(&fixture.root).unwrap();
}

#[test]
fn executable_rejects_tls_flags_for_http_and_unix() {
    let fixture = Fixture::new(false);
    let token_path = tls_token(&fixture);
    for listen in [
        None,
        Some("http://127.0.0.1:0"),
        Some("unix:/tmp/parrot-unused.sock"),
    ] {
        for flags in [
            vec!["--tls-cert-file", "absent.pem"],
            vec!["--tls-key-file", "absent-key.pem"],
            vec![
                "--tls-cert-file",
                "absent.pem",
                "--tls-key-file",
                "absent-key.pem",
            ],
        ] {
            let mut arguments = vec!["serve", "--token-file", token_path.to_str().unwrap()];
            if let Some(listen) = listen {
                arguments.extend(["--listen", listen]);
            }
            arguments.extend(flags);
            let output = fixture.spawn(&arguments).finish();
            assert!(!output.status.success(), "{}", describe(&output));
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                stderr
                    .contains("--tls-cert-file and --tls-key-file require an HTTPS listen address"),
                "{}",
                describe(&output)
            );
            assert!(!stderr.contains("parrot: listening on "));
        }
    }
    std::fs::remove_dir_all(&fixture.root).unwrap();
}
