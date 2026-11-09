use super::Fixture;
use parrot_domain::protocol::messages::parrot::{
    AgentFailed, AgentFinished, AgentStarted, AgentStatisticsUpdatedEvent, Event,
    RequestUsageRecorded, ToolExecutionStarted, ToolFinished, ToolStarted,
};
use parrot_storage::events::{EventError, EventLoadProgress, record, statistics::*};
use protobuf::Message;
use rusqlite::params;
use std::sync::{Arc, Mutex};

const AT: &str = "2026-01-02T03:04:05.0000000+00:00";
fn clock() -> String {
    AT.to_owned()
}
fn envelope(id: &str, agent: &str) -> Event {
    Event {
        id: id.into(),
        agent_session_id: agent.into(),
        ..Default::default()
    }
}
fn start(id: &str, agent: &str, parent: &str) -> Event {
    let mut event = envelope(id, agent);
    event.set_agent_started(AgentStarted {
        parent_agent_session_id: parent.into(),
        name: format!("name-{agent}"),
        ..Default::default()
    });
    event
}
fn request(id: &str, agent: &str, parent: &str, key: &str, input: i64) -> Event {
    let mut event = envelope(id, agent);
    event.set_request_usage_recorded(RequestUsageRecorded {
        request_id: key.into(),
        parent_agent_session_id: parent.into(),
        provider: "provider".into(),
        model: "model".into(),
        input_tokens: input,
        cached_input_tokens: 2,
        output_tokens: 3,
        input_cost: 1.25,
        output_cost: 2.5,
        context_size: 17,
        context_limit: 99,
        tool_call_ids: vec!["call".into()],
        ..Default::default()
    });
    event
}
fn tool(id: &str, agent: &str, parent: &str, call: &str) -> Event {
    let mut event = envelope(id, agent);
    event.set_tool_execution_started(ToolExecutionStarted {
        tool_call_id: call.into(),
        parent_agent_session_id: parent.into(),
        provider: "provider".into(),
        model: "model".into(),
        effort: Some(String::new()),
        ..Default::default()
    });
    event
}
fn baseline(id: &str, agent: &str, input: i64) -> Event {
    let mut event = envelope(id, agent);
    event.set_agent_statistics_updated(AgentStatisticsUpdatedEvent {
        input_tokens: input,
        cached_input_tokens: 4,
        output_tokens: 5,
        context_size: 60,
        context_limit: 100,
        input_cost: 3.5,
        output_cost: 4.5,
        ..Default::default()
    });
    event
}

#[tokio::test]
async fn usage_fact_identity_and_per_agent_keys_are_exact_and_transactional() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    repository.transaction(|tx| {
        let fact = request("event", "agent", "", "request", 7);
        let revision = append_usage_fact(tx, &fact, &clock)?;
        assert_eq!(revision, 1);
        assert_eq!(append_usage_fact(tx, &fact, &clock)?, revision);
        let mut changed = fact.clone(); changed.mut_request_usage_recorded().input_tokens += 1;
        assert!(matches!(append_usage_fact(tx, &changed, &clock), Err(EventError::Invalid(message)) if message == "A usage fact identity was reused with different content."));
        changed.id = "second-id".into();
        assert!(matches!(append_usage_fact(tx, &changed, &clock), Err(EventError::Invalid(message)) if message == "A request or tool execution already has a usage fact with another event identity."));
        assert!(append_usage_fact(tx, &request("other-agent", "other", "", "request", 7), &clock).is_ok());
        assert!(append_usage_fact(tx, &tool("tool", "agent", "", "request"), &clock).is_ok());
        let duplicate_tool = tool("tool-other", "agent", "", "request");
        assert!(matches!(append_usage_fact(tx, &duplicate_tool, &clock), Err(EventError::Invalid(_))));
        assert!(matches!(append_usage_fact(tx, &envelope("not-fact", "agent"), &clock), Err(EventError::Argument(message)) if message == "An individual usage fact is required."));
        let counts: (i64, i64) = tx.query_row("SELECT (SELECT COUNT(*) FROM event), (SELECT COUNT(*) FROM event WHERE history_anchor IS NOT NULL);", [], |row| Ok((row.get(0)?, row.get(1)?)))?;
        assert_eq!(counts, (3, 2));
        Ok(())
    }).await.unwrap();
    repository.database().close().await.unwrap();
}

