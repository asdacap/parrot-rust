use super::{harness::Harness, support::*};
use parrot_domain::{
    contracts::{
        agent_state::{AgentStateError, AgentStateErrorKind, AgentStateStore},
        async_ops::BoxFuture,
    },
    values::agent_state::{AgentStateSnapshot, AgentStateUpdate, RetainedChild},
};
use parrot_runtime::{agent_turn::services::SelectionSource, children::*};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[tokio::test]
async fn retained_tree_reopens_same_identities_and_histories_without_execution() {
    bounded(async {
        let first = Harness::new();
        let root = first.root("root").await;
        let completed = first.spawn(&root, "completed").await;
        let sent = first
            .factory
            .get("completed")
            .adapter
            .session()
            .send_text_message(
                "finish work".into(),
                Arc::new(parrot_storage::events::NeverCancelled),
            )
            .await
            .unwrap();
        sent.execution.join().await.unwrap();
        let idle = first.spawn(&root, "idle").await;
        let nested = first.spawn(&idle, "nested").await;
        let expected = first
            .registry
            .snapshot_scopes()
            .into_iter()
            .map(|scope| {
                (
                    scope.identity().session.name.clone(),
                    scope.identity().session.session_id.clone(),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        let saved_conversation = completed.history().conversation().await.unwrap();
        let histories = first.histories.clone();
        let identity = root.identity().clone();
        first.close().await;
        let second = Harness::with_history_preparer(32, histories.clone(), histories);
        let restored_root = second.reopen_root(identity).await;
        restored_root
            .spawner()
            .restore_children()
            .unwrap()
            .join()
            .await
            .unwrap();
        let actual = second
            .registry
            .snapshot_scopes()
            .into_iter()
            .map(|scope| {
                (
                    scope.identity().session.name.clone(),
                    scope.identity().session.session_id.clone(),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(actual, expected);
        assert_eq!(
            restored_root
                .children()
                .resolve("completed")
                .unwrap()
                .history()
                .conversation()
                .await
                .unwrap(),
            saved_conversation
        );
        let restored_idle = restored_root.children().resolve("idle").unwrap();
        assert_eq!(
            restored_idle
                .children()
                .resolve("nested")
                .unwrap()
                .identity()
                .session
                .session_id,
            nested.identity().session.session_id
        );
        assert!(!restored_idle.session().is_active());
        assert!(second.provider.requests.lock().unwrap().is_empty());
        let reused = restored_root
            .spawner()
            .spawn_or_resume(second.request(&restored_root, "idle"))
            .unwrap()
            .join()
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&restored_idle, &reused));
        let root_state = restored_root.agent_state().load().await.unwrap().unwrap();
        assert_eq!(root_state.children.len(), 2);
        assert!(root_state.tasks.entries.is_empty());
        let nested_state = restored_idle.agent_state().load().await.unwrap().unwrap();
        assert_eq!(
            nested_state.children,
            vec![RetainedChild {
                requested_name: "nested".into(),
                name: "nested".into(),
                session_id: nested.identity().session.session_id.clone()
            }]
        );
        second.close().await;
    })
    .await;
}

#[tokio::test]
async fn accepted_child_selection_and_current_parent_policy_survive_restoration() {
    bounded(async {
        let first = Harness::new();
        let root = first.root("root").await;
        let child = first.spawn(&root, "worker").await;
        let mut request = first.request(&root, "worker");
        request.requested_profile = "query".into();
        request.model = first.provider.selection("second");
        request.requested_scope = "must not replace retained scope".into();
        let resumed = root
            .spawner()
            .spawn_or_resume(request)
            .unwrap()
            .join()
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&child, &resumed));
        let snapshot = child.agent_state().load().await.unwrap().unwrap();
        assert_eq!(snapshot.descriptor.selection.profile_id, "query");
        assert_eq!(
            snapshot.descriptor.selection.model_selector,
            "scripted/second"
        );
        assert_eq!(snapshot.descriptor.local_scope, "test scope");
        let identity = root.identity().clone();
        let histories = first.histories.clone();
        first.close().await;
        let second = Harness::with_history_preparer(32, histories.clone(), histories);
        let restored = second.reopen_root(identity).await;
        restored
            .selection()
            .try_update_security(|_| Ok::<_, ()>(Security::new(0x0f)))
            .unwrap();
        restored
            .spawner()
            .restore_children()
            .unwrap()
            .join()
            .await
            .unwrap();
        let worker = restored.children().resolve("worker").unwrap();
        let selected = worker.selection().capture().unwrap();
        assert_eq!(selected.profile.id, "query");
        assert_eq!(selected.model.requested_selector.value, "scripted/second");
        assert_eq!(selected.security.permissions, 0x0f);
        assert!(second.provider.requests.lock().unwrap().is_empty());
        second.close().await;
    })
    .await;
}

#[tokio::test]
async fn missing_referenced_snapshot_and_unavailable_model_fail_before_child_construction() {
    bounded(async {
        let h = Harness::new();
        let root = h.root("root").await;
        root.agent_state()
            .update(AgentStateUpdate::Children(vec![RetainedChild {
                requested_name: "missing".into(),
                name: "missing".into(),
                session_id: "missing-id".into(),
            }]))
            .await
            .unwrap();
        assert!(
            root.spawner()
                .restore_children()
                .unwrap()
                .join()
                .await
                .unwrap_err()
                .to_string()
                .contains("missing")
        );
        assert!(root.children().snapshot_children().is_empty());
        assert_eq!(h.factory.build_calls.load(Ordering::SeqCst), 1);
        root.agent_state()
            .update(AgentStateUpdate::Children(vec![]))
            .await
            .unwrap();
        let child = h.spawn(&root, "invalid-model").await;
        let mut snapshot = child.agent_state().load().await.unwrap().unwrap();
        snapshot.descriptor.selection.model_selector = "no-such-provider/model".into();
        child
            .agent_state()
            .update(AgentStateUpdate::Descriptor(snapshot.descriptor))
            .await
            .unwrap();
        let identity = root.identity().clone();
        let histories = h.histories.clone();
        h.close().await;
        let second = Harness::with_history_preparer(32, histories.clone(), histories);
        let restored = second.reopen_root(identity).await;
        assert!(
            restored
                .spawner()
                .restore_children()
                .unwrap()
                .join()
                .await
                .is_err()
        );
        assert_eq!(second.factory.build_calls.load(Ordering::SeqCst), 1);
        assert!(second.provider.requests.lock().unwrap().is_empty());
        second.close().await;
    })
    .await;
}

async fn leave_orphan(h: &Harness, root: &Arc<Scope<Security>>, name: &str) {
    h.factory.plans.lock().unwrap().insert(
        name.into(),
        ScopePlan {
            failure: Some(ChildError::registry("reject construction")),
            ..Default::default()
        },
    );
    assert!(
        root.spawner()
            .spawn(h.request(root, name))
            .unwrap()
            .join()
            .await
            .is_err()
    );
    assert!(
        root.agent_state()
            .load()
            .await
            .unwrap()
            .unwrap()
            .children
            .iter()
            .all(|child| child.name != name)
    );
}

#[tokio::test]
async fn orphan_incarnations_are_not_overwritten_and_named_reuse_keeps_suffix_identity() {
    bounded(async {
        let h = Harness::new();
        let root = h.root("root").await;
        leave_orphan(&h, &root, "orphan").await;
        let path = h
            .histories
            .path
            .join("root-agents/root/orphan/agent-state.json");
        let orphan_bytes = std::fs::read(&path).unwrap();
        let child = root
            .spawner()
            .spawn_or_resume(h.request(&root, "orphan"))
            .unwrap()
            .join()
            .await
            .unwrap();
        assert_eq!(child.identity().session.name, "orphan-2");
        let reused = root
            .spawner()
            .spawn_or_resume(h.request(&root, "orphan"))
            .unwrap()
            .join()
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&child, &reused));
        assert_eq!(std::fs::read(path).unwrap(), orphan_bytes);
        leave_orphan(&h, &root, "orphan-get").await;
        let request = h.request(&root, "orphan-get");
        let child = root
            .spawner()
            .get_or_spawn("orphan-get".into(), move || Ok(request))
            .unwrap()
            .join()
            .await
            .unwrap();
        let request = h.request(&root, "orphan-get");
        let reused = root
            .spawner()
            .get_or_spawn("orphan-get".into(), move || Ok(request))
            .unwrap()
            .join()
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&child, &reused));
        let histories = h.histories.clone();
        let identity = root.identity().clone();
        let retained_ids: Vec<_> = root
            .agent_state()
            .load()
            .await
            .unwrap()
            .unwrap()
            .children
            .into_iter()
            .map(|child| child.session_id)
            .collect();
        h.close().await;
        let second = Harness::with_history_preparer(32, histories.clone(), histories);
        let restored = second.reopen_root(identity).await;
        restored
            .spawner()
            .restore_children()
            .unwrap()
            .join()
            .await
            .unwrap();
        let ids: Vec<_> = restored
            .children()
            .snapshot_children()
            .into_iter()
            .map(|child| child.identity().session.session_id.clone())
            .collect();
        assert_eq!(ids, retained_ids);
        second.close().await;
    })
    .await;
}

