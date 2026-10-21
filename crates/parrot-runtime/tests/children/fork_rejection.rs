use super::{harness::Harness, support::*};
use parrot_runtime::children::*;
use parrot_storage::conversation::HistoryForkSelection;

#[tokio::test]
async fn invalid_sibling_sources_reject_parent_self_other_branch_and_foreign_root_without_consuming_name()
 {
    bounded(async {
        let h = Harness::new();
        let root = h.root("root").await;
        let owner = h.spawn(&root, "owner").await;
        let other_branch = h.spawn(&root, "other-branch").await;
        let foreign_child = h.spawn(&other_branch, "foreign-child").await;
        let foreign_root = h.root("foreign-root").await;
        let allowed = h.spawn(&owner, "sibling").await;
        h.histories
            .append(allowed.history(), "allowed sibling history")
            .await;
        for (index, source) in [
            owner.session().clone(),
            foreign_child.session().clone(),
            foreign_root.session().clone(),
        ]
        .into_iter()
        .enumerate()
        {
            let name = format!("reusable-{index}");
            let mut request = h.request(&owner, &name);
            request.history_source = HistorySource::Sibling(source);
            request.fork = HistoryForkSelection::parse("full");
            let error = bounded(owner.spawner().spawn(request).unwrap().join())
                .await
                .err()
                .unwrap();
            assert!(error.to_string().contains("registered sibling"));
            assert!(owner.children().find(&name).is_none());
            let mut valid = h.request(&owner, &name);
            valid.history_source = HistorySource::Sibling(allowed.session().clone());
            valid.fork = HistoryForkSelection::parse("full");
            let child = bounded(owner.spawner().spawn(valid).unwrap().join())
                .await
                .unwrap();
            assert_eq!(child.identity().session.name, name);
            assert!(
                child
                    .history()
                    .conversation()
                    .await
                    .unwrap()
                    .iter()
                    .any(|item| item.message.parts
                        == [parrot_storage::events::ConversationPart::text(
                            "allowed sibling history"
                        )])
            );
        }
        h.close().await;
    })
    .await;
}
