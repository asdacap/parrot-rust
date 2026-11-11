//! One JSON line per session event, for headless benchmarking consumers.
//!
//! Each line is an append-only, per-line-flushed object:
//! `{"id", "agent_session_id", "payload"}` where the payload is
//! `{"type": "<proto oneof field name>", ...scalar variant fields}` using the
//! proto field names. Proto enums serialize as their integer value. Unset
//! optional fields are omitted. Repeated message fields, nested message
//! fields and bytes fields are omitted. Only the one-shot chat path writes
//! the log.

use std::fs::{self, File};
use std::io::{self, Write};

use parrot_domain::protocol::messages::parrot as wire;
use parrot_domain::protocol::messages::parrot::event::Payload;
use serde_json::json;

pub struct JsonlEventLog {
    file: Option<File>,
}

impl JsonlEventLog {
    /// Create the log at `path`; an empty path disables logging.
    pub fn create(path: &str) -> io::Result<Self> {
        let file = if path.is_empty() {
            None
        } else {
            Some(
                fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)?,
            )
        };
        Ok(Self { file })
    }

    /// Append one event as a single flushed JSON line.
    pub fn write(&mut self, event: &wire::Event) -> io::Result<()> {
        let Some(file) = self.file.as_mut() else {
            return Ok(());
        };
        let mut line = serde_json::to_vec(&event_line(event))?;
        line.push(b'\n');
        file.write_all(&line)?;
        file.flush()
    }
}

fn event_line(event: &wire::Event) -> serde_json::Value {
    json!({
        "id": event.id,
        "agent_session_id": event.agent_session_id,
        "payload": event.payload.as_ref().map(payload_value),
    })
}

