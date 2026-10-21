//! Synchronous observation of scoped process owners for runtime status assembly.
//!
//! Mirrors source revision 5f27a8a92625ee31d8bec094e5c3e09550193781:
//! `Parrot.Common/Statuses/ActiveWorkObservation.cs:3-6`,
//! `ActiveWorkState.cs:3-6`, `Process/ShellProcessStatusSnapshot.cs:5-12`,
//! `IProcessOwner.cs:53-57`, and `Parrot.Tool.Process/Process/ShellProcessOwner.cs:172-199`.
//! Runtime status query/provider assembly remains runtime-local. The production
//! host adapter remains open in P08/P05 T41; empty test observers are not backend
//! proof. This port neither enumerates global OS processes nor launches, signals,
//! or grants access to processes.

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActiveWorkState {
    Running,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveWorkObservation {
    pub id: String,
    pub name: String,
    pub state: ActiveWorkState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessStatusSnapshot {
    pub owner_session_id: String,
    /// Logical source identity, not an OS numeric process identifier.
    pub process_id: String,
    pub name: String,
    pub state: ActiveWorkState,
}

impl ProcessStatusSnapshot {
    pub fn id(&self) -> String {
        format!("{}/{}", self.owner_session_id, self.name)
    }
}

pub trait ProcessStatusSource: Send + Sync {
    /// Captures non-retired, turn-blocking work, ordered by ordinal observation ID.
    /// Completed-but-undelivered processes remain `Running`; this is not an
    /// OS-alive filter.
    fn active(&self) -> Vec<ActiveWorkObservation>;

    /// Captures non-retired processes, ordered by ordinal logical process ID,
    /// including completed-but-undelivered processes with state `Running`.
    fn snapshot(&self) -> Vec<ProcessStatusSnapshot>;
}

#[cfg(test)]
mod tests {
    use super::{
        ActiveWorkObservation, ActiveWorkState, ProcessStatusSnapshot, ProcessStatusSource,
    };

    #[test]
    fn snapshot_id_is_derived_from_current_owner_and_name() {
        let mut snapshot = ProcessStatusSnapshot {
            owner_session_id: "owner/session".into(),
            process_id: "logical-process-id".into(),
            name: " original name ".into(),
            state: ActiveWorkState::Running,
        };

        assert_eq!(snapshot.id(), "owner/session/ original name ");
        snapshot.name = "renamed/work".into();
        assert_eq!(snapshot.id(), "owner/session/renamed/work");
    }

    #[test]
    fn empty_observer_has_synchronous_object_safe_signatures() {
        struct EmptyObserver;

        impl ProcessStatusSource for EmptyObserver {
            fn active(&self) -> Vec<ActiveWorkObservation> {
                Vec::new()
            }

            fn snapshot(&self) -> Vec<ProcessStatusSnapshot> {
                Vec::new()
            }
        }

        let _: &dyn ProcessStatusSource = &EmptyObserver;
        let _: fn(&dyn ProcessStatusSource) -> Vec<ActiveWorkObservation> =
            |source| source.active();
        let _: fn(&dyn ProcessStatusSource) -> Vec<ProcessStatusSnapshot> =
            |source| source.snapshot();
    }
}