struct UncertainPreparer {
    files: AgentStateFiles,
    fail: Arc<AtomicBool>,
}
impl AgentStatePreparer for UncertainPreparer {
    fn open<'a>(
        &'a self,
        names: &'a [String],
        owner: &'a str,
    ) -> BoxFuture<'a, ChildResult<Arc<dyn AgentStateStore>>> {
        Box::pin(async move {
            let inner = self.files.open(names, owner).await?;
            if names.len() == 1 {
                Ok(Arc::new(UncertainRoster {
                    inner,
                    fail: self.fail.clone(),
                }) as Arc<dyn AgentStateStore>)
            } else {
                Ok(inner)
            }
        })
    }
}
struct UncertainRoster {
    inner: Arc<dyn AgentStateStore>,
    fail: Arc<AtomicBool>,
}
impl AgentStateStore for UncertainRoster {
    fn retire(&self) -> BoxFuture<'_, Result<(), AgentStateError>> {
        self.inner.retire()
    }
    fn load(&self) -> BoxFuture<'_, Result<Option<AgentStateSnapshot>, AgentStateError>> {
        self.inner.load()
    }
    fn initialize(&self, value: AgentStateSnapshot) -> BoxFuture<'_, Result<(), AgentStateError>> {
        self.inner.initialize(value)
    }
    fn update(
        &self,
        value: AgentStateUpdate,
    ) -> BoxFuture<'_, Result<AgentStateSnapshot, AgentStateError>> {
        Box::pin(async move {
            let roster = matches!(&value, AgentStateUpdate::Children(_));
            let saved = self.inner.update(value).await?;
            if roster && self.fail.swap(false, Ordering::SeqCst) {
                return Err(AgentStateError::new(
                    AgentStateErrorKind::Io,
                    "directory sync failed after rename",
                ));
            }
            Ok(saved)
        })
    }
}