#[tokio::test]
async fn usage_fact_retry_conflict_and_invalid_fact_do_not_consume_clock() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    repository
        .transaction(|tx| {
            let calls = std::cell::Cell::new(0);
            let clock = || {
                calls.set(calls.get() + 1);
                AT.to_owned()
            };
            let fact = request("fact", "agent", "", "request", 1);
            assert_eq!(append_usage_fact(tx, &fact, &clock)?, 1);
            assert_eq!(calls.get(), 1);
            assert_eq!(append_usage_fact(tx, &fact, &clock)?, 1);
            assert_eq!(calls.get(), 1);
            let mut conflict = fact.clone();
            conflict.mut_request_usage_recorded().input_tokens = 2;
            assert!(append_usage_fact(tx, &conflict, &clock).is_err());
            conflict.id = "different-id".into();
            assert!(append_usage_fact(tx, &conflict, &clock).is_err());
            assert!(append_usage_fact(tx, &envelope("invalid", "agent"), &clock).is_err());
            assert_eq!(calls.get(), 1);
            Ok(())
        })
        .await
        .unwrap();
    repository.database().close().await.unwrap();
}

#[tokio::test]
async fn usage_fact_semantic_retry_preserves_original_wire_unknowns_and_optional_presence() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    repository
        .transaction(|tx| {
            let mut fact = request("raw", "agent", "", "", 7);
            fact.special_fields.mut_unknown_fields().add_varint(100, 11);
            let mut raw = fact.write_to_bytes()?;
            // Repeated singular known fields parse to the same value, but the
            // originally committed wire bytes must not be canonicalized on retry.
            raw.extend_from_slice(&[0x0a, 0x03, b'r', b'a', b'w']);
            let parsed = Event::parse_from_bytes(&raw)?;
            record(tx, &parsed, Some(&raw), AT)?;
            assert_eq!(append_usage_fact(tx, &fact, &clock)?, 1);
            let stored: Vec<u8> =
                tx.query_row("SELECT payload FROM event WHERE id = 'raw';", [], |row| {
                    row.get(0)
                })?;
            assert_eq!(stored, raw);
            assert_eq!(
                tx.query_row(
                    "SELECT history_anchor FROM event WHERE id = 'raw';",
                    [],
                    |row| row.get::<_, Option<i64>>(0)
                )?,
                None
            );
            let mut changed = fact.clone();
            changed.mut_request_usage_recorded().effort = Some(String::new());
            assert!(matches!(
                append_usage_fact(tx, &changed, &clock),
                Err(EventError::Invalid(_))
            ));
            changed = fact.clone();
            changed
                .special_fields
                .mut_unknown_fields()
                .add_varint(101, 1);
            assert!(matches!(
                append_usage_fact(tx, &changed, &clock),
                Err(EventError::Invalid(_))
            ));
            let mut other = fact;
            other.id = "duplicate-empty".into();
            assert!(matches!(
                append_usage_fact(tx, &other, &clock),
                Err(EventError::Invalid(_))
            ));
            Ok(())
        })
        .await
        .unwrap();
    repository.database().close().await.unwrap();
}

#[tokio::test]
async fn usage_fact_double_identity_uses_source_bitwise_cost_equality() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    repository
        .transaction(|tx| {
            let mut fact = request("nan", "agent", "", "nan", 1);
            fact.mut_request_usage_recorded().input_cost = f64::from_bits(0x7ff8_0000_0000_0001);
            assert_eq!(
                append_usage_fact(tx, &fact, &clock)?,
                append_usage_fact(tx, &fact, &clock)?
            );
            let mut changed = fact.clone();
            changed.mut_request_usage_recorded().input_cost = f64::from_bits(0x7ff8_0000_0000_0002);
            assert!(matches!(
                append_usage_fact(tx, &changed, &clock),
                Err(EventError::Invalid(_))
            ));
            let mut zero = request("zero", "agent", "", "zero", 1);
            zero.mut_request_usage_recorded().input_cost = 0.0;
            append_usage_fact(tx, &zero, &clock)?;
            zero.mut_request_usage_recorded().input_cost = -0.0;
            assert!(matches!(
                append_usage_fact(tx, &zero, &clock),
                Err(EventError::Invalid(_))
            ));
            Ok(())
        })
        .await
        .unwrap();
    repository.database().close().await.unwrap();
}

