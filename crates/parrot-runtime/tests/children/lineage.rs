use super::{harness::Harness, support::*};
use parrot_runtime::agent_turn::services::SelectionSource;
use parrot_runtime::children::*;

#[tokio::test]
async fn grandchild_turn_restricts_current_grandparent_and_parent_without_retroactive_capture_mutation()
 {
    bounded(async {
        let h = Harness::new();
        let root = h.root("root").await;
        let middle = h.spawn(&root, "middle").await;
        let leaf = h.spawn(&middle, "leaf").await;
        let old = leaf.selection().capture().unwrap();
        assert_eq!(old.security.permissions, 0xff);
        middle
            .session()
            .update_selection(selection(&h.provider, "first", "middle-policy", 0b1011))
            .unwrap();
        let mut tightened = selection(&h.provider, "first", "root-policy", 0b0110);
        tightened.security = std::sync::Arc::new(Security {
            permissions: 0b0110,
            read_only: true,
        });
        root.session().update_selection(tightened).unwrap();
        let execution = send(leaf.session().as_ref(), "new authority").await;
        bounded(execution.execution.join()).await.unwrap();
        let built = h.factory.get("leaf");
        assert_eq!(
            built
                .tools
                .captures
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .security
                .permissions,
            0b0010
        );
        assert_eq!(old.security.permissions, 0xff);
        assert!(!old.security.read_only);
        assert!(
            built
                .tools
                .captures
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .security
                .read_only
        );
        assert_eq!(
            leaf.selection()
                .lineage()
                .count_profile("middle-policy")
                .unwrap(),
            1
        );
        assert_eq!(
            leaf.selection()
                .lineage()
                .count_profile("root-policy")
                .unwrap(),
            1
        );
        let root_policy = root.selection().current().unwrap().security;
        assert!(std::sync::Arc::ptr_eq(
            &PolicyLineage::root().resolve(root_policy.clone()).unwrap(),
            &root_policy
        ));
        h.close().await;
    })
    .await;
}

#[tokio::test]
async fn idle_existing_resume_survives_spawner_close_while_get_or_spawn_and_fresh_spawn_reject() {
    bounded(async {
        let h = Harness::new();
        let root = h.root("root").await;
        let child = h.spawn(&root, "child").await;
        bounded(root.spawner().close().join()).await.unwrap();
        let mut request = h.request(&root, "child");
        request.requested_profile = "updated".into();
        request.model = h.provider.selection("second");
        let resumed = bounded(root.spawner().spawn_or_resume(request).unwrap().join())
            .await
            .unwrap();
        assert!(std::sync::Arc::ptr_eq(&resumed, &child));
        assert_eq!(child.selection().current().unwrap().profile.id, "updated");
        if let Ok(operation) = root
            .spawner()
            .get_or_spawn("child".into(), || panic!("must reject before callback"))
        {
            assert!(bounded(operation.join()).await.is_err());
        }
        if let Ok(operation) = root.spawner().spawn(h.request(&root, "fresh")) {
            assert!(bounded(operation.join()).await.is_err());
        }
        h.close().await;
    })
    .await;
}
