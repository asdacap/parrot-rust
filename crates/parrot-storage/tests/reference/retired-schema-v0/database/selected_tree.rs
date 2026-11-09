//! Finite selected-root safety recipe; this does not exercise root discovery.
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_domain::contracts::paths::{RootOrigin, SelectedSessionRoot};
use parrot_storage::database::SessionDatabase;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
struct Active;
impl Cancellation for Active {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}

fn hash(path: &Path) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(std::fs::read(path)?)))
}

fn tree(root: &Path) -> Result<BTreeMap<String, Value>> {
    fn walk(root: &Path, path: &Path, entries: &mut BTreeMap<String, Value>) -> Result<()> {
        let metadata = std::fs::symlink_metadata(path)?;
        let relative = path.strip_prefix(root)?.to_string_lossy().into_owned();
        let entry = if metadata.is_symlink() {
            json!({"type":"symlink", "mode":metadata.mode(), "target":std::fs::read_link(path)?})
        } else if metadata.is_dir() {
            json!({"type":"directory", "mode":metadata.mode()})
        } else if metadata.is_file() {
            json!({"type":"file", "mode":metadata.mode(), "sha256":hash(path)?})
        } else {
            return Err(format!("unexpected entry: {}", path.display()).into());
        };
        entries.insert(relative, entry);
        if metadata.is_dir() {
            for child in std::fs::read_dir(path)? {
                walk(root, &child?.path(), entries)?;
            }
        }
        Ok(())
    }
    let mut entries = BTreeMap::new();
    walk(root, root, &mut entries)?;
    Ok(entries)
}

fn allowed_changes(before: &BTreeMap<String, Value>, after: &BTreeMap<String, Value>) {
    for (name, entry) in before {
        if name != "session.db" {
            assert_eq!(after.get(name), Some(entry), "changed {name}");
        }
    }
    for (name, entry) in after {
        if name == "session.db" {
            assert_eq!(entry["type"], "file");
            if let Some(old) = before.get(name) {
                assert_eq!(entry["mode"], old["mode"]);
            }
        } else if name == "session.db-journal" {
            assert_eq!(entry["type"], "file");
        } else {
            assert_eq!(before.get(name), Some(entry), "unapproved entry {name}");
        }
    }
}

/// Uses only synthetic copies of the existing independent fixtures.
pub async fn suite(workspace: &Path, fixtures: &Path) -> Result<Value> {
    let source = fixtures.join("legacy-canonical-before.sqlite");
    let source_hash = hash(&source)?;
    let mut cases = Vec::new();
    for case in ["fresh-selected", "legacy-selected"] {
        let case_root = workspace.join(case);
        let selected = case_root.join("selected/same-id");
        let unselected = case_root.join("unselected/same-id");
        // Refuse accidental reuse of a previous runtime tree.
        std::fs::create_dir(&case_root)?;
        std::fs::create_dir_all(&selected)?;
        std::fs::create_dir_all(&unselected)?;
        std::fs::write(selected.join("keep"), b"selected marker")?;
        std::fs::write(unselected.join("keep"), b"unselected marker")?;
        std::os::unix::fs::symlink("keep", selected.join("keep-link"))?;
        std::os::unix::fs::symlink("keep", unselected.join("keep-link"))?;
        std::fs::copy(&source, unselected.join("session.db"))?;
        assert_eq!(hash(&unselected.join("session.db"))?, source_hash);
        let path = selected.join("session.db");
        let before = if case == "legacy-selected" {
            std::fs::copy(&source, &path)?;
            assert_eq!(hash(&path)?, source_hash);
            Some(super::observe::inspect_raw(&path)?)
        } else {
            assert!(!path.exists());
            None
        };
        let selected_before = tree(&selected)?;
        let unselected_before = tree(&unselected)?;
        let database = SessionDatabase::open(SelectedSessionRoot::new(
            selected.clone(),
            RootOrigin::Current,
        )?)?;
        let observed_path = path.clone();
        let opened = database
            .admit("selected-tree-inspect", &Active, move |tx| {
                super::observe::inspect(tx, &observed_path)
            })
            .map_err(|error| format!("inspection admission: {error:?}"))?
            .into_completion()
            .await?
            .into_value()
            .into_output();
        database.close().await?;
        let after = super::observe::inspect_raw(&path)?;
        if let Some(before) = &before {
            let before_payloads: Vec<_> = before["tables"]["event"]["rows"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| (&row["sequence"], &row["payload"]))
                .collect();
            let after_payloads: Vec<_> = after["tables"]["event"]["rows"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| (&row["sequence"], &row["payload"]))
                .collect();
            assert_eq!(before_payloads, after_payloads);
            assert!(after["tables"]["agent_lineage"].is_object());
            assert!(
                after["tables"]["event"]["columns"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|column| column["name"] == "request_id")
            );
        } else {
            for table in after["tables"].as_object().unwrap().values() {
                assert!(
                    table["rows"].as_array().unwrap().is_empty(),
                    "fresh root borrowed facts"
                );
            }
        }
        assert_eq!(opened["journal_mode"], json!([{"journal_mode":"truncate"}]));
        assert_eq!(
            after["sidecars"],
            json!({"wal_exists":false,"shm_exists":false})
        );
        let journal = path.with_extension("db-journal");
        if journal.exists() {
            assert_eq!(std::fs::metadata(&journal)?.len(), 0);
        }
        let selected_after = tree(&selected)?;
        let unselected_after = tree(&unselected)?;
        allowed_changes(&selected_before, &selected_after);
        assert_eq!(unselected_before, unselected_after);
        cases.push(
            json!({"case_id":case, "fixture_sha256":source_hash, "before":before,
            "opened":opened, "after":after, "selected-before":selected_before,
            "selected-after":selected_after, "unselected-before":unselected_before,
            "unselected-after":unselected_after}),
        );
    }
    assert_eq!(hash(&source)?, source_hash);
    Ok(json!({"schema_version":1,"cases":cases}))
}

#[tokio::test]
async fn selected_creation_and_legacy_upgrade_do_not_borrow_or_mutate_counterpart() {
    let workspace = std::env::temp_dir().join(format!(
        "parrot-t16-selected-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&workspace).unwrap();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/database/fixtures");
    let result = suite(&workspace, &fixtures).await.unwrap();
    println!("{}", json!({"workspace":workspace,"result":result}));
    // Keep the exact runtime trees for an authorized capture's follow-on reader.
}
