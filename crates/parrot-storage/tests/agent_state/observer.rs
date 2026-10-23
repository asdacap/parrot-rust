use super::*;

#[tokio::test]
async fn dropping_an_unpolled_observer_does_not_admit_a_write() {
    let fixture = Fixture::new();
    let store = fixture.store().await;
    let expected = snapshot();
    store.initialize(expected.clone()).await.unwrap();
    drop(store.update(AgentStateUpdate::Execution(None)));
    assert_eq!(store.load().await.unwrap(), Some(expected));
}

#[tokio::test]
async fn dropping_an_admitted_observer_does_not_abandon_its_write() {
    let fixture = Fixture::new();
    let store = fixture.store().await;
    store.initialize(snapshot()).await.unwrap();
    let mut observer = store.update(AgentStateUpdate::Execution(None));
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    if let std::task::Poll::Ready(result) = observer.as_mut().poll(&mut context) {
        result.unwrap();
    }
    drop(observer);
    let actual = store.load().await.unwrap().unwrap();
    assert_eq!(actual.execution, None);
    assert_eq!(actual.revision, snapshot().revision + 1);
}

#[cfg(unix)]
#[tokio::test]
async fn denied_snapshot_read_is_io_not_a_path_error() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let store = fixture.store().await;
    store.initialize(snapshot()).await.unwrap();
    fs::set_permissions(fixture.path(), fs::Permissions::from_mode(0o000)).unwrap();
    let result = store.load().await;
    fs::set_permissions(fixture.path(), fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(result.unwrap_err().kind, AgentStateErrorKind::Io);
}