#[tokio::test]
async fn uncertain_roster_commit_keeps_referenced_fork_history_reconstructable() {
    bounded(async {
        let histories = Histories::new();
        let states = Arc::new(UncertainPreparer {
            files: AgentStateFiles::new(
                histories
                    .repository
                    .events()
                    .database()
                    .selected_root()
                    .clone(),
            ),
            fail: Arc::new(AtomicBool::new(true)),
        });
        let h = Harness::with_state_preparer(histories.clone(), states);
        let root = h.root("root").await;
        h.histories
            .append(root.history(), "retained fork content")
            .await;
        let mut request = h.request(&root, "uncertain");
        request.fork = parrot_storage::conversation::HistoryForkSelection::parse("full");
        assert!(root.spawner().spawn(request).unwrap().join().await.is_err());
        let retained = root.agent_state().load().await.unwrap().unwrap().children;
        assert_eq!(retained.len(), 1);
        let identity = root.identity().clone();
        h.close().await;
        let second = Harness::with_history_preparer(32, histories.clone(), histories);
        let restored = second.reopen_root(identity).await;
        restored
            .spawner()
            .restore_children()
            .unwrap()
            .join()
            .await
            .unwrap();
        let child = restored.children().resolve("uncertain").unwrap();
        assert_eq!(child.identity().session.session_id, retained[0].session_id);
        assert_eq!(child.history().conversation().await.unwrap().len(), 1);
        assert!(second.provider.requests.lock().unwrap().is_empty());
        second.close().await;
    })
    .await;
}

