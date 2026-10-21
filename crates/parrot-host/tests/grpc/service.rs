//! Adapter delegation through the new real-composition fixture. External provider,
//! process, network and filesystem capabilities are denied by that fixture; this
//! does not attest production composition/runtime ownership (the T65 obligation).
//! The all-method matrix mixes successful and rejected requests; it is not proof
//! of every method's successful network path.

use crate::support::authoritative::{Fixture, active, bounded, create_request};
use bytes::Bytes;
use http_body_util::Full;
use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation},
    protocol::{
        bindings::parrot::parrot_server::{Parrot, ParrotServer},
        codec::RustProtobufCodec,
        messages::parrot as wire,
    },
};
use parrot_host::{
    composition::protocol::{self, ParrotService, UploadStream},
    grpc::service::GrpcService,
};
use std::sync::Arc;
use tonic::{Request, Response, Status, codec::Codec};

#[test]
fn borrowed_service_adapter_is_cloneable_and_implements_the_complete_generated_trait() {
    fn complete_service<T: Parrot + Clone + Send + Sync + 'static>() {}
    complete_service::<GrpcService>();
    let constructor: fn(Arc<ParrotService>) -> GrpcService = GrpcService::new;
    let binding: fn(GrpcService) -> ParrotServer<GrpcService> = ParrotServer::new;
    let _ = (constructor, binding);
}

fn same<T: std::fmt::Debug + PartialEq>(
    method: &str,
    expected: protocol::Result<T>,
    actual: Result<Response<T>, Status>,
) {
    match (expected, actual) {
        (Ok(expected), Ok(actual)) => assert_eq!(actual.into_inner(), expected, "{method}"),
        (Err(expected), Err(actual)) => {
            assert_eq!(actual.code() as i32, expected.code as i32, "{method}");
            assert_eq!(actual.message(), expected.message, "{method}");
        }
        (expected, actual) => panic!("{method}: service {expected:?}, transport {actual:?}"),
    }
}

/// The source host registers UnhandledFailureInterceptor: ordinary handler
/// failures become Internal with the original exception message. This asserts
/// that explicit pinned contract, not an inferred default framework behavior.
#[tokio::test]
async fn set_context_limit_persistence_fault_requires_server_exception_projection() {
    use parrot_host::grpc::{
        address::TransportAddress,
        token::TransportToken,
        transport::{GrpcServer, GrpcTransportClient},
    };
    bounded(async {
        let fixture = Fixture::new().await;
        let token = TransportToken::generate().unwrap();
        let mut server = GrpcServer::start(
            GrpcService::new(fixture.service.clone()),
            TransportAddress::parse("http://127.0.0.1:0").unwrap(),
            Some(token.clone()),
            None,
            fixture.diagnostics.clone(),
        ).await.unwrap();
        let mut client = GrpcTransportClient::connect(
            server.address(), Some(&token), None, fixture.diagnostics.clone(),
        ).unwrap();
        let session = client.client().unwrap().create_session(create_request()).await.unwrap().into_inner();
        let workspace = std::path::PathBuf::from(&session.working_directory);
        let configuration = workspace.parent().unwrap().join("config.yaml");
        assert!(configuration.is_file(), "the returned fixture workspace identifies its real configuration file");
        std::fs::remove_file(&configuration).unwrap();
        std::fs::create_dir(&configuration).unwrap();
        // Capture this platform's exact failed atomic-replacement message, not
        // an OS-specific literal or a message obtained from the RPC under test.
        let probe = workspace.parent().unwrap().join("persistence-message-probe");
        std::fs::write(&probe, b"probe").unwrap();
        let expected_message = std::fs::rename(&probe, &configuration).unwrap_err().to_string();
        std::fs::remove_file(probe).unwrap();
        let failure = client.client().unwrap().set_context_limit(wire::SetContextLimitRequest {
            user_session_id: session.id.clone(),
            context_limit: "100k".into(),
            ..Default::default()
        }).await.unwrap_err();
        // SetModelPreset explicitly translates persistence IO to an RpcError;
        // unlike the unexpected SetContextLimit origin, it must not be logged.
        let explicit_rpc = client.client().unwrap().set_model_preset(wire::SetModelPresetRequest {
            user_session_id: session.id.clone(),
            name: "fixture-preset".into(),
            ..Default::default()
        }).await.unwrap_err();
        fixture.gate.arm();
        let mut request = Request::new(wire::ListSkillsRequest {
            user_session_id: session.id,
            ..Default::default()
        });
        request.set_timeout(std::time::Duration::from_millis(100));
        let cancelled = {
            let call = client.client().unwrap().list_skills(request);
            tokio::pin!(call);
            tokio::select! {
                _ = fixture.gate.entered.notified() => {},
                result = &mut call => panic!("refresh settled before cancellation: {result:?}"),
            }
            call.await.unwrap_err()
        };
        fixture.gate.release.notify_one();
        // Settle all borrowed owners even when the final source predicate fails.
        client.close();
        server.close().await.unwrap();
        fixture.close().await;
        assert_eq!(failure.code(), tonic::Code::Internal,
            "pinned UnhandledFailureInterceptor translates configuration IO failure to Internal; actual native message: {}", failure.message());
        assert_eq!(failure.message(), expected_message);
        assert_eq!(explicit_rpc.code(), tonic::Code::Internal);
        assert!(matches!(cancelled.code(), tonic::Code::Cancelled | tonic::Code::DeadlineExceeded));
        let log = std::fs::read_to_string(fixture.diagnostics.path()).unwrap();
        let failures = log.lines().filter(|line| line.contains("event=\"handler.failure\"")).collect::<Vec<_>>();
        assert_eq!(failures.len(), 1, "only the unexpected typed IO origin is logged; explicit RpcError Internal and cancellation are excluded");
        let event = failures[0];
        assert!(event.contains("ERROR category=\"transport\""));
        assert!(event.contains("request=\"/parrot.v1.Parrot/SetContextLimit\""));
        assert!(event.contains("error=\"io\""));
        assert!(event.contains("outcome=\"failure\""));
        assert!(!event.contains(&expected_message), "diagnostics must not contain the IO exception text");
        assert!(!event.contains("config.yaml"));
    }).await;
}