#[tokio::test]
async fn request_history_anchor_uses_agent_max_and_lookup_uses_latest_matching_request() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    repository.transaction(|tx| {
        tx.execute("INSERT INTO conversation_item(sequence,agent_session,origin,role,tool_call_id,created_at) VALUES (1,'agent','model','assistant','',?1),(2,'other','model','assistant','',?1);", [AT])?;
        tx.execute("INSERT INTO agent_history(sequence,agent_session,kind,conversation_sequence,summary,watermark,created_at) VALUES (10,'agent','message',1,'',0,?1),(20,'other','message',2,'',0,?1);", [AT])?;
        let first = append_usage_fact(tx, &request("old", "agent", "", "old", 1), &clock)?;
        let second = append_usage_fact(tx, &request("new", "agent", "", "new", 2), &clock)?;
        let mut irrelevant = request("irrelevant", "agent", "", "irrelevant", 3);
        irrelevant.mut_request_usage_recorded().tool_call_ids = vec!["different".into()];
        append_usage_fact(tx, &irrelevant, &clock)?;
        append_usage_fact(tx, &request("other", "other", "", "other", 4), &clock)?;
        let anchor: i64 = tx.query_row("SELECT history_anchor FROM event WHERE sequence = ?1;", [second], |row| row.get(0))?;
        assert_eq!(anchor, 10); assert!(second > first);
        assert_eq!(find_request_usage(tx, "agent", "call")?.unwrap().request_id, "new");
        assert!(find_request_usage(tx, "agent", "absent")?.is_none());
        assert!(find_request_usage(tx, "missing", "call")?.is_none());
        Ok(())
    }).await.unwrap();
    repository.database().close().await.unwrap();
}

#[tokio::test]
async fn replay_progress_observes_each_decoded_row_without_changing_statistics() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    repository
        .transaction(|tx| {
            for event in [
                start("start", "agent", ""),
                request("request", "agent", "", "repeat", 7),
                request("duplicate", "agent", "", "repeat", 100),
                envelope("nonusage", "observer"),
            ] {
                record(tx, &event, None, AT)?;
            }
            Ok(())
        })
        .await
        .unwrap();
    let expected = repository.replay_statistics().await.unwrap();
    let counts = Arc::new(Mutex::new(Vec::new()));
    let observed = counts.clone();
    let progress: EventLoadProgress = Arc::new(move |count| observed.lock().unwrap().push(count));
    let replay = repository
        .replay_statistics_with_progress(Some(progress))
        .await
        .unwrap();
    assert_eq!(*counts.lock().unwrap(), [1, 2, 3, 4]);
    assert_eq!(replay.revision, expected.revision);
    assert_eq!(replay.agents, expected.agents);
    assert_eq!(replay.parents, expected.parents);
    assert_eq!(
        replay.incomplete_legacy_tool_counts,
        expected.incomplete_legacy_tool_counts
    );
    assert_eq!(replay.agents["agent"].own.totals.input_tokens, 7);
    repository.database().close().await.unwrap();
}

#[tokio::test]
async fn empty_replay_progress_observes_zero_once() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    let counts = Arc::new(Mutex::new(Vec::new()));
    let observed = counts.clone();
    let replay = repository
        .replay_statistics_with_progress(Some(Arc::new(move |count| {
            observed.lock().unwrap().push(count);
        })))
        .await
        .unwrap();
    assert_eq!(*counts.lock().unwrap(), [0]);
    assert_eq!(replay.revision, 0);
    assert!(replay.agents.is_empty());
    assert!(replay.parents.is_empty());
    assert!(replay.incomplete_legacy_tool_counts.is_empty());
    repository.database().close().await.unwrap();
}

