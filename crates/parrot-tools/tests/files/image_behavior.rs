use std::{
    collections::BTreeMap,
    io::Read,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use parrot_domain::{
    contracts::{
        async_ops::OperationError,
        filesystem::{EntryKind, FileReadError, FileReadErrorKind, LinkPolicy, ReadRequest},
        paths::{RootOrigin, SelectedSessionRoot},
        security::InvocationContext,
        tools::{ParallelSafety, Tool},
    },
    values::tools::ToolCycleImageBudget,
};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use parrot_storage::{
    artifacts::{ImageArtifactRepository, ImageArtifactStore},
    database::SessionDatabase,
};
use parrot_tools::files::{FileToolInvocation, FileToolOutcome, ImageBudget, ReadImageTool};
use serde_json::json;

use super::fixtures::*;

// ReadImageToolTests.cs Png(), source pin 5f27a8, decoded without re-encoding the image.
const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8, 0xcf, 0xc0, 0xf0,
    0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x89, 0x99, 0x3d, 0x1d, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];
static NEXT_SESSION: AtomicU64 = AtomicU64::new(0);

struct ImageFixture {
    repository: Arc<ImageArtifactRepository>,
    database: SessionDatabase,
    path: PathBuf,
}
impl ImageFixture {
    fn new() -> Self {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.tmp/t55-image-sessions")
            .join(format!(
                "{}-{}",
                std::process::id(),
                NEXT_SESSION.fetch_add(1, Ordering::Relaxed)
            ));
        std::fs::create_dir_all(&path).unwrap();
        let path = std::fs::canonicalize(path).unwrap();
        let root = SelectedSessionRoot::new(path.clone(), RootOrigin::Current).unwrap();
        let database = SessionDatabase::open(root.clone()).unwrap();
        let store = ImageArtifactStore::for_session(&root, "artifacts").unwrap();
        let repository = Arc::new(ImageArtifactRepository::new(database.clone(), store).unwrap());
        Self {
            repository,
            database,
            path,
        }
    }

    fn tool(&self, service: RecordingService, limit: i32) -> ReadImageTool<RecordingService> {
        ReadImageTool::new(service, self.repository.clone(), limit, definition())
    }

    async fn close(self) {
        self.database.close().await.unwrap();
        drop(self.repository);
        drop(self.database);
        std::fs::remove_dir_all(self.path).unwrap();
    }
}

fn templates() -> Arc<PromptTemplateCatalog> {
    let environment = [
        ("HOME", "/tmp/t55/home"),
        ("XDG_CONFIG_HOME", "/tmp/t55/config"),
        ("XDG_STATE_HOME", "/tmp/t55/state"),
        ("XDG_DATA_HOME", "/tmp/t55/data"),
        ("WORKDIR", "/tmp/t55/workspace"),
    ]
    .into_iter()
    .map(|(key, value)| (key.into(), value.into()))
    .collect::<BTreeMap<_, _>>();
    Arc::new(
        PromptTemplateCatalog::from_content("", &environment)
            .unwrap()
            .1,
    )
}

fn budget_invocation(
    call_id: &str,
    arguments: &str,
    budget: &Arc<ToolCycleImageBudget>,
    templates: &Arc<PromptTemplateCatalog>,
) -> FileToolInvocation {
    let mut invocation = FileToolInvocation::new(call_id, arguments);
    invocation.image_budget = Some(Arc::new(ImageBudget::new(
        budget.clone(),
        templates.clone(),
    )));
    invocation
}

