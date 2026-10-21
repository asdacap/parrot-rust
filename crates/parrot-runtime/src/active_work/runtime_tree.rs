//! Runtime tree observations from retained scopes and their actual scoped services.
use super::status::{
    StatusError, StatusObservation, StatusProvider, StatusQuery, StatusResult, ordinal, render,
};
use crate::{
    agent_queues::{AgentQueues, QueueOwnerSnapshot},
    agent_turn::services::SessionIdentity,
    children::{AgentRegistry, Scope},
};
use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation, OperationError},
        security::RestrictableAccessSnapshot,
        status::{ProcessStatusSnapshot, ProcessStatusSource},
    },
    values::activity::DrainState,
};
use parrot_foundation::{
    config::opaque_json::json_string, template_catalog::PromptTemplateCatalog,
};
use parrot_storage::queue_files::QueueInfo;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

#[derive(Default)]
pub struct RuntimeTreeSnapshot {
    pub queue_owners: Vec<QueueOwnerSnapshot>,
    pub active_agents: Vec<SessionIdentity>,
    pub processes: Vec<ProcessStatusSnapshot>,
    pub activity: Vec<(String, DrainState)>,
}
pub trait RuntimeTreeSnapshotSource: Send + Sync {
    fn capture(&self) -> Result<RuntimeTreeSnapshot, StatusError>;
}

/// The callback reads the actual session's domain activity tracker; it neither
/// creates a second activity state nor guesses a state from `is_active`.
pub type ScopeActivity<S> = Arc<dyn Fn(&Scope<S>) -> Result<DrainState, StatusError> + Send + Sync>;
pub struct RegistryRuntimeTreeSource<S: RestrictableAccessSnapshot + 'static> {
    registry: Arc<AgentRegistry<S>>,
    activity: ScopeActivity<S>,
}
impl<S: RestrictableAccessSnapshot + 'static> RegistryRuntimeTreeSource<S> {
    pub fn new(registry: Arc<AgentRegistry<S>>, activity: ScopeActivity<S>) -> Self {
        Self { registry, activity }
    }
}
impl<S: RestrictableAccessSnapshot + 'static> RuntimeTreeSnapshotSource
    for RegistryRuntimeTreeSource<S>
{
    fn capture(&self) -> Result<RuntimeTreeSnapshot, StatusError> {
        let scopes = self.registry.snapshot_scopes();
        let mut queue_owners = Vec::with_capacity(scopes.len());
        for scope in &scopes {
            let queues = scope
                .get_service::<AgentQueues<S>>()
                .map_err(|error| StatusError::caused_by(error.to_string(), error))?;
            queue_owners.push(
                queues
                    .snapshot()
                    .map_err(|error| StatusError::caused_by(error.to_string(), error))?,
            );
        }
        let active_agents = self.registry.active_snapshot();
        let mut processes = Vec::new();
        for scope in &scopes {
            let source = scope
                .get_service::<dyn ProcessStatusSource>()
                .map_err(|error| StatusError::caused_by(error.to_string(), error))?;
            processes.extend(source.snapshot());
        }
        let mut activity = Vec::with_capacity(scopes.len());
        for scope in &scopes {
            activity.push((
                scope.identity().session.session_id.clone(),
                (self.activity)(scope)?,
            ));
        }
        Ok(RuntimeTreeSnapshot {
            queue_owners,
            active_agents,
            processes,
            activity,
        })
    }
}

