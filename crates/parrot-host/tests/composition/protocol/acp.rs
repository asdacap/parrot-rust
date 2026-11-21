//! The ACP adapter driven by a scripted client against the protocol fixture.
//! The scripted client answers `session/request_permission` by selecting the
//! `allow_once` option, proving the selection reaches the permission broker as
//! a grant and the prompt turn then continues to completion.
use super::service::support::{Approval, Call, active, bounded, until};
use agent_client_protocol::{
    Channel, Client, on_receive_request,
    schema::ProtocolVersion,
    v1::ConnectionTo,
    v1::schema::{
        ContentBlock, InitializeRequest, NewSessionRequest, PermissionOptionKind, PromptRequest,
        RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
        SelectedPermissionOutcome, StopReason, TextContent,
    },
};
use parrot_domain::protocol::messages::parrot as wire;
use parrot_host::acp::AcpAgent;
use parrot_runtime::interactions::{
    PermissionDecision, PermissionRequest, PermissionTargetDescription,
    PermissionTargetDescriptionKind, map_pending_permission,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::oneshot;

#[tokio::test]
async fn acp_permission_selection_grants_and_the_turn_continues() {
    let f = crate::protocol::service::support::Fixture::new();
    let agent = AcpAgent::new(
        f.service.clone(),
        wire::CreateSessionRequest {
            model: "scripted/model".into(),
            ..Default::default()
        },
    );
    let (agent_transport, client_transport) = Channel::duplex();
    let adapter = tokio::spawn(async move { agent.run(agent_transport).await });

    let (session_tx, session_rx) = oneshot::channel::<String>();
    let client = tokio::spawn(
        Client
            .v1()
            .on_receive_request(
                async |request: RequestPermissionRequest, responder, _cx| {
                    let kinds: Vec<PermissionOptionKind> =
                        request.options.iter().map(|option| option.kind).collect();
                    assert_eq!(
                        kinds,
                        vec![
                            PermissionOptionKind::AllowOnce,
                            PermissionOptionKind::AllowAlways,
                            PermissionOptionKind::RejectOnce,
                            PermissionOptionKind::RejectAlways,
                        ]
                    );
                    responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                            "allow_once",
                        )),
                    ))
                },
                on_receive_request!(),
            )
            .connect_with(client_transport, move |cx: ConnectionTo<_>| async move {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let new_session = cx
                    .send_request(NewSessionRequest::new(std::path::PathBuf::from("/")))
                    .block_task()
                    .await?;
                let session_id = new_session.session_id.0.to_string();
                session_tx.send(session_id.clone()).unwrap();
                let prompt = cx
                    .send_request(PromptRequest::new(
                        new_session.session_id.clone(),
                        vec![ContentBlock::Text(TextContent::new(
                            "please update the files",
                        ))],
                    ))
                    .block_task()
                    .await?;
                assert_eq!(prompt.stop_reason, StopReason::EndTurn);
                Ok(())
            }),
    );

    let session_id = bounded(session_rx).await.unwrap();
    let fake = loop {
        let created = f.factory.created.lock().unwrap().get(&session_id).cloned();
        if let Some(fake) = created {
            break fake;
        }
        tokio::task::yield_now().await;
    };
    until(|| {
        fake.calls()
            .iter()
            .any(|call| matches!(call, Call::Send(..)))
    })
    .await;

    let approval = Arc::new(Approval(AtomicBool::new(false)));
    let requesting = {
        let fake = fake.clone();
        let approval = approval.clone();
        tokio::spawn(async move {
            fake.permissions
                .request(
                    PermissionRequest {
                        agent_session_id: fake.root_agent.clone(),
                        reason: "update generated files".into(),
                        targets: vec![PermissionTargetDescription {
                            path: "/work/file.txt".into(),
                            kind: PermissionTargetDescriptionKind::File,
                        }],
                        approval,
                    },
                    active().as_ref(),
                )
                .await
        })
    };

    // The fixture's permission broker publishes no durable events, so the
    // test mirrors the published `permission_pending` event into the live
    // stream exactly as the durable publisher does.
    bounded(async {
        while fake.permissions.pending().await.is_empty() || fake.writers.lock().unwrap().is_empty()
        {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let pending = map_pending_permission(&fake.permissions.pending().await.remove(0), |target| {
        target.clone()
    });
    let mut event = wire::Event::new();
    event.agent_session_id = fake.root_agent.clone();
    event.set_permission_pending(pending);
    let writer = fake.writers.lock().unwrap()[0].clone();
    writer.write(event, active().as_ref()).await.unwrap();

    let decision = bounded(requesting).await.unwrap().unwrap();
    assert_eq!(decision.decision, PermissionDecision::Grant);
    assert!(approval.0.load(Ordering::SeqCst));

    // The permission settled, so the simulated tool runs on and the turn
    // completes; the mode run's terminal event ends the ACP prompt.
    let mut event = wire::Event::new();
    event.agent_session_id = fake.root_agent.clone();
    event.set_mode_turn_completed(wire::ModeTurnCompleted::new());
    writer.write(event, active().as_ref()).await.unwrap();

    bounded(client).await.unwrap().unwrap();
    adapter.abort();
    f.close().await;
}