#[tokio::test]
async fn shutdown_during_restoration_closes_the_constructed_rejected_scope() {
    bounded(async {
        let first = Harness::new();
        let root = first.root("root").await;
        first.spawn(&root, "worker").await;
        let identity = root.identity().clone();
        let histories = first.histories.clone();
        first.close().await;
        let second = Harness::with_history_preparer(32, histories.clone(), histories);
        let restored = second.reopen_root(identity).await;
        let gate = Gate::new();
        *second.factory.construction.lock().unwrap() = Some(gate.clone());
        let restoration = restored.spawner().restore_children().unwrap();
        gate.entered().await;
        let shutdown = second.registry.shutdown();
        gate.open();
        assert!(restoration.join().await.is_err());
        shutdown.join().await.unwrap();
        assert_eq!(
            second
                .factory
                .get("worker")
                .output
                .closes
                .load(Ordering::SeqCst),
            1
        );
        second.budget.reserve().unwrap().rollback();
    })
    .await;
}

#[tokio::test]
async fn failed_binding_does_not_enter_the_retained_roster() {
    bounded(async {
        let h = Harness::new();
        let root = h.root("root").await;
        *h.admission_error.lock().unwrap() = Some(ChildError::registry("binding rejected"));
        assert!(
            root.spawner()
                .spawn(h.request(&root, "rejected"))
                .unwrap()
                .join()
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("binding rejected")
        );
        assert!(root.children().snapshot_children().is_empty());
        assert!(
            root.agent_state()
                .load()
                .await
                .unwrap()
                .unwrap()
                .children
                .is_empty()
        );
        let child = h.spawn(&root, "rejected").await;
        assert_eq!(child.identity().session.name, "rejected-2");
        assert_eq!(
            root.agent_state()
                .load()
                .await
                .unwrap()
                .unwrap()
                .children
                .len(),
            1
        );
        h.close().await;
    })
    .await;
}

#[tokio::test]
async fn unrelated_explicit_suffix_is_not_reused_for_an_orphaned_base_name() {
    bounded(async {
        let h = Harness::new();
        let root = h.root("root").await;
        leave_orphan(&h, &root, "worker").await;
        let unrelated = h.spawn(&root, "worker-2").await;
        let child = root
            .spawner()
            .spawn_or_resume(h.request(&root, "worker"))
            .unwrap()
            .join()
            .await
            .unwrap();
        assert_eq!(child.identity().session.name, "worker-3");
        assert_ne!(
            child.identity().session.session_id,
            unrelated.identity().session.session_id
        );
        let reused = root
            .spawner()
            .spawn_or_resume(h.request(&root, "worker"))
            .unwrap()
            .join()
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&child, &reused));
        h.close().await;
    })
    .await;
}