#[tokio::test]
async fn replay_progress_stops_before_malformed_row_without_reporting_empty_success() {
    for malformed in [0, 2] {
        let fixture = Fixture::new();
        let repository = fixture.repository();
        repository
            .transaction(move |tx| {
                for index in 0..4 {
                    record(tx, &envelope(&format!("event-{index}"), "agent"), None, AT)?;
                }
                tx.execute(
                    "UPDATE event SET payload = ?1 WHERE id = ?2;",
                    params![vec![0xff_u8], format!("event-{malformed}")],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let counts = Arc::new(Mutex::new(Vec::new()));
        let observed = counts.clone();
        assert!(
            repository
                .replay_statistics_with_progress(Some(Arc::new(move |count| {
                    observed.lock().unwrap().push(count);
                })))
                .await
                .is_err()
        );
        assert_eq!(*counts.lock().unwrap(), (1..=malformed).collect::<Vec<_>>());
        assert!(repository.replay_statistics().await.is_err());
        repository.database().close().await.unwrap();
    }
}

#[tokio::test]
async fn replay_progress_counts_are_not_a_replay_success_marker() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    repository
        .transaction(|tx| {
            record(tx, &start("first", "agent", "parent"), None, AT)?;
            record(tx, &start("conflict", "agent", "other-parent"), None, AT)?;
            Ok(())
        })
        .await
        .unwrap();
    let counts = Arc::new(Mutex::new(Vec::new()));
    let observed = counts.clone();
    assert!(matches!(
        repository
            .replay_statistics_with_progress(Some(Arc::new(move |count| {
                observed.lock().unwrap().push(count);
            })))
            .await,
        Err(EventError::Invalid(_))
    ));
    assert_eq!(*counts.lock().unwrap(), [1, 2]);
    repository.database().close().await.unwrap();
}

#[tokio::test]
async fn replay_ignores_aggregate_publications_and_preserves_fact_dedup_tools_and_revision() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    repository
        .transaction(|tx| {
            let events = [
                start("start", "agent", ""),
                request("pre", "agent", "", "repeat", 100),
                tool("tool-pre", "agent", "", "call"),
                baseline("baseline-old", "agent", 20),
                baseline("baseline-last", "agent", 10),
                request("post-repeat", "agent", "", "repeat", 200),
                request("post-new", "agent", "", "new", 3),
                tool("tool-duplicate", "agent", "", "call"),
                envelope("trailing", "observer"),
            ];
            for event in events {
                record(tx, &event, None, AT)?;
            }
            let replay = replay_statistics(tx)?;
            let agent = &replay.agents["agent"];
            assert_eq!(replay.revision, 9);
            assert_eq!(agent.own.totals.input_tokens, 103);
            assert_eq!(agent.own.totals.cached_input_tokens, 4);
            assert_eq!(agent.own.totals.output_tokens, 6);
            assert_eq!(agent.own.totals.tool_calls, 1);
            assert_eq!(agent.own.models.len(), 2);
            assert_eq!((agent.context_size, agent.context_limit), (17, 99));
            assert_eq!(agent.own, agent.cumulative);
            assert!(replay.agents["observer"].own.models.is_empty());
            assert_eq!(replay.parents["agent"], "");
            Ok(())
        })
        .await
        .unwrap();
    repository.database().close().await.unwrap();
}

#[tokio::test]
async fn replay_accumulates_each_descendant_own_models_once_in_source_agent_order() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    repository
        .transaction(|tx| {
            for event in [
                start("root", "root", ""),
                start("middle", "middle", "root"),
                start("leaf", "leaf", "middle"),
                request("root-use", "root", "", "root", 2),
                request("middle-use", "middle", "root", "middle", 3),
                request("leaf-use", "leaf", "middle", "leaf", 5),
            ] {
                record(tx, &event, None, AT)?;
            }
            let replay = replay_statistics(tx)?;
            assert_eq!(replay.agents["root"].own.totals.input_tokens, 2);
            assert_eq!(replay.agents["root"].cumulative.totals.input_tokens, 10);
            assert_eq!(replay.agents["middle"].own.totals.input_tokens, 3);
            assert_eq!(replay.agents["middle"].cumulative.totals.input_tokens, 8);
            assert_eq!(replay.agents["leaf"].cumulative.totals.input_tokens, 5);
            assert_eq!(replay.agents["root"].context_size, 17);
            assert_eq!(replay.parents["middle"], "root");
            assert_eq!(replay.parents["leaf"], "middle");
            Ok(())
        })
        .await
        .unwrap();
    repository.database().close().await.unwrap();
}

