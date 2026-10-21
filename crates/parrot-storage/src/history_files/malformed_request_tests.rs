//! Concrete malformed request-event history observations; no injected repository.
use super::AgentHistoryFile;
use crate::conversation::{
    ConversationMessage, ConversationRepository, HistoryProjection, MessageRole,
};
use crate::database::{DatabaseError, SessionDatabase};
use crate::events::{EventError, EventRepository};
use parrot_domain::contracts::paths::{RootOrigin, SelectedSessionRoot};
use parrot_domain::protocol::messages::parrot::{Event, RequestUsageRecorded};
use protobuf::Message;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(output, "{byte:02x}").unwrap();
    }
    output
}

fn file_state(path: &Path) -> Value {
    let raw = std::fs::read(path).unwrap();
    json!({"bytes":raw.len(),"hex":hex(&raw),"sha256":format!("{:x}",Sha256::digest(&raw)),
           "lines":raw.iter().filter(|&&byte|byte==b'\n').count()})
}

async fn sql(repository: &ConversationRepository) -> Value {
    repository
        .events()
        .transaction(|tx| {
            let mut names = tx
                .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")?
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            names.insert(0, "sqlite_master".into());
            let mut tables = serde_json::Map::new();
            for name in names {
                let mut statement = tx.prepare(&format!(
                    "SELECT * FROM \"{}\" ORDER BY rowid",
                    name.replace('"', "\"\"")
                ))?;
                let columns = statement
                    .column_names()
                    .iter()
                    .map(|name| name.to_string())
                    .collect::<Vec<_>>();
                let mut rows = statement.query([])?;
                let mut output = Vec::new();
                while let Some(row) = rows.next()? {
                    let mut fields = serde_json::Map::new();
                    for (index, name) in columns.iter().enumerate() {
                        use rusqlite::types::ValueRef;
                        let value = match row.get_ref(index)? {
                            ValueRef::Null => json!({"kind":"null"}),
                            ValueRef::Integer(value) => json!({"kind":"integer","value":value}),
                            ValueRef::Real(value) => json!({"kind":"real","bits":value.to_bits()}),
                            ValueRef::Text(value) => json!({"kind":"text","hex":hex(value)}),
                            ValueRef::Blob(value) => json!({"kind":"blob","hex":hex(value)}),
                        };
                        fields.insert(name.clone(), value);
                    }
                    output.push(Value::Object(fields));
                }
                tables.insert(name, json!(output));
            }
            Ok(Value::Object(tables))
        })
        .await
        .unwrap()
}

fn outcome<T: Serialize>(result: Result<T, EventError>) -> Value {
    match result {
        Ok(value) => json!({"returned":"ok","value":value}),
        Err(error) => json!({"returned":"error","type":std::any::type_name::<EventError>(),
            "is_protobuf":matches!(error,EventError::Database(DatabaseError::Protobuf(_))),
            "message":error.to_string(),"raw":format!("{error:?}")}),
    }
}

fn request(id: &str) -> Event {
    let mut event = Event {
        id: id.into(),
        agent_session_id: "agent".into(),
        ..Default::default()
    };
    event.set_request_usage_recorded(RequestUsageRecorded {
        request_id: id.into(),
        provider: "provider".into(),
        model: "model".into(),
        input_tokens: 101,
        cached_input_tokens: 23,
        output_tokens: 17,
        input_cost: 0.25,
        output_cost: 0.5,
        ..Default::default()
    });
    event
}

async fn payload(repository: &ConversationRepository, bytes: Vec<u8>) {
    repository
        .events()
        .transaction(move |tx| {
            let changed = tx.execute(
                "UPDATE event SET payload=?1 WHERE id='request-bad' AND request_id='request-bad'",
                [bytes],
            )?;
            assert_eq!(
                changed, 1,
                "selected non-null request row must actually be corrupted/repaired"
            );
            Ok(())
        })
        .await
        .unwrap();
}