#[tokio::test]
async fn independent_construction_remains_parallel_with_serialized_roster_updates() {
    bounded(async {
        let h = Harness::new();
        let root = h.root("root").await;
        let gate = Gate::new();
        *h.factory.construction.lock().unwrap() = Some(gate.clone());
        let first = root.spawner().spawn(h.request(&root, "first")).unwrap();
        gate.entered().await;
        let second = root
            .spawner()
            .spawn(h.request(&root, "second"))
            .unwrap()
            .join()
            .await
            .unwrap();
        assert!(!first.is_finished());
        gate.open();
        let first = first.join().await.unwrap();
        let children = root.agent_state().load().await.unwrap().unwrap().children;
        assert_eq!(children.len(), 2);
        assert!(
            children
                .iter()
                .any(|child| child.session_id == first.identity().session.session_id)
        );
        assert!(
            children
                .iter()
                .any(|child| child.session_id == second.identity().session.session_id)
        );
        h.close().await;
    })
    .await;
}

#[tokio::test]
async fn retirement_releases_exact_name_and_does_not_restore_the_old_identity() {
    bounded(async {
        let h = Harness::new();
        let root = h.root("root").await;
        let original = h.spawn(&root, "worker").await;
        let id = original.identity().session.session_id.clone();
        root.spawner()
            .retire(original.clone())
            .unwrap()
            .join()
            .await
            .unwrap();
        assert_eq!(
            h.factory.get("worker").output.closes.load(Ordering::SeqCst),
            1
        );
        assert!(
            root.agent_state()
                .load()
                .await
                .unwrap()
                .unwrap()
                .children
                .is_empty()
        );
        let replacement = h.spawn(&root, "worker").await;
        assert_eq!(replacement.identity().session.name, "worker");
        assert_ne!(replacement.identity().session.session_id, id);
        let identity = root.identity().clone();
        let histories = h.histories.clone();
        h.close().await;
        let second = Harness::with_history_preparer(32, histories.clone(), histories);
        let restored = second.reopen_root(identity).await;
        restored
            .spawner()
            .restore_children()
            .unwrap()
            .join()
            .await
            .unwrap();
        assert_eq!(
            restored
                .children()
                .resolve("worker")
                .unwrap()
                .identity()
                .session
                .session_id,
            replacement.identity().session.session_id
        );
        assert!(
            second.registry.snapshot_scopes().iter().all(|scope| scope
                .identity()
                .session
                .session_id
                != id)
        );
        second.close().await;
    })
    .await;
}

#[tokio::test]
async fn uncertain_retirement_keeps_the_name_reserved_until_join_and_retry() {
    bounded(async {
        let histories = Histories::new();
        let fail = Arc::new(AtomicBool::new(false));
        let states = Arc::new(UncertainPreparer {
            files: AgentStateFiles::new(
                histories
                    .repository
                    .events()
                    .database()
                    .selected_root()
                    .clone(),
            ),
            fail: fail.clone(),
        });
        let h = Harness::with_state_preparer(histories, states);
        let root = h.root("root").await;
        let original = h.spawn(&root, "worker").await;
        fail.store(true, Ordering::SeqCst);
        assert!(
            root.spawner()
                .retire(original.clone())
                .unwrap()
                .join()
                .await
                .is_err()
        );
        assert!(Arc::ptr_eq(
            &root.children().resolve("worker").unwrap(),
            &original
        ));
        assert_eq!(
            h.factory.get("worker").output.closes.load(Ordering::SeqCst),
            1
        );
        assert!(original.agent_state().load().await.unwrap().is_some());
        root.spawner()
            .retire(original.clone())
            .unwrap()
            .join()
            .await
            .unwrap();
        assert!(root.children().find("worker").is_none());
        assert!(original.agent_state().load().await.unwrap().is_none());
        let replacement = h.spawn(&root, "worker").await;
        assert_eq!(replacement.identity().session.name, "worker");
        assert_ne!(
            replacement.identity().session.session_id,
            original.identity().session.session_id
        );
        h.close().await;
    })
    .await;
}
