use super::{harness::Harness, support::*};
use parrot_foundation::config::runtime::AgentLimitsConfig;
use std::sync::Arc;

#[tokio::test]
async fn default_disabled_recursion_allows_worker_depth_five_without_relaxing_other_admission() {
    bounded(async {
        let h = Harness::with_limits(5, AgentLimitsConfig::default());
        let root = h.root("root").await;
        let mut restricted = selection(&h.provider, "first", "root-mode", 0b0011);
        restricted.security = Arc::new(Security {
            permissions: 0b0011,
            read_only: true,
        });
        root.session().update_selection(restricted).unwrap();
        let mut parent = root.clone();
        for depth in 1..=5 {
            let mut request = h.request(&parent, &format!("worker-{depth}"));
            request.requested_profile = "worker".into();
            parent = parent
                .spawner()
                .spawn(request)
                .unwrap()
                .join()
                .await
                .unwrap();
            assert_eq!(parent.identity().session.depth, depth);
            let selected = parent.selection().current().unwrap();
            assert_eq!(selected.profile.id, "worker");
            assert_eq!(selected.security.permissions, 0b0011);
            assert!(selected.security.read_only);
        }
        let error = parent
            .spawner()
            .spawn(h.request(&root, "forged-parent"))
            .unwrap()
            .join()
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("launch parent does not match"));
        let error = parent
            .spawner()
            .spawn(h.request(&parent, "over-capacity"))
            .unwrap()
            .join()
            .await
            .err()
            .unwrap();
        assert_eq!(error.to_string(), "subagent retention limit reached");
        assert_eq!(
            h.factory
                .build_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            6
        );
        h.close().await;
        assert!(
            parent
                .spawner()
                .spawn(h.request(&parent, "after-close"))
                .is_err()
        );
    })
    .await;
}

#[tokio::test]
async fn recursion_switch_controls_both_fresh_profile_admission_and_retained_resume() {
    for disabled in [true, false] {
        bounded(async {
            let h = Harness::with_limits(
                8,
                AgentLimitsConfig {
                    disable_recursion_limit: disabled,
                    ..Default::default()
                },
            );
            let root = h.root("root").await;
            let existing = h.spawn(&root, "existing").await;
            root.session()
                .update_selection(selection(&h.provider, "first", "once", 0b0101))
                .unwrap();
            let mut request = h.request(&root, "existing");
            request.requested_profile = "once".into();
            let resumed = root
                .spawner()
                .spawn_or_resume(request)
                .unwrap()
                .join()
                .await;
            if disabled {
                assert!(Arc::ptr_eq(&resumed.unwrap(), &existing));
                let selected = existing.selection().current().unwrap();
                assert_eq!(selected.profile.id, "once");
                assert_eq!(selected.security.permissions, 0b0101);
            } else {
                assert_eq!(
                    resumed.err().unwrap().to_string(),
                    "subagent profile recursion limit reached"
                );
                assert_eq!(existing.selection().current().unwrap().profile.id, "child");
            }
            let mut request = h.request(&root, "fresh-once");
            request.requested_profile = "once".into();
            let fresh = root.spawner().spawn(request).unwrap().join().await;
            if disabled {
                assert_eq!(
                    fresh.unwrap().selection().current().unwrap().profile.id,
                    "once"
                );
            } else {
                assert_eq!(
                    fresh.err().unwrap().to_string(),
                    "subagent profile recursion limit reached"
                );
            }
            h.close().await;
        })
        .await;
    }
}

#[tokio::test]
async fn explicit_false_restores_depth_four_and_latest_query_profile_count_three() {
    bounded(async {
        let h = Harness::with_limits(
            16,
            AgentLimitsConfig {
                disable_recursion_limit: false,
                ..Default::default()
            },
        );
        let root = h.root("root").await;
        let mut parent = root.clone();
        for depth in 1..=3 {
            let mut request = h.request(&parent, &format!("query-{depth}"));
            request.requested_profile = "query".into();
            parent = parent
                .spawner()
                .spawn(request)
                .unwrap()
                .join()
                .await
                .unwrap();
        }
        let mut request = h.request(&parent, "fourth-query");
        request.requested_profile = "query".into();
        let error = parent
            .spawner()
            .spawn(request)
            .unwrap()
            .join()
            .await
            .err()
            .unwrap();
        assert_eq!(
            error.to_string(),
            "subagent profile recursion limit reached"
        );
        let mut parent = root.clone();
        for depth in 1..=4 {
            let mut request = h.request(&parent, &format!("worker-{depth}"));
            request.requested_profile = "worker".into();
            parent = parent
                .spawner()
                .spawn(request)
                .unwrap()
                .join()
                .await
                .unwrap();
        }
        let mut request = h.request(&parent, "fifth-worker");
        request.requested_profile = "worker".into();
        let error = parent
            .spawner()
            .spawn(request)
            .unwrap()
            .join()
            .await
            .err()
            .unwrap();
        assert_eq!(error.to_string(), "subagent depth limit reached");
        h.close().await;
    })
    .await;
}