#[tokio::test]
async fn replay_cumulative_cost_addition_keeps_source_first_seen_agent_order() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    repository
        .transaction(|tx| {
            record(tx, &start("root", "root", ""), None, AT)?;
            for (index, cost) in [1e16, -1e16, 1.0].into_iter().enumerate() {
                let agent = format!("child-{index}");
                let mut event = request(&format!("r-{index}"), &agent, "root", &agent, 0);
                event.mut_request_usage_recorded().input_cost = cost;
                record(tx, &event, None, AT)?;
            }
            assert_eq!(
                replay_statistics(tx)?.agents["root"]
                    .cumulative
                    .totals
                    .input_cost,
                1.0
            );
            Ok(())
        })
        .await
        .unwrap();
    repository.database().close().await.unwrap();
}

#[tokio::test]
async fn replay_legacy_finished_tools_count_once_unfinished_mark_only_original_agents() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    repository
        .transaction(|tx| {
            for event in [start("root", "root", ""), start("child", "child", "root")] {
                record(tx, &event, None, AT)?;
            }
            let mut finished = envelope("finished-before-start", "child");
            finished.set_tool_finished(ToolFinished {
                tool_call_id: "legacy".into(),
                ..Default::default()
            });
            record(tx, &finished, None, AT)?;
            for (id, call) in [
                ("start-legacy", "legacy"),
                ("start-unfinished", "unfinished"),
                ("start-explicit", "explicit"),
            ] {
                let mut event = envelope(id, "child");
                event.set_tool_started(ToolStarted {
                    tool_call_id: call.into(),
                    ..Default::default()
                });
                record(tx, &event, None, AT)?;
            }
            finished.id = "duplicate-finish".into();
            record(tx, &finished, None, AT)?;
            let mut explicit_finish = finished.clone();
            explicit_finish.id = "explicit-finish".into();
            explicit_finish.mut_tool_finished().tool_call_id = "explicit".into();
            record(tx, &explicit_finish, None, AT)?;
            record(
                tx,
                &tool("explicit-fact", "child", "root", "explicit"),
                None,
                AT,
            )?;
            let replay = replay_statistics(tx)?;
            assert_eq!(replay.agents["child"].own.totals.tool_calls, 2);
            assert_eq!(replay.agents["root"].cumulative.totals.tool_calls, 2);
            assert_eq!(
                replay.incomplete_legacy_tool_counts,
                std::collections::HashSet::from(["child".to_owned()])
            );
            assert!(!replay.agents["root"].has_incomplete_legacy_tool_counts);
            assert!(!replay.agents["child"].has_incomplete_legacy_tool_counts);
            Ok(())
        })
        .await
        .unwrap();
    repository.database().close().await.unwrap();
}

#[tokio::test]
async fn replay_rejects_conflicting_missing_ancestor_and_cyclic_lineage_exactly() {
    for (events, expected) in [
        (
            vec![start("one", "agent", ""), start("two", "agent", "other")],
            "Historical agent lineage contains conflicting parents.",
        ),
        (
            vec![request("usage", "child", "absent", "request", 1)],
            "Historical ancestor absent has no parent lineage.",
        ),
        (
            vec![start("one", "a", "b"), start("two", "b", "a")],
            "Historical agent lineage contains a cycle.",
        ),
    ] {
        let fixture = Fixture::new();
        let repository = fixture.repository();
        repository.transaction(move |tx| {
            for event in events { record(tx, &event, None, AT)?; }
            assert!(matches!(replay_statistics(tx), Err(EventError::Invalid(message)) if message == expected));
            Ok(())
        }).await.unwrap();
        repository.database().close().await.unwrap();
    }
}

