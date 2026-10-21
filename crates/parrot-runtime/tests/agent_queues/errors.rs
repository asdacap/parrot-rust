use parrot_runtime::{agent_queues::QueueError, children::ChildError};
use parrot_storage::queue_files::{Error, ErrorKind};

#[test]
fn queue_failure_classification_matches_source_exception_families_without_walking_inner_causes() {
    for (kind, code) in [
        (ErrorKind::Queue, "unexpected"),
        (ErrorKind::NotFound, "unexpected"),
        (ErrorKind::AlreadyExists, "unexpected"),
        (ErrorKind::Closed, "unexpected"),
        (ErrorKind::Empty, "unexpected"),
        (ErrorKind::Disposed, "invalid_operation"),
        (ErrorKind::Inventory, "invalid_operation"),
        (ErrorKind::Argument, "argument"),
        (ErrorKind::Io, "io"),
        (ErrorKind::UnauthorizedAccess, "access_denied"),
        (ErrorKind::Cancelled, "cancelled"),
        (ErrorKind::CancelledBeforeWait, "cancelled"),
    ] {
        assert_eq!(
            ChildError::from(QueueError::Storage(Error::new(kind, "private details"))).code(),
            code,
            "{kind:?}"
        );
    }
    let wrapper = Error {
        kind: ErrorKind::Queue,
        message: "queue wrapper".into(),
        info: None,
        inner: Some(Box::new(Error::new(
            ErrorKind::Io,
            "private inner I/O details",
        ))),
    };
    assert_eq!(
        ChildError::from(QueueError::Storage(wrapper)).code(),
        "unexpected"
    );
}