async fn observe(root: PathBuf) -> Value {
    std::fs::create_dir(&root).unwrap();
    let selected =
        SelectedSessionRoot::new(root.join("sessions/listed"), RootOrigin::Current).unwrap();
    let repository = ConversationRepository::new(EventRepository::new(
        SessionDatabase::open(selected.clone()).unwrap(),
    ));
    repository
        .append_conversation(
            Event {
                id: "message".into(),
                agent_session_id: "agent".into(),
                ..Default::default()
            },
            ConversationMessage::text(MessageRole::User, "published before malformed request"),
        )
        .await
        .unwrap();
    let first = repository
        .events()
        .append_usage_fact(request("request-first"))
        .await
        .unwrap();
    let path = selected.path().join("root-agents/main/history.jsonl");
    let history = AgentHistoryFile::new(selected.clone(), &path, vec!["agent".into()]).unwrap();
    history.refresh(&repository, "agent").await.unwrap();
    let initial = file_state(&path);
    let valid = request("request-bad").write_to_bytes().unwrap();
    let bad = repository
        .events()
        .append_usage_fact(request("request-bad"))
        .await
        .unwrap();
    assert!(bad > first);
    let mut operations = Vec::new();
    macro_rules! record {
        ($name:literal, $operation:expr) => {{
            let before = sql(&repository).await;
            let result = outcome($operation);
            let after = sql(&repository).await;
            assert_eq!(before, after, "history read changed durable SQL: {}", $name);
            operations.push(json!({"name":$name,"outcome":result,"file":file_state(&path),
                "sql_unchanged":true,"sql_before":before,"sql_after":after}));
        }};
    }
    payload(&repository, vec![0x0a, 0x05, 0x61]).await;
    record!(
        "direct_full",
        repository
            .agent_history("agent".into())
            .await
            .map(|rows| rows.len())
    );
    record!(
        "direct_after_selected",
        repository
            .agent_history_after("agent".into(), 0, first)
            .await
            .map(|rows| rows.len())
    );
    record!(
        "refresh_incremental",
        history.refresh(&repository, "agent").await
    );
    record!(
        "wrong_session",
        history.refresh(&repository, "foreign").await
    );
    payload(&repository, valid.clone()).await;
    record!(
        "refresh_after_repair",
        history.refresh(&repository, "agent").await
    );
    payload(&repository, vec![0x0a, 0x05, 0x61]).await;
    record!(
        "direct_after_skips_old",
        repository
            .agent_history_after("agent".into(), 0, bad)
            .await
            .map(|rows| rows.len())
    );
    record!(
        "refresh_skips_old",
        history.refresh(&repository, "agent").await
    );
    record!("rebuild", history.rebuild(&repository, "agent").await);
    repository
        .events()
        .append_usage_fact(request("request-new"))
        .await
        .unwrap();
    record!(
        "refresh_after_failed_rebuild",
        history.refresh(&repository, "agent").await
    );
    payload(&repository, valid).await;
    record!(
        "refresh_final_repair",
        history.refresh(&repository, "agent").await
    );
    let final_sql = sql(&repository).await;
    repository.events().database().close().await.unwrap();
    let corrupting = ConversationRepository::new(EventRepository::new(
        SessionDatabase::open(selected.clone()).unwrap(),
    ));
    payload(&corrupting, vec![0x0a, 0x05, 0x61]).await;
    let corrupted_at_close = sql(&corrupting).await;
    corrupting.events().database().close().await.unwrap();
    let reopened = ConversationRepository::new(EventRepository::new(
        SessionDatabase::open(selected.clone()).unwrap(),
    ));
    let malformed_before = sql(&reopened).await;
    assert_eq!(corrupted_at_close, malformed_before);
    let projection = AgentHistoryFile::new(selected, &path, vec!["agent".into()]).unwrap();
    let malformed_direct = outcome(
        reopened
            .agent_history("agent".into())
            .await
            .map(|rows| rows.len()),
    );
    let malformed_refresh = outcome(projection.refresh(&reopened, "agent").await);
    let malformed_file = file_state(&path);
    let malformed_after = sql(&reopened).await;
    assert_eq!(malformed_before, malformed_after);
    payload(&reopened, request("request-bad").write_to_bytes().unwrap()).await;
    projection.refresh(&reopened, "agent").await.unwrap();
    let reopened_sql = sql(&reopened).await;
    assert_eq!(final_sql, reopened_sql, "reopen changed durable SQL");
    let result = json!({"initial":initial,"operations":operations,"reopen":{"file":file_state(&path),
        "sql_unchanged":true,"sql":reopened_sql,"direct_count":reopened.agent_history("agent".into()).await.unwrap().len(),
        "malformed":{"direct":malformed_direct,"refresh":malformed_refresh,"file":malformed_file,
            "sql_unchanged":true,"sql_before":malformed_before,"sql_after":malformed_after}}});
    reopened.events().database().close().await.unwrap();
    result
}


#[tokio::test]
async fn malformed_request_projection_suppresses_decode_failure_and_recovers_cursors() {
    let root = std::env::temp_dir().join(format!(
        "parrot-t19-malformed-regression-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let observed = observe(root.clone()).await;
    let operations = observed["operations"].as_array().unwrap();
    let row = |name: &str| operations.iter().find(|row| row["name"] == name).unwrap();
    for name in ["direct_full", "direct_after_selected"] {
        assert_eq!(row(name)["outcome"]["returned"], "error");
        assert_eq!(row(name)["outcome"]["is_protobuf"], true);
    }
    for name in [
        "refresh_incremental",
        "refresh_after_repair",
        "refresh_skips_old",
        "rebuild",
        "refresh_after_failed_rebuild",
        "refresh_final_repair",
    ] {
        assert_eq!(row(name)["outcome"]["returned"], "ok", "{name}");
    }
    assert_eq!(row("wrong_session")["outcome"]["returned"], "error");
    assert_eq!(row("wrong_session")["outcome"]["is_protobuf"], false);
    assert_eq!(row("refresh_incremental")["file"], observed["initial"]);
    assert_eq!(row("wrong_session")["file"], observed["initial"]);
    let repaired = &row("refresh_after_repair")["file"];
    assert_eq!(repaired["lines"], 3);
    assert_eq!(row("direct_after_skips_old")["outcome"]["value"], 1);
    for name in [
        "refresh_skips_old",
        "rebuild",
        "refresh_after_failed_rebuild",
    ] {
        assert_eq!(&row(name)["file"], repaired, "{name}");
    }
    let final_file = &row("refresh_final_repair")["file"];
    assert_eq!(final_file["lines"], 4);
    let file_hex = final_file["hex"].as_str().unwrap();
    for id in ["request-first", "request-bad", "request-new"] {
        assert_eq!(file_hex.matches(&hex(id.as_bytes())).count(), 1, "{id}");
    }
    let malformed_reopen = &observed["reopen"]["malformed"];
    assert_eq!(malformed_reopen["direct"]["returned"], "error");
    assert_eq!(malformed_reopen["direct"]["is_protobuf"], true);
    assert_eq!(malformed_reopen["refresh"]["returned"], "ok");
    assert_eq!(&malformed_reopen["file"], final_file);
    assert_eq!(
        malformed_reopen["sql_before"],
        malformed_reopen["sql_after"]
    );
    assert_eq!(&observed["reopen"]["file"], final_file);
    assert_eq!(observed["reopen"]["direct_count"], 4);
    assert_eq!(observed["reopen"]["sql_unchanged"], true);
    std::fs::remove_dir_all(root).unwrap();
}
