//! Borrowed transport ownership against real composition/storage.
//! This harness owns its Runtime through every explicit service close/join;
//! application construction and production Runtime ownership remain T65 work.
use crate::support::authoritative::{Fixture, active, bounded, create_request};
use parrot_domain::protocol::{bindings::parrot::parrot_server::Parrot, messages::parrot as wire};
use parrot_host::grpc::{
    address::TransportAddress,
    service::GrpcService,
    token::TransportToken,
    transport::{GrpcServer, GrpcTransportClient},
};
use std::{sync::atomic::Ordering, time::Duration};

async fn transport_borrows_service(address: TransportAddress) {
    let fixture = Fixture::new().await;
    let token = address
        .is_tcp()
        .then(|| TransportToken::generate().unwrap());
    let mut server = bounded(GrpcServer::start(
        GrpcService::new(fixture.service.clone()),
        address,
        token.clone(),
        None,
        fixture.diagnostics.clone(),
    ))
    .await
    .unwrap();
    let address = server.address().clone();
    let mut client =
        GrpcTransportClient::connect(&address, token.as_ref(), None, fixture.diagnostics.clone())
            .unwrap();
    let session = bounded(client.client().unwrap().create_session(create_request()))
        .await
        .unwrap()
        .into_inner();
    let mut stream = bounded(client.client().unwrap().listen(wire::ListenRequest {
        user_session_id: session.id.clone(),
        replay: true,
        ..Default::default()
    }))
    .await
    .unwrap()
    .into_inner();
    // A separate client's disposal is local; the owner still accepts requests.
    let mut disposed =
        GrpcTransportClient::connect(&address, token.as_ref(), None, fixture.diagnostics.clone())
            .unwrap();
    bounded(
        disposed
            .client()
            .unwrap()
            .list_modes(wire::ListModesRequest::new()),
    )
    .await
    .unwrap();
    disposed.close();
    bounded(
        client
            .client()
            .unwrap()
            .session_status(wire::SessionStatusRequest {
                user_session_id: session.id.clone(),
                ..Default::default()
            }),
    )
    .await
    .unwrap();
    bounded(server.close()).await.unwrap();
    // Drain already buffered events, then observe actual network termination.
    bounded(async { while let Ok(Some(_)) = stream.message().await {} }).await;
    bounded(server.close()).await.unwrap();
    if let Some(path) = address.unix_path() {
        assert!(!path.exists(), "transport close removes its owned socket");
    } else {
        assert!(
            bounded(tokio::net::TcpStream::connect(
                address.socket_addr().unwrap()
            ))
            .await
            .is_err(),
            "joined server no longer accepts TCP connections"
        );
    }
    assert_eq!(fixture.host.closed.load(Ordering::SeqCst), 0);
    fixture.assert_database_open();
    fixture.assert_live_activation().await;
    bounded(fixture.service.session_status(
        &wire::SessionStatusRequest {
            user_session_id: session.id,
            ..Default::default()
        },
        active().as_ref(),
    ))
    .await
    .unwrap();
    // Only the caller/composition owner closes the borrowed service.
    client.close();
    fixture.close().await;
    fixture.close().await;
    assert_eq!(fixture.host.opened.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.host.closed.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn tcp_stop_joins_network_stream_but_borrows_real_service() {
    transport_borrows_service(TransportAddress::parse("http://127.0.0.1:0").unwrap()).await;
}

#[cfg(unix)]
#[tokio::test]
async fn unix_stop_joins_stream_cleans_owned_socket_but_borrows_real_service() {
    // Keep sockaddr_un below its path limit, independently of the harness TMPDIR.
    let directory = std::path::PathBuf::from("/tmp").join(format!(
        "t63-{}",
        parrot_domain::values::identifiers::Identifier::event_id().unwrap()
    ));
    transport_borrows_service(TransportAddress::Unix(directory.join("grpc.sock"))).await;
    std::fs::remove_dir(directory).unwrap();
}

#[test]
fn client_runtime_loss_does_not_settle_actual_refresh_cleanup_or_service_close() {
    let owner = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    owner.block_on(async {
        let fixture = Fixture::new().await;
        let token = TransportToken::generate().unwrap();
        let mut server = bounded(GrpcServer::start(GrpcService::new(fixture.service.clone()),
            TransportAddress::parse("http://127.0.0.1:0").unwrap(), Some(token.clone()), None,
            fixture.diagnostics.clone())).await.unwrap();
        let address = server.address().clone();
        let mut client = GrpcTransportClient::connect(&address, Some(&token), None, fixture.diagnostics.clone()).unwrap();
        let session = bounded(client.client().unwrap().create_session(create_request())).await.unwrap().into_inner();
        client.close();
        fixture.gate.arm();
        let gate = fixture.gate.clone();
        let diagnostics = fixture.diagnostics.clone();
        let (send, receive) = tokio::sync::oneshot::channel();
        let observer = std::thread::spawn(move || {
            let caller = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            caller.block_on(async {
                let mut client = GrpcTransportClient::connect(&address, Some(&token), None, diagnostics).unwrap();
                let request = wire::ListSkillsRequest { user_session_id: session.id, ..Default::default() };
                tokio::select! {
                    result = client.client().unwrap().list_skills(request) => panic!("refresh unexpectedly settled: {result:?}"),
                    _ = gate.entered.notified() => {}
                }
                client.close();
            });
            drop(caller);
            send.send(()).unwrap();
        });
        bounded(receive).await.unwrap();
        observer.join().unwrap();
        bounded(server.close()).await.unwrap();
        assert!(!fixture.gate.finished.load(Ordering::SeqCst));
        fixture.assert_database_open();
        fixture.assert_live_activation().await;
        let closing = fixture.service.close();
        tokio::pin!(closing);
        assert!(tokio::time::timeout(Duration::from_millis(25), &mut closing).await.is_err());
        assert!(!fixture.gate.finished.load(Ordering::SeqCst));
        fixture.assert_database_open();
        fixture.assert_live_activation().await;
        fixture.gate.release.notify_one();
        bounded(closing).await.unwrap();
        assert!(fixture.gate.finished.load(Ordering::SeqCst));
        fixture.assert_database_closed();
        assert_eq!(fixture.host.opened.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.host.closed.load(Ordering::SeqCst), 1);
        fixture.close().await;
        assert_eq!(fixture.host.closed.load(Ordering::SeqCst), 1);
    });
    drop(owner);
}

#[tokio::test]
async fn failed_listener_admission_closes_actual_partial_session_before_error_and_repeated_close() {
    let fixture = Fixture::new().await;
    fixture.host.fail_next.store(true, Ordering::SeqCst);
    let adapter = GrpcService::new(fixture.service.clone());
    let error = bounded(adapter.create_session(tonic::Request::new(create_request())))
        .await
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::Internal);
    fixture.assert_database_closed();
    assert_eq!(fixture.host.opened.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.host.closed.load(Ordering::SeqCst), 0);
    // A conclusively cleaned failure does not poison subsequent admissions.
    let session = bounded(fixture.service.create_session(&create_request(), active()))
        .await
        .unwrap();
    assert!(!session.id.is_empty());
    fixture.assert_database_open();
    fixture.close().await;
    fixture.close().await;
    assert_eq!(fixture.host.opened.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.host.closed.load(Ordering::SeqCst), 1);
}
