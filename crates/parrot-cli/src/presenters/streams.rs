//! Source-shaped filters for transient usage and chunked tool inventories.
//!
//! A consumed event never reaches the activity renderer. Inventory state is
//! scoped to this reader, so reconnecting starts a fresh generation history.

use std::collections::{HashMap, HashSet};

use futures_util::Stream;
use futures_util::StreamExt;
use futures_util::future::{Either, select};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_domain::protocol::messages::{parrot as wire, process, queue};
use protobuf::Message;
use tonic::Status;

pub const QUEUE_INVENTORY_KIND: &str = "queue";
pub const SHELL_PROCESS_INVENTORY_KIND: &str = "shell_process";

/// `Consumed(None)` includes incomplete, stale, retired or malformed-envelope
/// inventory chunks. Invalid protobuf data is an error, not a forwarded event.
#[derive(Clone, Debug, PartialEq)]
pub enum StreamObservation<T> {
    Forward,
    Consumed(Option<T>),
}

async fn next_filtered<S, T, F, O>(
    source: &mut S,
    filter: &mut F,
    observe: &mut O,
    cancellation: &dyn Cancellation,
) -> Result<Option<wire::Event>, Status>
where
    S: Stream<Item = Result<wire::Event, Status>> + Unpin,
    F: FnMut(&wire::Event) -> Result<StreamObservation<T>, Status>,
    O: for<'a> FnMut(T, &'a dyn Cancellation) -> BoxFuture<'a, Result<(), Status>>,
{
    loop {
        cancellation
            .check()
            .map_err(|_| Status::cancelled("operation cancelled"))?;
        let next = match select(cancellation.cancelled(), Box::pin(source.next())).await {
            Either::Left(_) => return Err(Status::cancelled("operation cancelled")),
            Either::Right((next, _)) => next,
        };
        let Some(published) = next else {
            return Ok(None);
        };
        let published = published?;
        match filter(&published)? {
            StreamObservation::Forward => return Ok(Some(published)),
            StreamObservation::Consumed(Some(value)) => observe(value, cancellation).await?,
            StreamObservation::Consumed(None) => {}
        }
    }
}

#[derive(Default, Debug)]
pub struct ProviderCallUsageStreamReader;

impl ProviderCallUsageStreamReader {
    pub fn new() -> Self {
        Self
    }

    pub fn observe(
        &mut self,
        published: &wire::Event,
    ) -> Result<StreamObservation<wire::ProviderCallUsage>, Status> {
        Ok(match &published.payload {
            Some(wire::event::Payload::ProviderCallUsage(usage)) => {
                StreamObservation::Consumed(Some(usage.clone()))
            }
            _ => StreamObservation::Forward,
        })
    }

    pub async fn next<S, O>(
        &mut self,
        source: &mut S,
        mut observe: O,
        cancellation: &dyn Cancellation,
    ) -> Result<Option<wire::Event>, Status>
    where
        S: Stream<Item = Result<wire::Event, Status>> + Unpin,
        O: for<'a> FnMut(
            wire::ProviderCallUsage,
            &'a dyn Cancellation,
        ) -> BoxFuture<'a, Result<(), Status>>,
    {
        next_filtered(
            source,
            &mut |event| self.observe(event),
            &mut observe,
            cancellation,
        )
        .await
    }
}

#[derive(Default, Debug)]
pub struct SessionUsageSnapshotStreamReader;

impl SessionUsageSnapshotStreamReader {
    pub fn new() -> Self {
        Self
    }

    pub fn observe(
        &mut self,
        published: &wire::Event,
    ) -> Result<StreamObservation<wire::SessionUsageSnapshot>, Status> {
        Ok(match &published.payload {
            Some(wire::event::Payload::SessionUsageSnapshot(snapshot)) => {
                StreamObservation::Consumed(Some(snapshot.clone()))
            }
            _ => StreamObservation::Forward,
        })
    }

    pub async fn next<S, O>(
        &mut self,
        source: &mut S,
        mut observe: O,
        cancellation: &dyn Cancellation,
    ) -> Result<Option<wire::Event>, Status>
    where
        S: Stream<Item = Result<wire::Event, Status>> + Unpin,
        O: for<'a> FnMut(
            wire::SessionUsageSnapshot,
            &'a dyn Cancellation,
        ) -> BoxFuture<'a, Result<(), Status>>,
    {
        next_filtered(
            source,
            &mut |event| self.observe(event),
            &mut observe,
            cancellation,
        )
        .await
    }
}

