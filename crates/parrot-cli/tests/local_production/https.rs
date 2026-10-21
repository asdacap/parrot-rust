use super::*;
use openssl::{pkcs12::Pkcs12, pkey::PKey, symm::Cipher, x509::X509};
use parrot_domain::protocol::bindings::parrot::parrot_client::ParrotClient;
use parrot_host::grpc::token::TransportToken;
use std::os::unix::fs::PermissionsExt;
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint};

#[cfg(target_os = "linux")]
mod linux_store;

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
    let key = PKey::private_key_from_pem(TLS_KEY).unwrap();
    std::fs::write(
        directory.join("encrypted-key.pem"),
        key.private_key_to_pem_pkcs8_passphrase(Cipher::aes_256_cbc(), TLS_PASSWORD.as_bytes())
            .unwrap(),
    )
    .unwrap();
    let certificate = X509::from_pem(TLS_CERTIFICATE).unwrap();
    let mut chain = openssl::stack::Stack::new().unwrap();
    chain.push(X509::from_pem(TLS_ROOT).unwrap()).unwrap();
    let archive = Pkcs12::builder()
        .name("synthetic-loopback")
        .pkey(&key)
        .cert(&certificate)
        .ca(chain)
        .build2(TLS_PASSWORD)
        .unwrap();
    std::fs::write(directory.join("server.pfx"), archive.to_der().unwrap()).unwrap();
}

fn trusted_tls() -> ClientTlsConfig {
    ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(TLS_ROOT))
        .domain_name("localhost")
}

