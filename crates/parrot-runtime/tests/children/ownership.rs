use super::{harness::Harness, support::*};
use std::sync::Arc;

#[tokio::test]
async fn completed_construction_and_shutdown_barriers_do_not_retain_scope_outputs() {
    bounded(async {
        let h = Harness::new();
        let root = h.root("root").await;
        let child = h.spawn(&root, "child").await;
        let root_weak = Arc::downgrade(&root);
        let child_weak = Arc::downgrade(&child);
        let factory = h.factory.clone();
        h.close().await;
        drop(child);
        drop(root);
        drop(h);
        assert!(root_weak.upgrade().is_none());
        assert!(child_weak.upgrade().is_none());
        assert_eq!(
            factory.built.lock().unwrap().len(),
            2,
            "the test retains real adapters, never fake scope lifetimes"
        );
    })
    .await;
}
