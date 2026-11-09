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
        assert_eq!((version, tables), (2, 13));
        drop(reopened);
    }
    std::fs::remove_dir_all(workspace).unwrap();
}

#[test]
#[ignore = "child runner for the spilled-transaction recovery check"]
fn spill_child() {
    let path = std::env::var_os("PARROT_SCHEMA_SPILL_DB").unwrap();
    let connection = Connection::open(path).unwrap();
    connection
        .execute_batch(
            "PRAGMA cache_size=8; BEGIN IMMEDIATE; \
             UPDATE recovery_probe SET payload=zeroblob(65536);",
        )
        .unwrap();
    let ready = std::env::var_os("PARROT_SCHEMA_SPILL_READY").unwrap();
    std::fs::write(ready, b"ready").unwrap();
    loop {
        std::thread::park();
    }
}

#[test]
fn current_format_recovers_spilled_transactions_and_unsupported_formats_stay_unchanged() {
    let workspace = std::env::temp_dir().join(format!(
        "parrot-schema-spill-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&workspace).unwrap();
    let payload = vec![b'c'; 32768];
    let cases = vec![
        (2_i64, std::ffi::OsString::from("session-2.db")),
        (1, std::ffi::OsString::from("session-1.db")),
        (0, std::ffi::OsString::from("session-0.db")),
        (7, std::ffi::OsString::from("session-7.db")),
    ];
    #[cfg(unix)]
    let cases = {
        use std::os::unix::ffi::OsStringExt;

        let mut cases = cases;
        cases.push((2, std::ffi::OsString::from("native ?#%2F-é.db")));
        cases.push((
            2,
            std::ffi::OsString::from_vec(b"native-\xff?#%.db".to_vec()),
        ));
        cases
    };
    for (case, (version, name)) in cases.into_iter().enumerate() {
        let path = workspace.join(name);
        let ready = workspace.join(format!("spilled-{case}"));
        let mut connection = super::open(&path).unwrap();
        connection
            .execute_batch("CREATE TABLE recovery_probe(payload BLOB NOT NULL);")
            .unwrap();
        let transaction = connection.transaction().unwrap();
        for _ in 0..32 {
            transaction
                .execute("INSERT INTO recovery_probe VALUES (?1)", [&payload])
                .unwrap();
        }
        transaction.commit().unwrap();
        connection
            .pragma_update(None, "user_version", version)
            .unwrap();
        drop(connection);

        let mut child = Reap(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "database::schema::current_schema_crash::spill_child",
                    "--ignored",
                    "--nocapture",
                ])
                .env("PARROT_SCHEMA_SPILL_DB", &path)
                .env("PARROT_SCHEMA_SPILL_READY", &ready)
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(20);
        while std::fs::read(&ready).ok().as_deref() != Some(b"ready") {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "spill child exited before its transaction was ready"
            );
            assert!(Instant::now() < deadline, "spill child timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
        child.0.kill().unwrap();
        assert!(!child.0.wait().unwrap().success());
        let journal_path = path.with_extension("db-journal");
        let database_before = std::fs::read(&path).unwrap();
        let journal_before = std::fs::read(&journal_path).unwrap();
        assert!(journal_before.len() > 100_000);

        if version == 2 {
            let recovered = super::open(&path).unwrap();
            let retained: i64 = recovered
                .query_row(
                    "SELECT count(*) FROM recovery_probe WHERE payload=?1",
                    [&payload],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(retained, 32);
        } else {
            assert!(matches!(
                super::open(&path),
                Err(super::DatabaseError::UnsupportedSchema { version: found }) if found == version
            ));
            assert_eq!(std::fs::read(&path).unwrap(), database_before);
            assert_eq!(std::fs::read(&journal_path).unwrap(), journal_before);
        }
    }
    std::fs::remove_dir_all(workspace).unwrap();
}

#[test]
#[ignore = "child runner for the WAL-only unsupported-format check"]
fn wal_child() {
    let path = std::env::var_os("PARROT_SCHEMA_WAL_DB").unwrap();
    let connection = Connection::open(path).unwrap();
    connection
        .execute_batch(
            "PRAGMA journal_mode=WAL; \
             CREATE TABLE foreign_state(value TEXT); \
             INSERT INTO foreign_state VALUES ('kept'); \
             PRAGMA user_version=7;",
        )
        .unwrap();
    let ready = std::env::var_os("PARROT_SCHEMA_WAL_READY").unwrap();
    std::fs::write(ready, b"ready").unwrap();
    loop {
        std::thread::park();
    }
}

#[test]
fn wal_only_unknown_format_is_rejected_without_changing_committed_state() {
    let workspace = std::env::temp_dir().join(format!(
        "parrot-schema-wal-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&workspace).unwrap();
    let path = workspace.join("session.db");
    let ready = workspace.join("wal-committed");
    let mut child = Reap(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "database::schema::current_schema_crash::wal_child",
                "--ignored",
                "--nocapture",
            ])
            .env("PARROT_SCHEMA_WAL_DB", &path)
            .env("PARROT_SCHEMA_WAL_READY", &ready)
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    while std::fs::read(&ready).ok().as_deref() != Some(b"ready") {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "WAL child exited before its committed state was ready"
        );
        assert!(Instant::now() < deadline, "WAL child timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
    child.0.kill().unwrap();
    assert!(!child.0.wait().unwrap().success());
    let wal_path = path.with_extension("db-wal");
    let database_before = std::fs::read(&path).unwrap();
    let wal_before = std::fs::read(&wal_path).unwrap();
    assert!(wal_before.len() > 32);

    assert!(matches!(
        super::open(&path),
        Err(super::DatabaseError::UnsupportedSchema { version: 7 })
    ));
    assert_eq!(std::fs::read(&path).unwrap(), database_before);
    assert_eq!(std::fs::read(&wal_path).unwrap(), wal_before);
    let readonly =
        Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let version: i64 = readonly
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    let value: String = readonly
        .query_row("SELECT value FROM foreign_state", [], |row| row.get(0))
        .unwrap();
    assert_eq!((version, value), (7, "kept".into()));
    drop(readonly);
    std::fs::remove_dir_all(workspace).unwrap();
}

#[cfg(unix)]
#[test]
fn schema_preflight_preserves_literal_uri_filename_bytes() {
    let workspace = std::env::temp_dir().join(format!(
        "parrot-schema-filenames-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&workspace).unwrap();
    let names = [
        "question?immutable=0&mode=rw.db",
        "fragment#ignored.db",
        "percent%2Fslash%23.db",
        "space and unicode-é.db",
    ];
    for name in names {
        let path = workspace.join(name);
        let connection = super::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE path_probe(value TEXT); INSERT INTO path_probe VALUES ('kept');",
            )
            .unwrap();
        drop(connection);
        let reopened = super::open(&path).unwrap();
        let value: String = reopened
            .query_row("SELECT value FROM path_probe", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, "kept");
        reopened.pragma_update(None, "user_version", 7).unwrap();
        drop(reopened);
        let before = std::fs::read(&path).unwrap();
        assert!(matches!(
            super::open(&path),
            Err(super::DatabaseError::UnsupportedSchema { version: 7 })
        ));
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
    std::fs::remove_dir_all(workspace).unwrap();
}

#[cfg(unix)]
#[test]
fn schema_preflight_preserves_non_utf8_filename_bytes() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let workspace = std::env::temp_dir().join(format!(
        "parrot-schema-native-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&workspace).unwrap();
    let path = workspace.join(OsString::from_vec(b"native-\xff?#%.db".to_vec()));
    let connection = super::open(&path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE path_probe(value TEXT); INSERT INTO path_probe VALUES ('kept');",
        )
        .unwrap();
    drop(connection);
    let reopened = super::open(&path).unwrap();
    let value: String = reopened
        .query_row("SELECT value FROM path_probe", [], |row| row.get(0))
        .unwrap();
    assert_eq!(value, "kept");
    drop(reopened);
    std::fs::remove_dir_all(workspace).unwrap();
}