#[test]
fn executable_https_pem_pfx_and_encrypted_pem_preserve_trust_authentication_and_shutdown() {
    for provisioning in ["pem", "pfx", "encrypted-pem"] {
        let fixture = Fixture::new(false);
        tls_identity_files(&fixture);
        let token_path = tls_token(&fixture);
        let token = TransportToken::load(&token_path).unwrap();
        let token_path = token_path.to_str().unwrap();
        let environment = match provisioning {
            "pem" => vec![
                (
                    "ASPNETCORE_Kestrel__Certificates__Default__Path",
                    "server.pem",
                ),
                (
                    "ASPNETCORE_Kestrel__Certificates__Default__KeyPath",
                    "key.pem",
                ),
            ],
            "pfx" => vec![
                ("DOTNET_Kestrel__Certificates__Default__Path", "server.pfx"),
                (
                    "DOTNET_Kestrel__Certificates__Default__Password",
                    TLS_PASSWORD,
                ),
            ],
            _ => vec![
                ("Kestrel__Certificates__Default__Path", "server.pem"),
                (
                    "Kestrel__Certificates__Default__KeyPath",
                    "encrypted-key.pem",
                ),
                ("Kestrel__Certificates__Default__Password", TLS_PASSWORD),
            ],
        };
        let mut child = fixture.spawn_with_environment(
            &[
                "serve",
                "--listen",
                "https://127.0.0.1:0",
                "--token-file",
                token_path,
            ],
            &environment,
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
fn executable_https_rejects_missing_malformed_and_wrong_password_configuration_safely() {
    let fixture = Fixture::new(false);
    tls_identity_files(&fixture);
    let token_path = tls_token(&fixture);
    let token_path = token_path.to_str().unwrap();
    std::fs::write(
        fixture.root.join("workspace/malformed.pfx"),
        "not a certificate",
    )
    .unwrap();
    let cases = [
        (vec![], "HTTPS requires an explicit certificate"),
        (
            vec![("Kestrel__Certificates__Default__Path", "absent.pfx")],
            "cannot read HTTPS certificate",
        ),
        (
            vec![("Kestrel__Certificates__Default__Path", "malformed.pfx")],
            "invalid HTTPS PFX/PKCS12 certificate or password",
        ),
        (
            vec![
                ("Kestrel__Certificates__Default__Path", "server.pfx"),
                (
                    "Kestrel__Certificates__Default__Password",
                    "wrong-synthetic-secret",
                ),
            ],
            "invalid HTTPS PFX/PKCS12 certificate or password",
        ),
        (
            vec![
                ("Kestrel__Certificates__Default__Path", "server.pem"),
                ("Kestrel__Certificates__Default__KeyPath", "absent-key.pem"),
            ],
            "cannot read HTTPS private key",
        ),
    ];
    for (environment, expected) in cases {
        let output = fixture
            .spawn_with_environment(
                &[
                    "serve",
                    "--listen",
                    "https://127.0.0.1:0",
                    "--token-file",
                    token_path,
                ],
                &environment,
            )
            .finish();
        assert!(!output.status.success(), "{}", describe(&output));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(expected), "{}", describe(&output));
        assert!(
            !stderr.contains("parrot: listening on "),
            "{}",
            describe(&output)
        );
        assert!(!stderr.contains(TLS_PASSWORD));
        assert!(!stderr.contains("wrong-synthetic-secret"));
        assert!(!stderr.contains("PRIVATE KEY"));
    }
    std::fs::remove_dir_all(&fixture.root).unwrap();
}

#[test]
fn executable_https_reads_content_root_appsettings_and_application_environment_overrides() {
    let fixture = Fixture::new(false);
    tls_identity_files(&fixture);
    let token_path = tls_token(&fixture);
    let token = TransportToken::load(&token_path).unwrap();
    let content_root = fixture.root.join("cert-content");
    std::fs::create_dir_all(&content_root).unwrap();
    std::fs::copy(
        fixture.root.join("workspace/server.pem"),
        content_root.join("server.pem"),
    )
    .unwrap();
    std::fs::copy(
        fixture.root.join("workspace/key.pem"),
        content_root.join("key.pem"),
    )
    .unwrap();
    std::fs::write(
        content_root.join("appsettings.json"),
        r#"{
        // An overridden fallback must not be opened.
        "Kestrel": { "Certificates": { "Default": { "Path": "absent.pfx", } } }
    }"#,
    )
    .unwrap();
    std::fs::write(content_root.join("appsettings.Staging.json"), r#"{
        "Kestrel": { "Certificates": { "Default": { "Path": "server.pem", "KeyPath": "absent-key.pem" } } }
    }"#).unwrap();
    let mut child = fixture.spawn_with_environment(
        &[
            "serve",
            "--listen",
            "https://127.0.0.1:0",
            "--token-file",
            token_path.to_str().unwrap(),
        ],
        &[
            ("ASPNETCORE_CONTENTROOT", "/absent-synthetic-root"),
            ("DOTNET_CONTENTROOT", content_root.to_str().unwrap()),
            ("ASPNETCORE_ENVIRONMENT", "Development"),
            ("DOTNET_ENVIRONMENT", "Staging"),
            (
                "DOTNET_Kestrel__Certificates__Default__Path",
                "absent-host.pfx",
            ),
            ("Kestrel__Certificates__Default__KeyPath", "key.pem"),
        ],
    );
    let address = child.line("parrot: listening on ");
    runtime().block_on(async {
        tokio::time::timeout(Duration::from_secs(5), async {
            let diagnostics = Arc::new(
                FileDiagnosticLog::open_global(
                    fixture.root.join("client-logs"),
                    "https-appsettings",
                    |_| {},
                )
                .unwrap(),
            );
            let mut client = GrpcTransportClient::connect(
                &TransportAddress::parse(&address).unwrap(),
                Some(&token),
                Some(trusted_tls()),
                diagnostics,
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
        .expect("appsettings HTTPS endpoint timed out");
    });
    child.terminate();
    assert_clean_exit(&child.finish());
    std::fs::remove_dir_all(&fixture.root).unwrap();
}

fn developer_archive(subject: &str, oid: Option<&str>) -> (X509, Vec<u8>) {
    developer_archive_version(subject, oid, 4)
}

fn developer_archive_version(subject: &str, oid: Option<&str>, version: u8) -> (X509, Vec<u8>) {
    use openssl::{
        asn1::{Asn1Integer, Asn1Object, Asn1OctetString, Asn1Time},
        bn::BigNum,
        hash::MessageDigest,
        x509::{
            X509Extension, X509NameBuilder,
            extension::{BasicConstraints, ExtendedKeyUsage, KeyUsage, SubjectAlternativeName},
        },
    };
    let key = PKey::private_key_from_pem(TLS_KEY).unwrap();
    let mut name = X509NameBuilder::new().unwrap();
    name.append_entry_by_text("CN", subject).unwrap();
    let name = name.build();
    let mut certificate = X509::builder().unwrap();
    certificate.set_version(2).unwrap();
    let serial = Asn1Integer::from_bn(&BigNum::from_u32(42).unwrap()).unwrap();
    certificate.set_serial_number(&serial).unwrap();
    certificate.set_subject_name(&name).unwrap();
    certificate.set_issuer_name(&name).unwrap();
    certificate.set_pubkey(&key).unwrap();
    certificate
        .set_not_before(&Asn1Time::days_from_now(0).unwrap())
        .unwrap();
    certificate
        .set_not_after(&Asn1Time::days_from_now(2).unwrap())
        .unwrap();
    certificate
        .append_extension(BasicConstraints::new().critical().build().unwrap())
        .unwrap();
    certificate
        .append_extension(
            KeyUsage::new()
                .critical()
                .digital_signature()
                .key_encipherment()
                .build()
                .unwrap(),
        )
        .unwrap();
    certificate
        .append_extension(ExtendedKeyUsage::new().server_auth().build().unwrap())
        .unwrap();
    let san = SubjectAlternativeName::new()
        .dns("localhost")
        .ip("127.0.0.1")
        .build(&certificate.x509v3_context(None, None))
        .unwrap();
    certificate.append_extension(san).unwrap();
    if let Some(oid) = oid {
        let oid = Asn1Object::from_str(oid).unwrap();
        let payload = Asn1OctetString::new_from_bytes(&[version]).unwrap();
        certificate
            .append_extension(X509Extension::new_from_der(&oid, false, &payload).unwrap())
            .unwrap();
    }
    certificate.sign(&key, MessageDigest::sha256()).unwrap();
    let certificate = certificate.build();
    let archive = Pkcs12::builder()
        .name("synthetic-developer")
        .pkey(&key)
        .cert(&certificate)
        .build2(TLS_PASSWORD)
        .unwrap()
        .to_der()
        .unwrap();
    (certificate, archive)
}

#[test]
fn executable_https_developer_file_fallback_uses_synthetic_home_or_appdata_and_host_application_name()
 {
    for appdata in [false, true] {
        let fixture = Fixture::new(false);
        let token_path = tls_token(&fixture);
        let token = TransportToken::load(&token_path).unwrap();
        let (certificate, archive) = developer_archive("localhost", Some("1.3.6.1.4.1.311.84.1.1"));
        let base = if appdata {
            fixture.root.join("appdata/ASP.NET/https")
        } else {
            fixture.root.join("home/.aspnet/https")
        };
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(
            base.join(if appdata {
                "synthetic-app.pfx"
            } else {
                "parrot.pfx"
            }),
            archive,
        )
        .unwrap();
        let appdata_root = fixture.root.join("appdata");
        let mut environment = vec![("Kestrel__Certificates__Development__Password", TLS_PASSWORD)];
        if appdata {
            environment.extend([
                ("APPDATA", appdata_root.to_str().unwrap()),
                ("ASPNETCORE_APPLICATIONNAME", "wrong-host-name"),
                ("DOTNET_APPLICATIONNAME", "synthetic-app"),
                ("APPLICATIONNAME", "ignored-application-name"),
            ]);
        }
        let mut child = fixture.spawn_with_environment(
            &[
                "serve",
                "--listen",
                "https://127.0.0.1:0",
                "--token-file",
                token_path.to_str().unwrap(),
            ],
            &environment,
        );
        let address = child.line("parrot: listening on ");
        runtime().block_on(async {
            tokio::time::timeout(Duration::from_secs(5), async {
                let diagnostics = Arc::new(
                    FileDiagnosticLog::open_global(
                        fixture.root.join("client-logs"),
                        "https-developer-file",
                        |_| {},
                    )
                    .unwrap(),
                );
                let tls = ClientTlsConfig::new()
                    .ca_certificate(Certificate::from_pem(certificate.to_pem().unwrap()))
                    .domain_name("localhost");
                let mut client = GrpcTransportClient::connect(
                    &TransportAddress::parse(&address).unwrap(),
                    Some(&token),
                    Some(tls),
                    diagnostics,
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
            .expect("developer-file HTTPS endpoint timed out");
        });
        child.terminate();
        assert_clean_exit(&child.finish());
        std::fs::remove_dir_all(&fixture.root).unwrap();
    }
}

#[test]
fn executable_https_developer_file_fallback_rejects_non_developer_identity_and_disabled_fallback() {
    let fixture = Fixture::new(false);
    let token_path = tls_token(&fixture);
    let base = fixture.root.join("home/.aspnet/https");
    std::fs::create_dir_all(&base).unwrap();
    let path = base.join("parrot.pfx");
    let (_, valid) = developer_archive("localhost", Some("1.3.6.1.4.1.311.84.1.1"));
    let (_, missing_oid) = developer_archive("localhost", None);
    let (_, wrong_oid) = developer_archive("localhost", Some("1.3.6.1.4.1.311.84.1.2"));
    let (_, wrong_subject) = developer_archive("not-localhost", Some("1.3.6.1.4.1.311.84.1.1"));
    for (archive, environment) in [
        (missing_oid.as_slice(), vec![]),
        (wrong_oid.as_slice(), vec![]),
        (wrong_subject.as_slice(), vec![]),
        (b"malformed PFX".as_slice(), vec![]),
        (
            valid.as_slice(),
            vec![(
                "Kestrel__Certificates__Development__Password",
                "wrong-synthetic-secret",
            )],
        ),
        (
            valid.as_slice(),
            vec![("Kestrel__Certificates__Default__Password", "")],
        ),
        (
            valid.as_slice(),
            vec![("Kestrel__Certificates__Development__Path", "")],
        ),
        // APPDATA suppresses HOME lookup even when its certificate is missing.
        (
            valid.as_slice(),
            vec![("APPDATA", fixture.root.to_str().unwrap())],
        ),
    ] {
        std::fs::write(&path, archive).unwrap();
        let mut combined = vec![("Kestrel__Certificates__Development__Password", TLS_PASSWORD)];
        combined.extend(environment);
        let output = fixture
            .spawn_with_environment(
                &[
                    "serve",
                    "--listen",
                    "https://127.0.0.1:0",
                    "--token-file",
                    token_path.to_str().unwrap(),
                ],
                &combined,
            )
            .finish();
        assert!(!output.status.success(), "{}", describe(&output));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("no usable certificate file or certificate-store identity was found"),
            "{}",
            describe(&output)
        );
        assert!(!stderr.contains("parrot: listening on "));
        assert!(!stderr.contains(TLS_PASSWORD));
        assert!(!stderr.contains("wrong-synthetic-secret"));
    }
    std::fs::remove_file(path).unwrap();
    let output = fixture
        .spawn_with_environment(
            &[
                "serve",
                "--listen",
                "https://127.0.0.1:0",
                "--token-file",
                token_path.to_str().unwrap(),
            ],
            &[("Kestrel__Certificates__Development__Password", TLS_PASSWORD)],
        )
        .finish();
    assert!(!output.status.success(), "{}", describe(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("no usable certificate file or certificate-store identity was found")
    );
    std::fs::remove_dir_all(&fixture.root).unwrap();
}
