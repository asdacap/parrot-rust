//! Web projection of the already-owned native typed handler failure contract.
use crate::{
    authoritative::{Fixture, bounded, create_request},
    lifecycle::{NoWeb, message, request},
};
use http_body_util::BodyExt;
use parrot_domain::protocol::messages::parrot as wire;
use parrot_host::{grpc::service::GrpcService, web::server::WebServer};

#[tokio::test]
async fn web_preserves_real_internal_io_message_and_existing_handler_diagnostic() {
    let fixture = Fixture::new().await;
    let mut server = bounded(WebServer::start(
        GrpcService::new(fixture.service.clone()),
        NoWeb::default(),
        0,
    ))
    .await
    .unwrap();
    let (response, connection) =
        request(server.address(), "CreateSession", &create_request()).await;
    let created: wire::UserSession = message(
        &bounded(response.into_body().collect())
            .await
            .unwrap()
            .to_bytes(),
    );
    bounded(connection).await.unwrap().unwrap();
    let workspace = std::path::PathBuf::from(&created.working_directory);
    let configuration = workspace.parent().unwrap().join("config.yaml");
    std::fs::remove_file(&configuration).unwrap();
    std::fs::create_dir(&configuration).unwrap();
    let probe = workspace
        .parent()
        .unwrap()
        .join("web-persistence-message-probe");
    std::fs::write(&probe, b"probe").unwrap();
    let expected = std::fs::rename(&probe, &configuration)
        .unwrap_err()
        .to_string();
    std::fs::remove_file(probe).unwrap();
    let (response, connection) = request(
        server.address(),
        "SetContextLimit",
        &wire::SetContextLimitRequest {
            user_session_id: created.id,
            context_limit: "100k".into(),
            ..Default::default()
        },
    )
    .await;
    let body = bounded(response.into_body().collect())
        .await
        .unwrap()
        .to_bytes();
    bounded(connection).await.unwrap().unwrap();
    bounded(server.close()).await.unwrap();
    fixture.close().await;
    assert_eq!(body[0], 0x80);
    let length = u32::from_be_bytes(body[1..5].try_into().unwrap()) as usize;
    assert_eq!(body.len(), length + 5);
    let trailers = std::str::from_utf8(&body[5..]).unwrap();
    assert!(trailers.contains("grpc-status: 13\r\n"), "{trailers}");
    let headers = tonic::Status::internal(&expected)
        .into_http()
        .into_parts()
        .0
        .headers;
    let encoded = headers["grpc-message"].to_str().unwrap();
    assert!(
        trailers.contains(&format!("grpc-message: {encoded}\r\n")),
        "{trailers}"
    );
    let log = std::fs::read_to_string(fixture.diagnostics.path()).unwrap();
    let failures = log
        .lines()
        .filter(|line| line.contains("event=\"handler.failure\""))
        .collect::<Vec<_>>();
    assert_eq!(failures.len(), 1);
    assert!(failures[0].contains("request=\"/parrot.v1.Parrot/SetContextLimit\""));
    assert!(failures[0].contains("error=\"io\""));
    assert!(!failures[0].contains(&expected));
}