struct EmptyUpload;
impl UploadStream for EmptyUpload {
    fn next<'a>(
        &'a mut self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, protocol::Result<Option<wire::AttachmentUploadFrame>>> {
        Box::pin(async { Ok(None) })
    }
}

#[tokio::test]
async fn all_31_generated_rpcs_delegate_to_the_same_actual_service() {
    bounded(async {
        let fixture = Fixture::new().await;
        let rpc = GrpcService::new(fixture.service.clone());
        let cancellation = active();
        let created = rpc.create_session(Request::new(create_request())).await.unwrap().into_inner();
        assert_eq!(fixture.service.sessions().find(&created.id).unwrap().snapshot(), created);
        let mut calls = 1;
        macro_rules! verify {
            ($method:ident, $request:expr, $value:ident => $expected:expr) => {{
                let $value = $request;
                let expected = $expected;
                let actual = rpc.$method(Request::new($value)).await;
                same(stringify!($method), expected, actual);
                calls += 1;
            }};
        }
        verify!(list_models, wire::ListModelsRequest::new(), value => fixture.service.list_models(&value, cancellation.as_ref()).await);
        verify!(list_model_aliases, wire::ListModelAliasesRequest::new(), value => Ok(fixture.service.list_model_aliases(&value)));
        verify!(configure_model_alias, wire::ConfigureModelAliasRequest { name: "undefined-alias".into(), model_string: "local/model".into(), ..Default::default() }, value => fixture.service.configure_model_alias(&value));
        verify!(list_provider_model_alias_defaults, wire::ListProviderModelAliasDefaultsRequest::new(), value => fixture.service.list_provider_model_alias_defaults(&value, cancellation.as_ref()).await);
        verify!(apply_provider_model_alias_defaults, wire::ApplyProviderModelAliasDefaultsRequest { provider_id: "absent-provider".into(), ..Default::default() }, value => fixture.service.apply_provider_model_alias_defaults(&value, cancellation.as_ref()).await);
        verify!(set_model_preset, wire::SetModelPresetRequest { user_session_id: created.id.clone(), ..Default::default() }, value => fixture.service.set_model_preset(&value, cancellation.as_ref()));
        verify!(select_model_preset, wire::SelectModelPresetRequest { user_session_id: created.id.clone(), name: "undefined-preset".into(), ..Default::default() }, value => fixture.service.select_model_preset(&value, cancellation.as_ref()));
        verify!(list_model_presets, wire::ListModelPresetsRequest::new(), value => fixture.service.list_model_presets(&value, cancellation.as_ref()));
        verify!(list_modes, wire::ListModesRequest::new(), value => Ok(fixture.service.list_modes(&value)));
        verify!(list_skills, wire::ListSkillsRequest { user_session_id: created.id.clone(), ..Default::default() }, value => fixture.service.list_skills(&value, cancellation.as_ref()).await);
        verify!(configure_skill, wire::ConfigureSkillRequest { user_session_id: created.id.clone(), path: "/undefined-fixture-skill".into(), enabled: true, ..Default::default() }, value => fixture.service.configure_skill(&value, cancellation.as_ref()).await);
        verify!(list_sessions, wire::ListSessionsRequest::new(), value => fixture.service.list_sessions(&value));
        verify!(resume_session, wire::ResumeSessionRequest { user_session_id: "missing".into(), ..Default::default() }, value => fixture.service.resume_session(&value, cancellation.clone()).await);
        verify!(attach_session, wire::AttachSessionRequest { user_session_id: created.id.clone(), working_directory: created.working_directory.clone(), ..Default::default() }, value => fixture.service.attach_session(&value, cancellation.as_ref()));
        verify!(update_session, wire::UpdateSessionRequest { user_session_id: created.id.clone(), mode: "undefined-mode".into(), ..Default::default() }, value => fixture.service.update_session(&value).await);
        verify!(set_goal, wire::SetGoalRequest { user_session_id: created.id.clone(), ..Default::default() }, value => fixture.service.set_goal(&value, cancellation.clone()).await);
        verify!(set_standing_instruction, wire::SetStandingInstructionRequest { user_session_id: created.id.clone(), instruction: "native adapter fixture".into(), ..Default::default() }, value => fixture.service.set_standing_instruction(&value));
        verify!(send_message, wire::SendMessageRequest { user_session_id: created.id.clone(), ..Default::default() }, value => fixture.service.send_message(&value, cancellation.clone()).await);
        let expected = fixture.service.upload_attachment(&mut EmptyUpload, cancellation.as_ref()).await;
        let mut codec = RustProtobufCodec::<wire::AttachmentUploadFrame, wire::AttachmentUploadFrame>::default();
        let upload = tonic::Streaming::new_request(codec.decoder(), Full::new(Bytes::new()), None, None);
        same("upload_attachment", expected, rpc.upload_attachment(Request::new(upload)).await);
        calls += 1;
        verify!(interrupt, wire::InterruptRequest { user_session_id: "missing".into(), ..Default::default() }, value => fixture.service.interrupt(&value, cancellation.clone()).await);
        verify!(compact, wire::CompactRequest { user_session_id: created.id.clone(), target_context_size: "not-a-context-size".into(), ..Default::default() }, value => fixture.service.compact(&value, cancellation.clone()).await);
        verify!(set_context_limit, wire::SetContextLimitRequest { user_session_id: created.id.clone(), context_limit: "100k".into(), ..Default::default() }, value => {
            let expected = fixture.service.set_context_limit(&value, cancellation.as_ref());
            assert_eq!(expected.as_ref().unwrap().context_limit, "100000");
            expected
        });
        verify!(sandbox_enable, wire::SandboxEnableRequest { user_session_id: created.id.clone(), enabled: true, ..Default::default() }, value => fixture.service.sandbox_enable(&value, cancellation.as_ref()));
        verify!(session_status, wire::SessionStatusRequest { user_session_id: created.id.clone(), ..Default::default() }, value => fixture.service.session_status(&value, cancellation.as_ref()).await);
        verify!(list_pending_questions, wire::ListPendingQuestionsRequest { user_session_id: created.id.clone(), ..Default::default() }, value => fixture.service.list_pending_questions(&value));
        verify!(reply_question, wire::ReplyQuestionRequest { user_session_id: created.id.clone(), question_request_id: "missing-question".into(), ..Default::default() }, value => fixture.service.reply_question(&value));
        verify!(reject_question, wire::RejectQuestionRequest { user_session_id: created.id.clone(), question_request_id: "missing-question".into(), ..Default::default() }, value => fixture.service.reject_question(&value));
        verify!(list_pending_permissions, wire::ListPendingPermissionsRequest { user_session_id: created.id.clone(), ..Default::default() }, value => fixture.service.list_pending_permissions(&value).await);
        verify!(reply_permission, wire::ReplyPermissionRequest { user_session_id: created.id.clone(), permission_request_id: "missing-permission".into(), choice_value: "allow".into(), ..Default::default() }, value => fixture.service.reply_permission(&value).await);
        let request = wire::ListenRequest { user_session_id: created.id, ..Default::default() };
        let direct = fixture.service.listen(&request, cancellation).unwrap();
        let native = rpc.listen(Request::new(request)).await.unwrap().into_inner();
        calls += 1;
        drop((direct, native));
        assert_eq!(calls, 31, "every generated RPC must be exercised");
        fixture.assert_database_open();
        fixture.close().await;
        fixture.assert_database_closed();
    }).await;
}
