//! Private library-unit-test migration crash cuts. Included only by cfg(test).
#[path = "observe.rs"]
mod observe;

use crate::database::{Connection, DatabaseError};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const CHILD: &str = "database::schema::g16_process_kill::child";

fn selected_path(connection: &Connection, cut: &str) -> Option<PathBuf> {
    if std::env::var("T16_G16_CHILD_CUT").ok().as_deref() != Some(cut) {
        return None;
    }
    let path = PathBuf::from(std::env::var_os("T16_G16_CHILD_DB")?);
    if connection.path().map(Path::new) != Some(path.as_path()) {
        return None;
    }
    Some(path)
}

fn write_new(path: &Path, value: &Value) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    serde_json::to_writer(&mut file, value)?;
    file.write_all(b"\n")?;
    file.flush()?;
    file.sync_all()?;
    Ok(())
}

/// Captures only already committed work, before the selected migration begins.
pub(super) fn baseline(connection: &Connection, cut: &str) {
    let Some(path) = selected_path(connection, cut) else {
        return;
    };
    let workspace = path.parent().unwrap().parent().unwrap().parent().unwrap();
    write_new(
        &workspace.join("baseline.json"),
        &observe::inspect(connection, &path).unwrap(),
    )
    .unwrap();
}

/// The ready file is written only after all migration writes, before COMMIT.
pub(super) fn pause(connection: &Connection, cut: &str) {
    let Some(path) = selected_path(connection, cut) else {
        return;
    };
    let workspace = path.parent().unwrap().parent().unwrap().parent().unwrap();
    write_new(
        &workspace.join("ready.json"),
        &json!({"cut":cut,"pid":std::process::id(),"path":path}),
    )
    .unwrap();
    println!("T16_G16_READY {cut}");
    std::io::stdout().flush().unwrap();
    loop {
        std::thread::park();
    }
}

fn hash(path: &Path) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(std::fs::read(path)?)))
}

fn tree(root: &Path) -> Result<BTreeMap<String, Value>> {
    fn walk(root: &Path, path: &Path, entries: &mut BTreeMap<String, Value>) -> Result<()> {
        let metadata = std::fs::symlink_metadata(path)?;
        let entry = if metadata.is_symlink() {
            json!({"type":"symlink","mode":metadata.mode(),"target":std::fs::read_link(path)?})
        } else if metadata.is_dir() {
            json!({"type":"directory","mode":metadata.mode()})
        } else if metadata.is_file() {
            json!({"type":"file","mode":metadata.mode(),"sha256":hash(path)?})
        } else {
            return Err("unexpected runtime entry".into());
        };
        entries.insert(
            path.strip_prefix(root)?.to_string_lossy().into_owned(),
            entry,
        );
        if metadata.is_dir() {
            for entry in std::fs::read_dir(path)? {
                walk(root, &entry?.path(), entries)?;
            }
        }
        Ok(())
    }
    let mut entries = BTreeMap::new();
    walk(root, root, &mut entries)?;
    Ok(entries)
}

fn journal(path: &Path) -> Result<Value> {
    let journal = path.with_extension("db-journal");
    Ok(if journal.exists() {
        json!({"exists":true,"bytes":std::fs::metadata(&journal)?.len(),
            "first_byte":std::fs::read(&journal)?.first().copied(),"sha256":hash(&journal)?})
    } else {
        json!({"exists":false})
    })
}

fn payloads(observation: &Value) -> Vec<Value> {
    observation["tables"]["event"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| json!({"sequence":row["sequence"],"payload":row["payload"]}))
        .collect()
}