#[derive(Default, Debug)]
pub struct QueueSnapshotStreamReader {
    owners: HashMap<String, QueueOwnerInventory>,
}

impl QueueSnapshotStreamReader {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn observe(
        &mut self,
        published: &wire::Event,
    ) -> Result<StreamObservation<(wire::ToolSnapshot, queue::QueueSnapshot)>, Status> {
        let Some(wire::event::Payload::ToolSnapshot(inventory)) = &published.payload else {
            return Ok(StreamObservation::Forward);
        };
        if inventory.kind != QUEUE_INVENTORY_KIND {
            return Ok(StreamObservation::Forward);
        }
        if inventory.owner_agent_session_id.is_empty() || inventory.inventory_instance_id.is_empty()
        {
            return Ok(StreamObservation::Consumed(None));
        }
        let snapshot = queue::QueueSnapshot::parse_from_bytes(&inventory.data)
            .map_err(|error| Status::internal(format!("Invalid queue snapshot: {error}")))?;
        let completed = self
            .owners
            .entry(inventory.owner_agent_session_id.clone())
            .or_default()
            .observe(inventory, snapshot);
        Ok(StreamObservation::Consumed(
            completed.map(|snapshot| (inventory.clone(), snapshot)),
        ))
    }

    pub async fn next<S, O>(
        &mut self,
        source: &mut S,
        mut observe: O,
        cancellation: &dyn Cancellation,
    ) -> Result<Option<wire::Event>, Status>
    where
        S: Stream<Item = Result<wire::Event, Status>> + Unpin,
        O: for<'a> FnMut(
            (wire::ToolSnapshot, queue::QueueSnapshot),
            &'a dyn Cancellation,
        ) -> BoxFuture<'a, Result<(), Status>>,
    {
        next_filtered(
            source,
            &mut |event| self.observe(event),
            &mut observe,
            cancellation,
        )
        .await
    }
}

#[derive(Default, Debug)]
struct QueueOwnerInventory {
    retired_instances: HashSet<String>,
    queues: Vec<queue::QueueState>,
    instance: Option<String>,
    first: Option<wire::ToolSnapshot>,
    root_agent_session_id: Option<String>,
    revision: u64,
    next_chunk: u32,
    observed: bool,
    removed: bool,
}

impl QueueOwnerInventory {
    fn observe(
        &mut self,
        inventory: &wire::ToolSnapshot,
        snapshot: queue::QueueSnapshot,
    ) -> Option<queue::QueueSnapshot> {
        if self
            .retired_instances
            .contains(&inventory.inventory_instance_id)
        {
            return None;
        }
        if inventory.chunk_index == 0 {
            if self.instance.as_deref() == Some(&inventory.inventory_instance_id) {
                if self.removed || (self.observed && inventory.revision <= self.revision) {
                    return None;
                }
            } else {
                if let Some(instance) = self.instance.take() {
                    self.retired_instances.insert(instance);
                }
                self.instance = Some(inventory.inventory_instance_id.clone());
                self.removed = false;
            }
            self.reset_staging();
            self.first = Some(inventory.clone());
            self.root_agent_session_id = Some(snapshot.root_agent_session_id.clone());
            self.revision = inventory.revision;
            self.observed = true;
        }
        if self.first.is_none()
            || self.instance.as_deref() != Some(&inventory.inventory_instance_id)
            || inventory.revision != self.revision
            || inventory.chunk_index != self.next_chunk
            || inventory.removed != self.first.as_ref()?.removed
            || Some(snapshot.root_agent_session_id.as_str())
                != self.root_agent_session_id.as_deref()
        {
            return None;
        }
        self.queues.extend(snapshot.queues);
        self.next_chunk = self.next_chunk.wrapping_add(1);
        if !inventory.final_chunk {
            return None;
        }
        let completed = queue::QueueSnapshot {
            root_agent_session_id: snapshot.root_agent_session_id,
            queues: std::mem::take(&mut self.queues),
            ..Default::default()
        };
        self.removed = inventory.removed;
        self.reset_staging();
        Some(completed)
    }

