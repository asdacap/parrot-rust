mod agent;
mod compaction;
mod embedded_metadata;
mod forks;
mod reads;
mod reload_cancellation;
mod status_assembly;
mod upstream_remaining;
mod upstream_statistics_history;

use parrot_domain::contracts::paths::{RootOrigin, SelectedSessionRoot};
use parrot_storage::conversation::ConversationRepository;
use parrot_storage::database::SessionDatabase;
use parrot_storage::events::EventRepository;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf, parrot_storage::history_files::SessionIndex);
impl Fixture {
    fn image(
        artifact_id: impl Into<String>,
        media_type: impl Into<String>,
        display_name: impl Into<String>,
    ) -> parrot_storage::conversation::ConversationPart {
        let artifact_id = artifact_id.into();
        parrot_storage::conversation::ConversationPart::image(
            parrot_storage::artifacts::ImageArtifactMetadata {
                sha256: artifact_id.clone(),
                artifact_id,
                media_type: media_type.into(),
                display_name: display_name.into(),
                byte_length: 70,
                width: 24,
                height: 12,
                frame_count: 3,
                aggregate_pixels: 864,
                origin: "test".into(),
            },
        )
    }
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "parrot-t18-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        let root = SelectedSessionRoot::new(path.clone(), RootOrigin::Current).unwrap();
        let id = parrot_domain::values::identifiers::UserSessionId::parse(
            path.file_name().unwrap().to_string_lossy().into_owned(),
        )
        .unwrap();
        let metadata =
            parrot_storage::history_files::SessionIndex::new(root, id.clone(), &path).unwrap();
        let mut initial = parrot_storage::history_files::SessionMeta::new(
            id.value(),
            path.to_string_lossy(),
            "provider",
            "model",
        );
        initial.root_agent_name = Some("main".into());
        initial.mode = Some("initial".into());
        metadata.initialize(initial, "agent".into()).unwrap();
        Self(path, metadata)
    }
    fn repository(&self) -> ConversationRepository {
        let root = SelectedSessionRoot::new(self.0.clone(), RootOrigin::Current).unwrap();
        ConversationRepository::new(
            EventRepository::new(SessionDatabase::open(root).unwrap()),
            self.1.clone(),
        )
        .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