#[tokio::test]
async fn replay_uses_current_fact_and_finished_failed_lineage_and_checks_fact_overflow() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    repository
        .transaction(|tx| {
            record(tx, &request("root-fact", "root", "", "root", 2), None, AT)?;
            let mut finished = envelope("finished", "child");
            finished.set_agent_finished(AgentFinished {
                parent_agent_session_id: "root".into(),
                ..Default::default()
            });
            record(tx, &finished, None, AT)?;
            let mut failed = envelope("failed", "leaf");
            failed.set_agent_failed(AgentFailed {
                parent_agent_session_id: "child".into(),
                ..Default::default()
            });
            record(tx, &failed, None, AT)?;
            record(
                tx,
                &request("leaf-fact", "leaf", "child", "leaf", 3),
                None,
                AT,
            )?;
            let replay = replay_statistics(tx)?;
            assert_eq!(replay.parents["root"], "");
            assert_eq!(replay.parents["leaf"], "child");
            assert_eq!(replay.agents["root"].cumulative.totals.input_tokens, 5);
            assert_eq!(replay.agents["child"].cumulative.totals.input_tokens, 3);
            record(
                tx,
                &request("overflow", "root", "", "overflow", i64::MAX),
                None,
                AT,
            )?;
            assert!(matches!(replay_statistics(tx), Err(EventError::Overflow)));
            Ok(())
        })
        .await
        .unwrap();
    repository.database().close().await.unwrap();
}

#[tokio::test]
async fn usage_fact_anchor_failure_rolls_back_the_preceding_event_insert() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    repository.transaction(|tx| { tx.execute_batch("CREATE TRIGGER reject_usage_anchor BEFORE UPDATE OF history_anchor ON event BEGIN SELECT RAISE(ABORT,'anchor injection'); END;")?; Ok(()) }).await.unwrap();
    let result = repository
        .transaction(|tx| {
            append_usage_fact(tx, &request("fact", "agent", "", "request", 1), &clock)
        })
        .await;
    assert!(result.is_err());
    assert!(repository.replay().await.unwrap().is_empty());
    repository
        .transaction(|tx| {
            assert_eq!(
                tx.query_row("SELECT COUNT(*) FROM event WHERE history_anchor IS NOT NULL;", [], |row| {
                    row.get::<_, i64>(0)
                })?,
                0
            );
            assert_eq!(
                tx.query_row("SELECT COUNT(*) FROM sqlite_master WHERE name IN ('request_history_anchor', 'projection_version');", [], |row| row
                    .get::<_, i64>(0))?,
                0
            );
            Ok(())
        })
        .await
        .unwrap();
    repository.database().close().await.unwrap();
}