    fn reset_staging(&mut self) {
        self.queues.clear();
        self.first = None;
        self.next_chunk = 0;
    }
}

#[derive(Default, Debug)]
pub struct ShellProcessSnapshotStreamReader {
    owners: HashMap<String, ProcessOwnerInventory>,
}

impl ShellProcessSnapshotStreamReader {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn observe(
        &mut self,
        published: &wire::Event,
    ) -> Result<StreamObservation<(wire::ToolSnapshot, process::ShellProcessSnapshot)>, Status>
    {
        let Some(wire::event::Payload::ToolSnapshot(inventory)) = &published.payload else {
            return Ok(StreamObservation::Forward);
        };
        if inventory.kind != SHELL_PROCESS_INVENTORY_KIND {
            return Ok(StreamObservation::Forward);
        }
        if inventory.owner_agent_session_id.is_empty() || inventory.inventory_instance_id.is_empty()
        {
            return Ok(StreamObservation::Consumed(None));
        }
        let snapshot =
            process::ShellProcessSnapshot::parse_from_bytes(&inventory.data).map_err(|error| {
                Status::internal(format!("Invalid shell process snapshot: {error}"))
            })?;
        let completed = self
            .owners
            .entry(inventory.owner_agent_session_id.clone())
            .or_default()
            .observe(inventory, snapshot);
        Ok(StreamObservation::Consumed(
            completed.map(|snapshot| (inventory.clone(), snapshot)),
        ))
    }

    pub async fn next<S, O>(
        &mut self,
        source: &mut S,
        mut observe: O,
        cancellation: &dyn Cancellation,
    ) -> Result<Option<wire::Event>, Status>
    where
        S: Stream<Item = Result<wire::Event, Status>> + Unpin,
        O: for<'a> FnMut(
            (wire::ToolSnapshot, process::ShellProcessSnapshot),
            &'a dyn Cancellation,
        ) -> BoxFuture<'a, Result<(), Status>>,
    {
        next_filtered(
            source,
            &mut |event| self.observe(event),
            &mut observe,
            cancellation,
        )
        .await
    }
}

#[derive(Default, Debug)]
struct ProcessOwnerInventory {
    retired_instances: HashSet<String>,
    processes: Vec<process::ActiveShellProcess>,
    completed_processes: Vec<process::CompletedShellProcess>,
    instance: Option<String>,
    first: Option<wire::ToolSnapshot>,
    revision: u64,
    next_chunk: u32,
    observed: bool,
    removed: bool,
}

impl ProcessOwnerInventory {
    fn observe(
        &mut self,
        inventory: &wire::ToolSnapshot,
        snapshot: process::ShellProcessSnapshot,
    ) -> Option<process::ShellProcessSnapshot> {
        if self
            .retired_instances
            .contains(&inventory.inventory_instance_id)
        {
            return None;
        }
        if inventory.chunk_index == 0 {
            if self.instance.as_deref() == Some(&inventory.inventory_instance_id) {
                if self.removed || (self.observed && inventory.revision <= self.revision) {
                    return None;
                }
            } else {
                if let Some(instance) = self.instance.take() {
                    self.retired_instances.insert(instance);
                }
                self.instance = Some(inventory.inventory_instance_id.clone());
                self.removed = false;
            }
            self.reset_staging();
            self.first = Some(inventory.clone());
            self.revision = inventory.revision;
            self.observed = true;
        }
        if self.first.is_none()
            || self.instance.as_deref() != Some(&inventory.inventory_instance_id)
            || inventory.revision != self.revision
            || inventory.chunk_index != self.next_chunk
            || inventory.removed != self.first.as_ref()?.removed
        {
            return None;
        }
        self.processes.extend(snapshot.processes);
        self.completed_processes
            .extend(snapshot.completed_processes);
        self.next_chunk = self.next_chunk.wrapping_add(1);
        if !inventory.final_chunk {
            return None;
        }
        let completed = process::ShellProcessSnapshot {
            processes: std::mem::take(&mut self.processes),
            completed_processes: std::mem::take(&mut self.completed_processes),
            ..Default::default()
        };
        self.removed = inventory.removed;
        self.reset_staging();
        Some(completed)
    }

    fn reset_staging(&mut self) {
        self.processes.clear();
        self.completed_processes.clear();
        self.first = None;
        self.next_chunk = 0;
    }
}
