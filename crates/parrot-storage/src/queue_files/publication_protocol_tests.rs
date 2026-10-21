//! G22 process-death protocol. Included only in the library unit-test executable.
use super::{Direction, ErrorKind, QueueStore};
use std::env;
use std::fs;
use std::io::Write;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PublicationKind {
    CreateHardLink,
    OverwriteRename,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PublicationPoint {
    BeforePublicationAfterTempSync,
    AfterPublicationBeforeCleanup,
}

pub(super) fn publication_barrier(kind: PublicationKind, point: PublicationPoint, target: &Path) {
    if env::var("T22_QUEUE_PROBE_CHILD").as_deref() != Ok("g22-publication-v1")
        || env::var("T22_QUEUE_PROBE_KIND").ok().as_deref() != Some(&format!("{kind:?}"))
        || env::var("T22_QUEUE_PROBE_POINT").ok().as_deref() != Some(&format!("{point:?}"))
        || env::var_os("T22_QUEUE_PROBE_TARGET").as_deref() != Some(target.as_os_str())
    {
        return;
    }
    let ready = env::var_os("T22_QUEUE_PROBE_READY").expect("dedicated ready path");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(ready)
        .unwrap();
    writeln!(file, "{} {kind:?} {point:?}", std::process::id()).unwrap();
    file.sync_all().unwrap();
    loop {
        std::thread::park();
    }
}

#[test]
#[ignore = "dedicated G22 child; parent owns and kills this process"]
fn probe_child() {
    assert_eq!(
        env::var("T22_QUEUE_PROBE_CHILD").unwrap(),
        "g22-publication-v1"
    );
    let target = std::path::PathBuf::from(env::var_os("T22_QUEUE_PROBE_TARGET").unwrap());
    let name = target.file_stem().unwrap().to_str().unwrap();
    let store = QueueStore::new(target.parent().unwrap()).unwrap();
    match env::var("T22_QUEUE_PROBE_KIND").unwrap().as_str() {
        "CreateHardLink" => {
            store.create(name, "G22 publication").unwrap();
        }
        "OverwriteRename" => {
            store
                .push(
                    name,
                    &["g22-after-publication".to_owned()],
                    Direction::Back,
                    true,
                )
                .unwrap();
        }
        _ => panic!("unknown publication kind"),
    }
    panic!("child passed its configured barrier");
}

#[test]
fn parent_protocol() {
    // Ordinary test runs do not launch a process-death protocol.
    if env::var("T22_QUEUE_PROTOCOL").as_deref() != Ok("g22-publication-v1") {
        return;
    }
    let kind = env::var("T22_QUEUE_PROBE_KIND").unwrap();
    let point = env::var("T22_QUEUE_PROBE_POINT").unwrap();
    assert!(matches!(
        kind.as_str(),
        "CreateHardLink" | "OverwriteRename"
    ));
    assert!(matches!(
        point.as_str(),
        "BeforePublicationAfterTempSync" | "AfterPublicationBeforeCleanup"
    ));
    let target = std::path::PathBuf::from(env::var_os("T22_QUEUE_PROBE_TARGET").unwrap());
    let ready = std::path::PathBuf::from(env::var_os("T22_QUEUE_PROBE_READY").unwrap());
    assert!(!ready.exists());
    let executable = env::current_exe().unwrap();
    let mut child = Command::new(&executable)
        .args([
            "--exact",
            "queue_files::publication_protocol_tests::probe_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("T22_QUEUE_PROBE_CHILD", "g22-publication-v1")
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready.exists() {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("publication child exited before readiness: {status}");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let status = child.wait().unwrap();
            panic!("publication barrier timed out; child status {status}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    child.kill().unwrap();
    let status = child.wait().unwrap();
    assert_eq!(status.signal(), Some(9));
    // run_publication.py snapshots the killed tree before this separate read-only reopen.
    let trace = serde_json::json!({
        "kind": kind, "point": point, "pid": child.id(), "signal": status.signal(),
        "unit_test_executable": executable, "target": target,
        "ready": fs::read_to_string(&ready).unwrap(),
    });
    fs::write(
        env::var_os("T22_QUEUE_PROBE_TRACE").unwrap(),
        serde_json::to_vec_pretty(&trace).unwrap(),
    )
    .unwrap();
}

#[test]
fn reopen_protocol() {
    if env::var("T22_QUEUE_PROTOCOL").as_deref() != Ok("g22-readonly-v1") {
        return;
    }
    let target = std::path::PathBuf::from(env::var_os("T22_QUEUE_PROBE_TARGET").unwrap());
    let name = target.file_stem().unwrap().to_str().unwrap();
    let store = QueueStore::new(target.parent().unwrap()).unwrap();
    let list = store.list().unwrap();
    let absent = env::var("T22_QUEUE_PROBE_KIND").as_deref() == Ok("CreateHardLink")
        && env::var("T22_QUEUE_PROBE_POINT").as_deref() == Ok("BeforePublicationAfterTempSync");
    let metadata = if absent {
        assert_eq!(store.get(name).unwrap_err().kind, ErrorKind::NotFound);
        assert!(list.is_empty());
        serde_json::Value::Null
    } else {
        let info = store.get(name).unwrap();
        assert_eq!(list.len(), 1);
        let overwrite = env::var("T22_QUEUE_PROBE_KIND").as_deref() == Ok("OverwriteRename");
        let after =
            env::var("T22_QUEUE_PROBE_POINT").as_deref() == Ok("AfterPublicationBeforeCleanup");
        assert_eq!(info.closed, overwrite && after);
        assert_eq!(
            info.size,
            if overwrite {
                if after { 5 } else { 4 }
            } else {
                0
            }
        );
        serde_json::json!({"name": info.name, "description": info.description, "size": info.size, "closed": info.closed})
    };
    fs::write(
        env::var_os("T22_QUEUE_PROBE_REOPEN").unwrap(),
        serde_json::to_vec_pretty(&metadata).unwrap(),
    )
    .unwrap();
}