#[tokio::test]
async fn usage_fact_embedded_anchors_survive_history_deletion_and_retry_without_recapture() {
    use parrot_storage::conversation::{AgentHistoryEntry, ConversationRepository};

    let fixture = Fixture::new();
    let repository = fixture.repository();
    repository.transaction(|tx| {
        let fact = request("zero-anchor", "agent", "", "zero", 7);
        let bytes = fact.write_to_bytes()?;
        let revision = append_usage_fact(tx, &fact, &clock)?;
        tx.execute("INSERT INTO conversation_item(sequence,agent_session,origin,role,tool_call_id,created_at) VALUES (1,'agent','model','assistant','',?1),(2,'other','model','assistant','',?1);", [AT])?;
        tx.execute("INSERT INTO agent_history(sequence,agent_session,kind,conversation_sequence,summary,watermark,created_at) VALUES (10,'agent','message',1,'',0,?1),(20,'other','message',2,'',0,?1);", [AT])?;
        assert_eq!(append_usage_fact(tx, &fact, &clock)?, revision);
        append_usage_fact(tx, &request("ten-anchor", "agent", "", "ten", 8), &clock)?;
        append_usage_fact(tx, &request("other-anchor", "other", "", "other", 9), &clock)?;
        append_usage_fact(tx, &tool("tool-only", "tool-agent", "", "call"), &clock)?;
        let anchors = {
            let mut read = tx.prepare("SELECT id, history_anchor FROM event ORDER BY sequence;")?;
            read.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?)))?.collect::<Result<Vec<_>, _>>()?
        };
        assert_eq!(anchors, vec![("zero-anchor".into(), Some(0)), ("ten-anchor".into(), Some(10)), ("other-anchor".into(), Some(20)), ("tool-only".into(), None)]);
        tx.execute("DELETE FROM agent_history WHERE agent_session = 'agent';", [])?;
        tx.execute("DELETE FROM conversation_item WHERE agent_session = 'agent';", [])?;
        assert_eq!(append_usage_fact(tx, &fact, &clock)?, revision);
        let stored: Vec<u8> = tx.query_row("SELECT payload FROM event WHERE sequence = ?1;", [revision], |row| row.get(0))?;
        assert_eq!(stored, bytes);
        let retained: Option<i64> = tx.query_row("SELECT history_anchor FROM event WHERE id = 'ten-anchor';", [], |row| row.get(0))?;
        assert_eq!(retained, Some(10));
        Ok(())
    }).await.unwrap();
    repository.database().close().await.unwrap();
    let reopened = fixture.repository();
    let conversation = {
        let events = reopened.clone();
        let root = events.database().selected_root().clone();
        let id = parrot_domain::values::identifiers::UserSessionId::parse(
            root.path()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        )
        .unwrap();
        let metadata =
            parrot_storage::history_files::SessionIndex::new(root.clone(), id, root.path())
                .unwrap();
        ConversationRepository::new(events, metadata).unwrap()
    };
    assert_eq!(
        conversation.agent_history_session_ids().await.unwrap(),
        vec!["agent", "other"]
    );
    let history = conversation.agent_history("agent".into()).await.unwrap();
    assert!(
        matches!(&history[..], [AgentHistoryEntry::Request { sequence: 0, request_id: zero, .. }, AgentHistoryEntry::Request { sequence: 10, request_id: ten, .. }] if zero == "zero" && ten == "ten")
    );
    let other = conversation.agent_history("other".into()).await.unwrap();
    assert!(
        matches!(&other[..], [AgentHistoryEntry::Message { sequence: 20, .. }, AgentHistoryEntry::Request { sequence: 20, request_id, .. }] if request_id == "other")
    );
    reopened.database().close().await.unwrap();
}

#[tokio::test]
async fn aggregate_publications_are_raw_events_not_accounting_baselines() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    repository
        .append_usage_fact(request("before", "agent", "", "before", 2))
        .await
        .unwrap();
    let publication = baseline("publication", "agent", 900);
    let bytes = publication.write_to_bytes().unwrap();
    let appended = repository.append_raw(bytes.clone(), None).await.unwrap();
    assert_eq!(appended.stored.raw, bytes);
    repository
        .append_usage_fact(request("after", "agent", "", "after", 3))
        .await
        .unwrap();
    let before = repository.replay_statistics().await.unwrap();
    assert_eq!(before.agents["agent"].own.totals.input_tokens, 5);
    assert_eq!(before.agents["agent"].own.totals.input_cost, 2.5);
    assert_eq!(
        (
            before.agents["agent"].context_size,
            before.agents["agent"].context_limit
        ),
        (17, 99)
    );
    repository.database().close().await.unwrap();
    let reopened = fixture.repository();
    let after = reopened.replay_statistics().await.unwrap();
    assert_eq!(after.agents, before.agents);
    assert_eq!(after.revision, before.revision);
    assert_eq!(reopened.replay_raw().await.unwrap()[1].raw, bytes);
    reopened.database().close().await.unwrap();
}