fn payload_value(payload: &Payload) -> serde_json::Value {
    match payload {
        Payload::TurnStarted(started) => typed("turn_started", json!({ "model": started.model })),
        Payload::TextChunk(chunk) => typed("text_chunk", json!({ "fragment": chunk.fragment })),
        Payload::ReasoningChunk(chunk) => typed(
            "reasoning_chunk",
            json!({
                "fragment": chunk.fragment,
                "kind": chunk.kind.value(),
                "part_id": chunk.part_id,
                "completed": chunk.completed,
            }),
        ),
        Payload::ToolCallChunk(chunk) => typed(
            "tool_call_chunk",
            json!({
                "tool_call_id": chunk.tool_call_id,
                "tool_name": chunk.tool_name,
                "arguments_fragment": chunk.arguments_fragment,
            }),
        ),
        Payload::RetryNotice(notice) => typed(
            "retry_notice",
            json!({
                "attempt": notice.attempt,
                "retry_after_ms": notice.retry_after_ms,
                "reason": notice.reason,
            }),
        ),
        Payload::TurnEnded(ended) => typed(
            "turn_ended",
            json!({
                "finish_reason": ended.finish_reason,
                "input_tokens": ended.input_tokens,
                "output_tokens": ended.output_tokens,
            }),
        ),
        Payload::TurnFailed(failed) => typed(
            "turn_failed",
            json!({
                "message": failed.message,
                "provider_response_body": failed.provider_response_body,
            }),
        ),
        Payload::InputAdmitted(admitted) => typed(
            "input_admitted",
            json!({
                "input_id": admitted.input_id,
                "message_id": admitted.message_id,
                "content": admitted.content,
                "delivery": admitted.delivery.value(),
                "source": admitted.source.value(),
            }),
        ),
        Payload::InputPromoted(promoted) => typed(
            "input_promoted",
            json!({
                "input_id": promoted.input_id,
                "message_id": promoted.message_id,
            }),
        ),
        Payload::ToolStarted(tool) => typed(
            "tool_started",
            json!({
                "tool_call_id": tool.tool_call_id,
                "tool_name": tool.tool_name,
            }),
        ),
        Payload::ToolFinished(tool) => typed(
            "tool_finished",
            optional_fields(
                "tool_finished",
                [
                    ("tool_call_id", Some(json!(tool.tool_call_id))),
                    ("tool_name", Some(json!(tool.tool_name))),
                    ("result", tool.result.as_ref().map(|result| json!(result))),
                ],
            ),
        ),
        Payload::ToolCancelled(tool) => typed(
            "tool_cancelled",
            json!({
                "tool_call_id": tool.tool_call_id,
                "tool_name": tool.tool_name,
            }),
        ),
        Payload::ToolError(tool) => typed(
            "tool_error",
            json!({
                "tool_call_id": tool.tool_call_id,
                "tool_name": tool.tool_name,
                "message": tool.message,
            }),
        ),
        Payload::AgentStarted(agent) => typed(
            "agent_started",
            json!({
                "parent_agent_session_id": agent.parent_agent_session_id,
                "name": agent.name,
            }),
        ),
        Payload::AgentFinished(agent) => typed(
            "agent_finished",
            optional_fields(
                "agent_finished",
                [
                    (
                        "parent_agent_session_id",
                        Some(json!(agent.parent_agent_session_id)),
                    ),
                    ("name", Some(json!(agent.name))),
                    ("elapsed_ms", agent.elapsed_ms.map(|elapsed| json!(elapsed))),
                ],
            ),
        ),
        Payload::AgentFailed(agent) => typed(
            "agent_failed",
            json!({
                "parent_agent_session_id": agent.parent_agent_session_id,
                "name": agent.name,
                "message": agent.message,
            }),
        ),
        Payload::AgentStatisticsUpdated(statistics) => typed(
            "agent_statistics_updated",
            json!({
                "input_tokens": statistics.input_tokens,
                "cached_input_tokens": statistics.cached_input_tokens,
                "output_tokens": statistics.output_tokens,
                "context_size": statistics.context_size,
                "context_limit": statistics.context_limit,
                "input_cost": statistics.input_cost,
                "output_cost": statistics.output_cost,
            }),
        ),
        Payload::PlanCompleted(plan) => typed(
            "plan_completed",
            json!({
                "agent_session_id": plan.agent_session_id,
                "message_id": plan.message_id,
                "markdown": plan.markdown,
            }),
        ),
        Payload::PermissionPending(permission) => typed(
            "permission_pending",
            json!({
                "id": permission.id,
                "agent_session_id": permission.agent_session_id,
                "reason": permission.reason,
            }),
        ),
        Payload::SessionUsageSnapshot(usage) => typed(
            "session_usage_snapshot",
            json!({
                "revision": usage.revision,
                "input_tokens": usage.input_tokens,
                "cached_input_tokens": usage.cached_input_tokens,
                "output_tokens": usage.output_tokens,
                "context_size": usage.context_size,
                "context_limit": usage.context_limit,
                "input_cost": usage.input_cost,
                "output_cost": usage.output_cost,
            }),
        ),
        Payload::CompactionStarted(_) => typed("compaction_started", json!({})),
        Payload::CompactionFinished(_) => typed("compaction_finished", json!({})),
        Payload::CompactionFailed(compaction) => typed(
            "compaction_failed",
            json!({ "message": compaction.message }),
        ),
        Payload::ProviderCallUsage(usage) => typed(
            "provider_call_usage",
            json!({
                "input_tokens": usage.input_tokens,
                "output_tokens": usage.output_tokens,
            }),
        ),
        Payload::SkillLoaded(skill) => typed("skill_loaded", json!({ "path": skill.path })),
        Payload::InputCanceled(canceled) => {
            typed("input_canceled", json!({ "input_id": canceled.input_id }))
        }
        Payload::ProviderRequestPhaseChanged(changed) => typed(
            "provider_request_phase_changed",
            json!({
                "phase": changed.phase.value(),
                "attempt": changed.attempt,
            }),
        ),
        Payload::RequestUsageRecorded(usage) => typed(
            "request_usage_recorded",
            optional_fields(
                "request_usage_recorded",
                [
                    ("request_id", Some(json!(usage.request_id))),
                    (
                        "parent_agent_session_id",
                        Some(json!(usage.parent_agent_session_id)),
                    ),
                    ("provider", Some(json!(usage.provider))),
                    ("model", Some(json!(usage.model))),
                    ("effort", usage.effort.as_ref().map(|effort| json!(effort))),
                    ("input_tokens", Some(json!(usage.input_tokens))),
                    (
                        "cached_input_tokens",
                        Some(json!(usage.cached_input_tokens)),
                    ),
                    ("output_tokens", Some(json!(usage.output_tokens))),
                    ("input_cost", Some(json!(usage.input_cost))),
                    ("output_cost", Some(json!(usage.output_cost))),
                    ("context_size", Some(json!(usage.context_size))),
                    ("context_limit", Some(json!(usage.context_limit))),
                    ("tool_call_ids", Some(json!(usage.tool_call_ids))),
                ],
            ),
        ),
        Payload::ToolExecutionStarted(tool) => typed(
            "tool_execution_started",
            optional_fields(
                "tool_execution_started",
                [
                    ("request_id", Some(json!(tool.request_id))),
                    ("assistant_sequence", Some(json!(tool.assistant_sequence))),
                    ("tool_call_id", Some(json!(tool.tool_call_id))),
                    ("tool_name", Some(json!(tool.tool_name))),
                    (
                        "parent_agent_session_id",
                        Some(json!(tool.parent_agent_session_id)),
                    ),
                    ("provider", Some(json!(tool.provider))),
                    ("model", Some(json!(tool.model))),
                    ("effort", tool.effort.as_ref().map(|effort| json!(effort))),
                ],
            ),
        ),
        Payload::ToolRequestReceived(received) => typed(
            "tool_request_received",
            json!({ "tool_call_count": received.tool_call_count }),
        ),
        Payload::ModeTurnCompleted(_) => typed("mode_turn_completed", json!({})),
        Payload::AgentNotice(notice) => typed(
            "agent_notice",
            json!({
                "kind": notice.kind,
                "notice": notice.notice,
            }),
        ),
        Payload::ToolSnapshot(snapshot) => typed(
            "tool_snapshot",
            json!({
                "kind": snapshot.kind,
                "owner_agent_session_id": snapshot.owner_agent_session_id,
                "inventory_instance_id": snapshot.inventory_instance_id,
                "revision": snapshot.revision,
                "chunk_index": snapshot.chunk_index,
                "final_chunk": snapshot.final_chunk,
                "removed": snapshot.removed,
            }),
        ),
        Payload::EncryptedReasoningReceived(_) => typed("encrypted_reasoning_received", json!({})),
        Payload::CompactionProgress(progress) => typed(
            "compaction_progress",
            json!({
                "phase": progress.phase.value(),
                "completed_groups": progress.completed_groups,
                "total_groups": progress.total_groups,
            }),
        ),
        // The oneof is non-exhaustive; a newer writer may emit a payload this
        // reader cannot name.
        _ => typed("unrecognized", json!({})),
    }
}

