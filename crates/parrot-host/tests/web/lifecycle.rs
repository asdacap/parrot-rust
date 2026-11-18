//! HTTP/1.1 transport against actual composition/storage. The fixture owns the
//! construction executor through transport join and explicit service close.
pub(super) use super::browser_service::NoWeb;
use crate::authoritative::{Fixture, active, bounded, create_request};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Request, body::Incoming, client::conn::http1};
use hyper_util::rt::TokioIo;
use parrot_domain::protocol::messages::parrot as wire;
use parrot_host::{grpc::service::GrpcService, web::server::WebServer};
use protobuf::Message;
use std::{net::SocketAddr, sync::atomic::Ordering};

pub(super) async fn request(
    address: SocketAddr,
    method: &str,
    message: &impl Message,
) -> (
    hyper::Response<Incoming>,
    tokio::task::JoinHandle<Result<(), hyper::Error>>,
) {
    let io = bounded(tokio::net::TcpStream::connect(address))
        .await
        .unwrap();
    let (mut sender, connection) = bounded(http1::handshake(TokioIo::new(io))).await.unwrap();
    let task = tokio::spawn(connection);
    let bytes = message.write_to_bytes().unwrap();
    let mut frame = vec![0];
    frame.extend_from_slice(&(u32::try_from(bytes.len()).unwrap()).to_be_bytes());
    frame.extend_from_slice(&bytes);
    let response = bounded(
        sender.send_request(
            Request::builder()
                .method("POST")
                .uri(format!("http://{address}/parrot.v1.Parrot/{method}"))
                .header("content-type", "application/grpc-web+proto")
                .header("x-grpc-web", "1")
                .body(Full::new(Bytes::from(frame)))
                .unwrap(),
        ),
    )
    .await
    .unwrap();
    (response, task)
}

pub(super) fn message<T: Message>(body: &[u8]) -> T {
    assert_eq!(body[0], 0);
    let length = u32::from_be_bytes(body[1..5].try_into().unwrap()) as usize;
    T::parse_from_bytes(&body[5..5 + length]).unwrap()
}

#[tokio::test]
async fn web_stop_joins_http1_subscription_but_borrows_actual_service() {
    let fixture = Fixture::new().await;
    let mut server = bounded(WebServer::start(
        GrpcService::new(fixture.service.clone()),
        NoWeb::default(),
        0,
    ))
    .await
    .unwrap();
    let address = server.address();
    let (response, connection) = request(address, "CreateSession", &create_request()).await;
    let body = bounded(response.into_body().collect())
        .await
        .unwrap()
        .to_bytes();
    let created: wire::UserSession = message(&body);
    bounded(connection).await.unwrap().unwrap();
    assert!(!created.id.is_empty());
    let (mut response, stream_connection) = request(
        address,
        "Listen",
        &wire::ListenRequest {
            user_session_id: created.id.clone(),
            replay: true,
            ..Default::default()
        },
    )
    .await;
    let first = bounded(response.body_mut().frame()).await.unwrap().unwrap();
    assert!(
        !first.into_data().unwrap().is_empty(),
        "actual replay arrives before stream completion"
    );
    bounded(server.close()).await.unwrap();
    while bounded(response.body_mut().frame())
        .await
        .is_some_and(|result| result.is_ok())
    {}
    drop(response);
    let _ = bounded(stream_connection).await.unwrap();
    bounded(server.close()).await.unwrap();
    assert!(
        bounded(tokio::net::TcpStream::connect(address))
            .await
            .is_err()
    );
    assert_eq!(fixture.host.closed.load(Ordering::SeqCst), 0);
    fixture.assert_database_open();
    fixture.assert_live_activation().await;
    bounded(fixture.service.session_status(
        &wire::SessionStatusRequest {
            user_session_id: created.id,
            ..Default::default()
        },
        active().as_ref(),
    ))
    .await
    .unwrap();
    fixture.close().await;
    fixture.close().await;
    assert_eq!(fixture.host.opened.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.host.closed.load(Ordering::SeqCst), 1);
}