#[tokio::test]
async fn image_result_uses_bound_actual_filename_and_real_repository_preserves_original_bytes_and_first_labels()
 {
    let fixture = ImageFixture::new();
    let service = RecordingService::default();
    service.file("alias.dat", "/actual/pixel.png", PNG.to_vec());
    service.file("renamed.png", "/other/renamed.png", PNG.to_vec());
    let tool = fixture.tool(service.clone(), PNG.len() as i32);
    let original = tool
        .invoke(
            FileToolInvocation::new("original-call", r#"{"path":"alias.dat"}"#),
            InvocationContext::new(Snapshot(71), (), &NoCancel),
        )
        .await
        .unwrap();
    assert_eq!(original.text, "image read");
    assert_eq!(original.outcome, FileToolOutcome::Finished);
    assert_eq!(original.image_artifacts.len(), 1);
    let artifact = &original.image_artifacts[0];
    assert_eq!(artifact.display_name, "pixel.png");
    assert_eq!(artifact.origin, "read_image");
    assert_eq!(artifact.media_type, "image/png");
    assert_eq!(artifact.byte_length, PNG.len() as i64);
    assert_eq!(
        (
            artifact.width,
            artifact.height,
            artifact.frame_count,
            artifact.aggregate_pixels
        ),
        (1, 1, 1, 1)
    );
    let renamed = tool
        .invoke(
            FileToolInvocation::new("renamed-call", r#"{"path":"renamed.png"}"#),
            InvocationContext::new(Snapshot(72), (), &NoCancel),
        )
        .await
        .unwrap();
    assert_eq!(renamed.image_artifacts, original.image_artifacts);
    assert_eq!(
        fixture
            .repository
            .resolve(&artifact.artifact_id, &NoCancel)
            .await
            .unwrap()
            .as_ref(),
        Some(artifact)
    );
    let expected_path = fixture
        .path
        .join("artifacts")
        .join(format!("{}.png", artifact.artifact_id));
    assert_eq!(
        fixture
            .repository
            .store()
            .path_of(&artifact.artifact_id)
            .unwrap(),
        expected_path
    );
    assert_eq!(std::fs::read(&expected_path).unwrap(), PNG);
    let mut stored = Vec::new();
    fixture
        .repository
        .open(&artifact.artifact_id, &NoCancel)
        .await
        .unwrap()
        .read_to_end(&mut stored)
        .unwrap();
    assert_eq!(stored, PNG);
    assert_eq!(
        std::fs::read_dir(fixture.repository.store().directory())
            .unwrap()
            .count(),
        1
    );
    let calls = service.calls();
    assert_shared_snapshot(&calls[..2], 71);
    assert_shared_snapshot(&calls[2..], 72);
    assert_eq!(
        calls[0].action,
        Action::Inspect("alias.dat".into(), LinkPolicy::Follow)
    );
    assert_eq!(
        calls[1].action,
        Action::Read(ReadRequest {
            requested: "alias.dat".into(),
            expected_physical: Some("/actual/pixel.png".into()),
            links: LinkPolicy::Follow,
            max_bytes: Some(PNG.len())
        })
    );
    assert_eq!(
        tool.parallel_safety(&invocation(json!({}))),
        ParallelSafety::ParallelSafe
    );
    drop(tool);
    fixture.close().await;
}

#[tokio::test]
async fn image_per_read_limit_is_inclusive_and_is_passed_to_service_with_post_read_guard() {
    for (limit, ignore_service_limit, expected_count) in [
        (PNG.len() as i32, false, 1),
        (PNG.len() as i32 - 1, false, 0),
        (PNG.len() as i32 - 1, true, 0),
        (-1, false, 0),
    ] {
        let fixture = ImageFixture::new();
        let service = RecordingService::default();
        service.file("pixel.png", "/actual/pixel.png", PNG.to_vec());
        service.0.lock().unwrap().ignore_read_limit = ignore_service_limit;
        let tool = fixture.tool(service.clone(), limit);
        let result = tool
            .execute(
                &invocation(json!({"path":"pixel.png"})),
                &Snapshot(73),
                &NoCancel,
            )
            .await
            .unwrap();
        assert_eq!(result.image_artifacts.len(), expected_count);
        assert_eq!(
            result.text,
            if expected_count == 1 {
                "image read".into()
            } else {
                format!("error: An image cannot exceed {limit} bytes.")
            }
        );
        assert_eq!(
            service.calls()[1].action,
            Action::Read(ReadRequest {
                requested: "pixel.png".into(),
                expected_physical: Some("/actual/pixel.png".into()),
                links: LinkPolicy::Follow,
                max_bytes: Some(limit.max(0) as usize)
            })
        );
        if expected_count == 0 {
            assert!(!fixture.repository.store().directory().exists());
        }
        drop(tool);
        fixture.close().await;
    }
}

#[tokio::test]
async fn image_invalid_arguments_and_non_regular_entries_never_read_or_persist() {
    let fixture = ImageFixture::new();
    let service = RecordingService::default();
    let tool = fixture.tool(service.clone(), 1024);
    for arguments in [
        "{",
        "null",
        "[]",
        "{}",
        r#"{"path":null}"#,
        r#"{"path":1}"#,
        r#"{"path":"x","extra":true}"#,
    ] {
        let result = tool
            .execute(
                &FileToolInvocation::new("invalid", arguments),
                &Snapshot(1),
                &NoCancel,
            )
            .await
            .unwrap();
        assert!(result.text.starts_with("error: "));
    }
    assert!(service.calls().is_empty());
    for kind in [EntryKind::Directory, EntryKind::Other] {
        service
            .0
            .lock()
            .unwrap()
            .metadata
            .insert("x".into(), metadata("x", "x", kind));
        let result = tool
            .execute(&invocation(json!({"path":"x"})), &Snapshot(1), &NoCancel)
            .await
            .unwrap();
        assert_eq!(result.text, "error: no such file or directory");
    }
    assert_eq!(service.calls().len(), 2);
    assert!(
        service
            .calls()
            .iter()
            .all(|call| matches!(call.action, Action::Inspect(_, _)))
    );
    assert!(!fixture.repository.store().directory().exists());
    drop(tool);
    fixture.close().await;
}

#[tokio::test]
async fn image_rejected_resolution_returns_source_error_and_no_artifact_without_opening() {
    let fixture = ImageFixture::new();
    let service = RecordingService::default();
    service.0.lock().unwrap().inspect_errors.insert(
        "alias.png".into(),
        FileReadError::new(FileReadErrorKind::InvalidOperation, "access denied"),
    );
    let tool = fixture.tool(service.clone(), 1024);
    let result = tool
        .execute(
            &invocation(json!({"path":"alias.png"})),
            &Snapshot(1),
            &NoCancel,
        )
        .await
        .unwrap();
    assert_eq!(result.text, "error: access denied");
    assert!(result.image_artifacts.is_empty());
    assert_eq!(service.calls().len(), 1);
    assert!(
        matches!(&service.calls()[0].action, Action::Inspect(path, LinkPolicy::Follow) if path == std::path::Path::new("alias.png"))
    );
    assert!(!fixture.repository.store().directory().exists());
    drop(tool);
    fixture.close().await;
}

#[tokio::test]
async fn image_second_service_authorization_failure_does_not_persist_or_charge_budget() {
    let fixture = ImageFixture::new();
    let service = RecordingService::default();
    service.file("alias.png", "/actual/pixel.png", PNG.to_vec());
    service.0.lock().unwrap().reject_read = Some(FileReadError::new(
        FileReadErrorKind::Denied,
        "second authorization denied",
    ));
    let tool = fixture.tool(service.clone(), 1024);
    let budget = Arc::new(ToolCycleImageBudget::new(PNG.len() as i32));
    let templates = templates();
    let result = tool
        .invoke(
            budget_invocation("denied", r#"{"path":"alias.png"}"#, &budget, &templates),
            InvocationContext::new(Snapshot(74), (), &NoCancel),
        )
        .await
        .unwrap();
    assert_eq!(result.text, "error: second authorization denied");
    assert!(result.image_artifacts.is_empty());
    assert_eq!(result.outcome, FileToolOutcome::Finished);
    assert_shared_snapshot(&service.calls(), 74);
    assert_eq!(service.calls().len(), 2);
    assert!(!fixture.repository.store().directory().exists());
    service.0.lock().unwrap().reject_read = None;
    let retry = tool
        .execute(
            &budget_invocation("retry", r#"{"path":"alias.png"}"#, &budget, &templates),
            &Snapshot(1),
            &NoCancel,
        )
        .await
        .unwrap();
    assert_eq!(retry.image_artifacts.len(), 1);
    assert!(!budget.is_exceeded());
    drop(tool);
    fixture.close().await;
}

#[tokio::test]
async fn image_original_byte_budget_charges_deduplicated_reads_latches_before_parse_and_resets_by_new_cycle()
 {
    for extra_bytes in [0, 1] {
        let fixture = ImageFixture::new();
        let service = RecordingService::default();
        service.file("pixel.png", "/actual/pixel.png", PNG.to_vec());
        service.file(
            "invalid.txt",
            "/actual/invalid.txt",
            b"not an image".to_vec(),
        );
        let tool = fixture.tool(service.clone(), 1024);
        let budget = Arc::new(ToolCycleImageBudget::new(
            (PNG.len() * 2) as i32 + extra_bytes,
        ));
        let templates = templates();
        let invalid = tool
            .execute(
                &budget_invocation("invalid", r#"{"path":"invalid.txt"}"#, &budget, &templates),
                &Snapshot(1),
                &NoCancel,
            )
            .await
            .unwrap();
        assert!(
            invalid
                .text
                .starts_with("error: The image content is invalid or unsupported.")
        );
        assert!(invalid.image_artifacts.is_empty());
        assert!(!budget.is_exceeded());
        for index in 0..4 {
            let calls_before = service.calls().len();
            let result = tool
                .execute(
                    &budget_invocation(
                        &format!("read-{index}"),
                        r#"{"path":"pixel.png"}"#,
                        &budget,
                        &templates,
                    ),
                    &Snapshot(1),
                    &NoCancel,
                )
                .await
                .unwrap();
            assert_eq!(result.image_artifacts.len(), usize::from(index < 2));
            assert_eq!(
                result.outcome,
                if index < 2 {
                    FileToolOutcome::Finished
                } else {
                    FileToolOutcome::ImageBudgetExceeded
                }
            );
            assert_eq!(
                service.calls().len() - calls_before,
                if index < 3 { 2 } else { 0 }
            );
        }
        let calls_before = service.calls().len();
        let latched = tool
            .execute(
                &budget_invocation("latched", "invalid json", &budget, &templates),
                &Snapshot(1),
                &NoCancel,
            )
            .await
            .unwrap();
        let expected_failure = budget
            .describe_failure(|id, arguments| {
                templates.render(
                    id,
                    &arguments
                        .iter()
                        .map(|(key, value)| ((*key).into(), json!(value)))
                        .collect::<Vec<_>>(),
                    &NoCancel,
                )
            })
            .unwrap();
        assert_eq!(latched.text, expected_failure);
        assert_eq!(latched.outcome, FileToolOutcome::ImageBudgetExceeded);
        assert!(latched.image_artifacts.is_empty());
        assert_eq!(service.calls().len(), calls_before);
        let next_budget = Arc::new(ToolCycleImageBudget::new(PNG.len() as i32));
        let next = tool
            .execute(
                &budget_invocation(
                    "next-cycle",
                    r#"{"path":"pixel.png"}"#,
                    &next_budget,
                    &templates,
                ),
                &Snapshot(1),
                &NoCancel,
            )
            .await
            .unwrap();
        assert_eq!(next.image_artifacts.len(), 1);
        assert_eq!(next.outcome, FileToolOutcome::Finished);
        assert_eq!(
            std::fs::read(
                fixture
                    .repository
                    .store()
                    .path_of(&next.image_artifacts[0].artifact_id)
                    .unwrap()
            )
            .unwrap(),
            PNG
        );
        assert_eq!(
            std::fs::read_dir(fixture.repository.store().directory())
                .unwrap()
                .count(),
            1
        );
        drop(tool);
        fixture.close().await;
    }
}

#[tokio::test]
async fn image_preexceeded_budget_short_circuits_invalid_json_and_all_service_calls() {
    let fixture = ImageFixture::new();
    let service = RecordingService::default();
    let tool = fixture.tool(service.clone(), 1024);
    let budget = Arc::new(ToolCycleImageBudget::new(0));
    budget.restore_exceeded();
    let templates = templates();
    let result = tool
        .execute(
            &budget_invocation("preexceeded", "invalid json", &budget, &templates),
            &Snapshot(1),
            &NoCancel,
        )
        .await
        .unwrap();
    assert_eq!(result.outcome, FileToolOutcome::ImageBudgetExceeded);
    assert!(result.image_artifacts.is_empty());
    let cancelled = tool
        .execute(
            &budget_invocation("preexceeded-cancelled", "invalid json", &budget, &templates),
            &Snapshot(1),
            &AlreadyCancelled,
        )
        .await
        .unwrap();
    assert_eq!(cancelled, result);
    let mut malformed = FileToolInvocation::new("malformed-cancelled", "null");
    malformed.prompt_templates = Some(templates.clone());
    let invalid = tool
        .execute(&malformed, &Snapshot(1), &AlreadyCancelled)
        .await
        .unwrap();
    assert_eq!(invalid.text, "error: Tool arguments must be an object.");
    assert!(service.calls().is_empty());
    assert!(!fixture.repository.store().directory().exists());
    drop(tool);
    fixture.close().await;
}

#[tokio::test]
async fn image_service_cancellation_is_not_formatted_as_tool_error_or_persisted() {
    let fixture = ImageFixture::new();
    let service = RecordingService::default();
    service.file("pixel.png", "pixel.png", PNG.to_vec());
    let tool = fixture.tool(service.clone(), 1024);
    assert_eq!(
        tool.execute(
            &invocation(json!({"path":"pixel.png"})),
            &Snapshot(1),
            &AlreadyCancelled
        )
        .await,
        Err(OperationError::Cancelled)
    );
    assert_eq!(service.calls().len(), 1);
    assert!(!fixture.repository.store().directory().exists());
    drop(tool);
    fixture.close().await;
}