fn typed(kind: &str, fields: serde_json::Value) -> serde_json::Value {
    let mut object = serde_json::Map::new();
    object.insert("type".to_string(), json!(kind));
    if let serde_json::Value::Object(entries) = fields {
        object.extend(entries);
    }
    serde_json::Value::Object(object)
}

fn optional_fields<'a, I>(kind: &str, entries: I) -> serde_json::Value
where
    I: IntoIterator<Item = (&'a str, Option<serde_json::Value>)>,
{
    let mut object = serde_json::Map::new();
    object.insert("type".to_string(), json!(kind));
    for (name, value) in entries {
        if let Some(value) = value {
            object.insert(name.to_string(), value);
        }
    }
    serde_json::Value::Object(object)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage_event() -> wire::Event {
        wire::Event {
            id: "e1".into(),
            agent_session_id: "s1".into(),
            payload: Some(Payload::RequestUsageRecorded(wire::RequestUsageRecorded {
                request_id: "r1".into(),
                parent_agent_session_id: "s1".into(),
                provider: "mock".into(),
                model: "mock/tiny".into(),
                effort: Some("high".into()),
                input_tokens: 10,
                cached_input_tokens: 4,
                output_tokens: 20,
                input_cost: 0.5,
                output_cost: 1.5,
                context_size: 100,
                context_limit: 1000,
                tool_call_ids: vec!["t1".into()],
                ..Default::default()
            })),
            ..Default::default()
        }
    }

    #[test]
    fn serializes_scalars_and_enums() {
        let line = event_line(&usage_event());
        let payload = line["payload"].clone();
        assert_eq!(line["id"], "e1");
        assert_eq!(line["agent_session_id"], "s1");
        assert_eq!(payload["type"], "request_usage_recorded");
        assert_eq!(payload["provider"], "mock");
        assert_eq!(payload["effort"], "high");
        assert_eq!(payload["input_tokens"], 10);
        assert_eq!(payload["input_cost"], 0.5);
        assert_eq!(payload["tool_call_ids"], serde_json::json!(["t1"]));
    }

    #[test]
    fn omits_unset_optionals() {
        let finished = wire::Event {
            id: "e2".into(),
            agent_session_id: "s1".into(),
            payload: Some(Payload::ToolFinished(wire::ToolFinished {
                tool_call_id: "t1".into(),
                tool_name: "bash".into(),
                result: None,
                ..Default::default()
            })),
            ..Default::default()
        };
        let payload = event_line(&finished)["payload"].clone();
        assert_eq!(payload["type"], "tool_finished");
        assert_eq!(payload["tool_name"], "bash");
        assert!(payload.get("result").is_none());

        let started = wire::Event {
            id: "e5".into(),
            agent_session_id: "s1".into(),
            payload: Some(Payload::TurnStarted(wire::TurnStarted {
                model: "mock/tiny".into(),
                ..Default::default()
            })),
            ..Default::default()
        };
        let payload = event_line(&started)["payload"].clone();
        assert_eq!(payload["type"], "turn_started");
        assert_eq!(payload["model"], "mock/tiny");
        assert!(payload.get("model_alias_icon").is_none());
    }

    #[test]
    fn serializes_empty_payloads() {
        let compaction = wire::Event {
            id: "e3".into(),
            agent_session_id: "s1".into(),
            payload: Some(Payload::CompactionStarted(
                wire::CompactionStarted::default(),
            )),
            ..Default::default()
        };
        let payload = event_line(&compaction)["payload"].clone();
        assert_eq!(payload["type"], "compaction_started");
        assert_eq!(payload.as_object().expect("object").len(), 1);
    }

    #[test]
    fn empty_path_disables_the_log() {
        let mut disabled = JsonlEventLog::create("").expect("disabled log");
        disabled
            .write(&usage_event())
            .expect("disabled write stays silent");
    }

    #[test]
    fn writes_one_flushed_line_per_event() {
        let directory = std::env::temp_dir().join("parrot-jsonl-log-tests");
        std::fs::create_dir_all(&directory).expect("temp directory");
        let path = directory.join(format!("events-{}.jsonl", std::process::id()));
        let text = wire::Event {
            id: "e4".into(),
            agent_session_id: "s1".into(),
            payload: Some(Payload::TextChunk(wire::TextChunk {
                fragment: "hello".into(),
                ..Default::default()
            })),
            ..Default::default()
        };
        {
            let mut log = JsonlEventLog::create(path.to_str().expect("temp path is valid unicode"))
                .expect("log creation");
            log.write(&usage_event()).expect("first write");
            log.write(&text).expect("second write");
        }
        let content = std::fs::read_to_string(&path).expect("logged file");
        std::fs::remove_file(&path).expect("cleanup");
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: serde_json::Value = serde_json::from_str(lines[0]).expect("first line");
        let second: serde_json::Value = serde_json::from_str(lines[1]).expect("second line");
        assert_eq!(first["payload"]["type"], "request_usage_recorded");
        assert_eq!(second["payload"]["type"], "text_chunk");
        assert_eq!(second["payload"]["fragment"], "hello");
    }
}