pub struct RuntimeTreeStatusProvider {
    pub source: Arc<dyn RuntimeTreeSnapshotSource>,
    pub templates: Arc<PromptTemplateCatalog>,
}
impl StatusProvider for RuntimeTreeStatusProvider {
    fn key(&self) -> &str {
        "runtime:queues"
    }
    fn observe<'a>(
        &'a self,
        query: &'a StatusQuery,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, StatusResult<StatusObservation>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| OperationError::Cancelled)?;
            let snapshot = self.source.capture().map_err(OperationError::Failed)?;
            let nodes = build_nodes(&query.session_id, snapshot);
            let mut agents = Vec::new();
            append(
                &mut agents,
                &nodes,
                &query.session_id,
                0,
                &mut HashSet::new(),
            );
            render(
                &self.templates,
                "status.runtime",
                json!({"section": "tree", "agents": agents, "runs": []})
                    .as_object()
                    .unwrap()
                    .clone(),
                cancellation,
            )
        })
    }
}
struct Node {
    session_id: String,
    parent_session_id: String,
    name: String,
    status: &'static str,
    queues: Vec<QueueInfo>,
    processes: Vec<ProcessStatusSnapshot>,
    children: Vec<String>,
}
fn add<'a>(
    nodes: &'a mut HashMap<String, Node>,
    session_id: &str,
    parent_session_id: &str,
    name: &str,
) -> &'a mut Node {
    let node = nodes.entry(session_id.into()).or_insert_with(|| Node {
        session_id: session_id.into(),
        parent_session_id: parent_session_id.into(),
        name: if name.is_empty() { session_id } else { name }.into(),
        status: "",
        queues: Vec::new(),
        processes: Vec::new(),
        children: Vec::new(),
    });
    if node.parent_session_id.is_empty() && !parent_session_id.is_empty() {
        node.parent_session_id = parent_session_id.into();
    }
    if node.name == node.session_id && !name.is_empty() {
        node.name = name.into();
    }
    node
}
fn build_nodes(root: &str, snapshot: RuntimeTreeSnapshot) -> HashMap<String, Node> {
    let mut nodes = HashMap::new();
    add(&mut nodes, root, "", root);
    for owner in snapshot.queue_owners {
        let identity = &owner.identity.session;
        add(
            &mut nodes,
            &identity.session_id,
            &identity.parent_session_id,
            &identity.name,
        )
        .queues
        .extend(owner.queues);
    }
    for agent in snapshot.active_agents {
        add(
            &mut nodes,
            &agent.session_id,
            &agent.parent_session_id,
            &agent.name,
        );
    }
    for process in snapshot.processes {
        add(
            &mut nodes,
            &process.owner_session_id,
            "",
            &process.owner_session_id,
        )
        .processes
        .push(process);
    }
    let edges: Vec<_> = nodes
        .values()
        .filter(|node| node.session_id != root && !node.parent_session_id.is_empty())
        .map(|node| (node.parent_session_id.clone(), node.session_id.clone()))
        .collect();
    for (parent, child) in edges {
        if let Some(parent) = nodes.get_mut(&parent) {
            parent.children.push(child);
        }
    }
    for (session_id, state) in snapshot.activity {
        if let Some(node) = nodes.get_mut(&session_id) {
            node.status = match state {
                DrainState::Idle => "",
                DrainState::Running => "running",
                DrainState::Interrupting => "interrupting",
            };
        }
    }
    nodes
}
fn append(
    output: &mut Vec<Value>,
    nodes: &HashMap<String, Node>,
    session_id: &str,
    depth: usize,
    seen: &mut HashSet<String>,
) {
    if !seen.insert(session_id.into()) {
        return;
    }
    let Some(node) = nodes.get(session_id) else {
        return;
    };
    let mut queues: Vec<_> = node.queues.iter().collect();
    queues.sort_by(|left, right| ordinal(&left.name, &right.name));
    let queues: Vec<_> = queues.into_iter().map(|queue| json!({
        "name": queue.name, "size": queue.size.to_string(),
        "description": if queue.description.is_empty() { String::new() } else { json_string(&queue.description) },
    })).collect();
    let mut processes: Vec<_> = node.processes.iter().collect();
    processes.sort_by(|left, right| ordinal(&left.process_id, &right.process_id));
    let processes: Vec<_> = processes
        .into_iter()
        .map(|process| json!({"id": process.id(), "state": "running", "name": process.name}))
        .collect();
    output.push(json!({"indent": " ".repeat(depth * 2), "name": node.name, "status": node.status, "session_id": node.session_id, "queues": queues, "processes": processes}));
    let mut children: Vec<_> = node.children.iter().collect();
    children.sort_by(|left, right| ordinal(left, right));
    for child in children {
        append(output, nodes, child, depth + 1, seen);
    }
}
