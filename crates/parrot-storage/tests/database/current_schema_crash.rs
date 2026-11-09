use super::Connection;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const CHILD: &str = "database::schema::current_schema_crash::child";

pub(super) fn wait_before_commit(connection: &Connection) {
    let Some(path) = std::env::var_os("PARROT_SCHEMA_INIT_KILL_DB") else {
        return;
    };
    if connection.path().map(Path::new) != Some(Path::new(&path)) {
        return;
    }
    let ready = std::env::var_os("PARROT_SCHEMA_INIT_KILL_READY").unwrap();
    std::fs::write(ready, b"ready").unwrap();
    loop {
        std::thread::park();
    }
}

struct Reap(Child);
impl Drop for Reap {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "child runner for the initialization process-kill check"]
fn child() {
    let path = std::env::var_os("PARROT_SCHEMA_INIT_KILL_DB").unwrap();
    super::open(Path::new(&path)).unwrap();
    panic!("initialization child reached commit");
}

#[test]
fn interrupted_initialization_rolls_back_schema_and_version_then_reopens() {
    let workspace = std::env::temp_dir().join(format!(
        "parrot-schema-init-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&workspace).unwrap();
    for recover_first in [false, true] {
        let path = workspace.join(format!("session-{recover_first}.db"));
        let ready = workspace.join(format!("before-commit-{recover_first}"));
        let mut child = Reap(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", CHILD, "--ignored", "--nocapture"])
                .env("PARROT_SCHEMA_INIT_KILL_DB", &path)
                .env("PARROT_SCHEMA_INIT_KILL_READY", &ready)
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(20);
        while std::fs::read(&ready).ok().as_deref() != Some(b"ready") {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "initialization child exited before the commit cut"
            );
            assert!(Instant::now() < deadline, "initialization child timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
        child.0.kill().unwrap();
        assert!(!child.0.wait().unwrap().success());
        if recover_first {
            let recovered = Connection::open(&path).unwrap();
            let version: i64 = recovered
                .pragma_query_value(None, "user_version", |row| row.get(0))
                .unwrap();
            let tables: i64 = recovered
                .query_row(
                    "SELECT count(*) FROM sqlite_schema WHERE name NOT GLOB 'sqlite_*';",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!((version, tables), (0, 0));
            drop(recovered);
        }

        let reopened = super::open(&path).unwrap();
        let version: i64 = reopened
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        let tables: i64 = reopened
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE type='table' AND name NOT GLOB 'sqlite_*';",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!((version, tables), (1, 15));
        drop(reopened);
    }
    std::fs::remove_dir_all(workspace).unwrap();
}