struct Reap(Child);
impl Drop for Reap {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn run(cut: &str) -> Result<()> {
    let workspace = std::env::temp_dir().join(format!(
        "parrot-t16-kill-{cut}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    std::fs::create_dir(&workspace)?;
    let selected = workspace.join("selected/same-id");
    let unselected = workspace.join("unselected/same-id");
    std::fs::create_dir_all(&selected)?;
    std::fs::create_dir_all(&unselected)?;
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/database/fixtures/legacy-canonical-before.sqlite");
    let fixture_hash = hash(&fixture)?;
    let path = selected.join("session.db");
    std::fs::copy(&fixture, &path)?;
    std::fs::copy(&fixture, unselected.join("session.db"))?;
    assert_eq!(hash(&path)?, fixture_hash);
    assert_eq!(hash(&unselected.join("session.db"))?, fixture_hash);
    std::fs::write(selected.join("keep"), b"selected marker")?;
    std::fs::write(unselected.join("keep"), b"unselected marker")?;
    std::os::unix::fs::symlink("keep", selected.join("keep-link"))?;
    std::os::unix::fs::symlink("keep", unselected.join("keep-link"))?;
    let original = observe::inspect_raw(&path)?;
    let selected_before = tree(&selected)?;
    let unselected_before = tree(&unselected)?;
    let executable = std::env::current_exe()?;
    let executable_hash = hash(&executable)?;
    let child_stdout = workspace.join("child.stdout");
    let child_stderr = workspace.join("child.stderr");
    let mut child = Reap(
        Command::new(&executable)
            .args(["--exact", CHILD, "--ignored", "--nocapture"])
            .env("T16_G16_CHILD_CUT", cut)
            .env("T16_G16_CHILD_DB", &path)
            .stdout(Stdio::from(std::fs::File::create(&child_stdout)?))
            .stderr(Stdio::from(std::fs::File::create(&child_stderr)?))
            .spawn()?,
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    let ready_path = workspace.join("ready.json");
    loop {
        // Both the complete file and flushed stdout marker must be present.
        if ready_path.exists()
            && std::fs::read_to_string(&child_stdout)?.contains(&format!("T16_G16_READY {cut}"))
        {
            break;
        }
        if let Some(status) = child.0.try_wait()? {
            return Err(format!("child exited before barrier: {status}").into());
        }
        if Instant::now() >= deadline {
            return Err("migration barrier timed out".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let ready: Value = serde_json::from_slice(&std::fs::read(&ready_path)?)?;
    assert_eq!(ready["cut"], cut);
    assert_eq!(ready["pid"], child.0.id());
    assert_eq!(ready["path"], json!(path));
    child.0.kill()?; // std's Unix Child::kill sends SIGKILL, not a graceful exit.
    let status = child.0.wait()?;
    assert_eq!(status.signal(), Some(9));
    let selected_after_death = tree(&selected)?;
    let unselected_after_death = tree(&unselected)?;
    let journal_before_recovery = journal(&path)?;
    assert_eq!(
        observe::sidecars(&path),
        json!({"wal_exists":false,"shm_exists":false})
    );
    let baseline: Value = serde_json::from_slice(&std::fs::read(workspace.join("baseline.json"))?)?;
    let recovered = {
        let connection = Connection::open(&path)?;
        // Raw SQLite recovery, not another native migration. Match connection-local
        // observation settings; no schema/data mutation is introduced here.
        connection.execute_batch("PRAGMA journal_mode=TRUNCATE; PRAGMA foreign_keys=ON;")?;
        observe::inspect(&connection, &path)?
    };
    assert_eq!(
        baseline, recovered,
        "only the selected uncommitted migration rolls back"
    );
    assert_eq!(payloads(&original), payloads(&recovered));
    if cut == "usage-before-commit" {
        assert!(
            !recovered["tables"]["event"]["columns"]
                .as_array()
                .unwrap()
                .iter()
                .any(|column| column["name"] == "request_id")
        );
    } else {
        assert!(
            recovered["tables"]["event"]["columns"]
                .as_array()
                .unwrap()
                .iter()
                .any(|column| column["name"] == "request_id")
        );
        assert!(recovered["tables"]["agent_lineage"].is_null());
    }
    let journal_after_recovery = journal(&path)?;
    // SQLite ignores an unsynced zero-header journal: a read-only reopen need
    // not truncate it. Any retained nonempty journal must now be inactive.
    if journal_after_recovery["exists"] == true && journal_after_recovery["bytes"] != 0 {
        assert_eq!(journal_after_recovery["first_byte"], 0);
    }
    let selected_after_recovery = tree(&selected)?;
    let database = crate::database::SessionDatabase::open(
        parrot_domain::contracts::paths::SelectedSessionRoot::new(
            selected.clone(),
            parrot_domain::contracts::paths::RootOrigin::Current,
        )?,
    )?;
    database.close().await?;
    // The successful migration write commits in TRUNCATE mode and must finalize
    // the journal. Do not delete it ourselves or impose this on read-only Open.
    let journal_after_completion = journal(&path)?;
    if journal_after_completion["exists"] == true {
        assert_eq!(journal_after_completion["bytes"], 0);
    }
    let completed = observe::inspect_raw(&path)?;
    assert_eq!(payloads(&original), payloads(&completed));
    assert!(completed["tables"]["agent_lineage"].is_object());
    let selected_after = tree(&selected)?;
    let unselected_after = tree(&unselected)?;
    assert_eq!(unselected_before, unselected_after_death);
    assert_eq!(unselected_before, unselected_after);
    for snapshot in [
        &selected_after_death,
        &selected_after_recovery,
        &selected_after,
    ] {
        for (name, entry) in &selected_before {
            if name != "session.db" {
                assert_eq!(snapshot.get(name), Some(entry));
            }
        }
        for (name, entry) in snapshot {
            if name == "session.db" || name == "session.db-journal" {
                assert_eq!(entry["type"], "file");
                if let Some(before) = selected_before.get(name) {
                    assert_eq!(entry["mode"], before["mode"]);
                }
            } else {
                assert_eq!(selected_before.get(name), Some(entry));
            }
        }
    }
    assert_eq!(
        observe::sidecars(&path),
        json!({"wal_exists":false,"shm_exists":false})
    );
    assert_eq!(hash(&fixture)?, fixture_hash);
    let evidence = json!({"schema_version":1,"cut":cut,"workspace":workspace,"database":path,
        "fixture":fixture,"fixture_sha256":fixture_hash,"unit_test_elf":executable,"unit_test_elf_sha256":executable_hash,
        "child_filter":CHILD,"ready":ready,"termination_signal":status.signal(),
        "child_stdout":std::fs::read_to_string(&child_stdout)?,"child_stderr":std::fs::read_to_string(&child_stderr)?,
        "original":original,"committed_baseline":baseline,"recovered":recovered,"completed":completed,
        "journal-before-recovery":journal_before_recovery,"journal-after-recovery":journal_after_recovery,
        "journal-after-completion":journal_after_completion,
        "selected-before":selected_before,"selected-after-death":selected_after_death,
        "selected-after-recovery":selected_after_recovery,"selected-after":selected_after,
        "unselected-before":unselected_before,"unselected-after-death":unselected_after_death,"unselected-after":unselected_after});
    write_new(&workspace.join("protocol.json"), &evidence)?;
    println!("T16_G16_RESULT {}", serde_json::to_string(&evidence)?);
    Ok(())
}

#[test]
#[ignore = "dedicated subprocess; requires an exact private cut and selected path"]
fn child() {
    let cut = std::env::var("T16_G16_CHILD_CUT").unwrap();
    assert!(["usage-before-commit", "lineage-before-commit"].contains(&cut.as_str()));
    let path = PathBuf::from(std::env::var_os("T16_G16_CHILD_DB").unwrap());
    let _connection = super::open(&path).unwrap();
    panic!("selected migration barrier was not reached");
}

#[tokio::test]
async fn migration_process_death_usage_before_commit() {
    run("usage-before-commit").await.unwrap();
}

#[tokio::test]
async fn migration_process_death_lineage_before_commit() {
    run("lineage-before-commit").await.unwrap();
}
