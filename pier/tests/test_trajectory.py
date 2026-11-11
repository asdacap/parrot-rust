import json
from pathlib import Path

from parrot_agent import (
    ParrotAgent,
    collect_error_messages,
    convert_events_to_trajectory,
    parse_jsonl_events,
)


def _event(session: str, payload: dict) -> dict:
    return {"id": f"evt-{json.dumps(payload, sort_keys=True)[:12]}", "agent_session_id": session, "payload": payload}


def _usage_event(session: str, **overrides) -> dict:
    usage = {
        "type": "request_usage_recorded",
        "request_id": "req-1",
        "provider": "openai",
        "model": "openai/gpt-5.5",
        "input_tokens": 100,
        "cached_input_tokens": 20,
        "output_tokens": 40,
        "input_cost": 0.01,
        "output_cost": 0.02,
        "context_size": 120,
        "context_limit": 200000,
        "tool_call_ids": [],
    }
    usage.update(overrides)
    return _event(session, usage)


def _tool_call_chunk(session: str, call_id: str, name: str, arguments: str) -> dict:
    return _event(
        session,
        {
            "type": "tool_call_chunk",
            "tool_call_id": call_id,
            "tool_name": name,
            "arguments_fragment": arguments,
        },
    )


def _write_log(path: Path, events: list[dict]) -> Path:
    path.write_text("\n".join(json.dumps(event) for event in events) + "\n")
    return path


def test_tool_call_round_becomes_two_steps_with_metrics(tmp_path: Path):
    session = "sess-1"
    events = [
        _event(session, {"type": "turn_started", "model": "openai/gpt-5.5"}),
        _event(
            session,
            {"type": "input_admitted", "input_id": "in-1", "message_id": "m-1", "content": "Fix the failing test"},
        ),
        _event(session, {"type": "text_chunk", "fragment": "Let me "}),
        _event(session, {"type": "text_chunk", "fragment": "look."}),
        _event(session, {"type": "reasoning_chunk", "fragment": "thinking", "kind": 1, "part_id": "r1", "completed": True}),
        _tool_call_chunk(session, "call-1", "shell", '{"command": "ls"}'),
        _usage_event(session, tool_call_ids=["call-1"]),
        _event(session, {"type": "tool_started", "tool_call_id": "call-1", "tool_name": "shell"}),
        _event(
            session,
            {"type": "tool_finished", "tool_call_id": "call-1", "tool_name": "shell", "result": "src/\ntests/"},
        ),
        _event(session, {"type": "text_chunk", "fragment": "Done."}),
        _usage_event(
            session,
            request_id="req-2",
            input_tokens=200,
            cached_input_tokens=0,
            output_tokens=10,
            input_cost=0.03,
            output_cost=0.04,
            context_size=210,
            tool_call_ids=[],
        ),
    ]
    log = _write_log(tmp_path / "parrot.jsonl", events)

    trajectory = convert_events_to_trajectory(parse_jsonl_events(log.read_text()))
    assert trajectory is not None

    user_steps = [step for step in trajectory.steps if step.source == "user"]
    agent_steps = [step for step in trajectory.steps if step.source == "agent"]
    assert len(user_steps) == 1
    assert user_steps[0].message == "Fix the failing test"
    assert len(agent_steps) == 2

    first, second = agent_steps
    assert first.message == "Let me look."
    assert first.reasoning_content == "thinking"
    assert first.tool_calls is not None and len(first.tool_calls) == 1
    assert first.tool_calls[0].function_name == "shell"
    assert first.tool_calls[0].arguments == {"command": "ls"}
    assert first.metrics is not None
    assert first.metrics.prompt_tokens == 100
    assert first.metrics.cached_tokens == 20
    assert first.metrics.completion_tokens == 40
    assert first.metrics.cost_usd == 0.03
    assert first.llm_call_count == 1
    assert first.observation is not None
    assert first.observation.results[0].source_call_id == "call-1"
    assert first.observation.results[0].content == "src/\ntests/"

    assert second.message == "Done."
    assert second.observation is None
    assert second.metrics is not None
    assert second.metrics.prompt_tokens == 200

    metrics = trajectory.final_metrics
    assert metrics is not None
    assert metrics.total_prompt_tokens == 300
    assert metrics.total_completion_tokens == 50
    assert metrics.total_cached_tokens == 20
    assert metrics.total_cost_usd == 0.1
    assert metrics.total_steps == 2
    assert metrics.extra["peak_context_tokens"] == 210
    assert "summarization_count" not in metrics.extra
    assert trajectory.session_id == session
    assert trajectory.agent.name == "parrot"
    assert trajectory.agent.model_name == "openai/gpt-5.5"


def test_tool_events_after_boundary_reassign_to_owning_step(tmp_path: Path):
    session = "sess-1"
    events = [
        _tool_call_chunk(session, "call-9", "edit_file", '{"path": "a.rs"}'),
        _usage_event(session, tool_call_ids=["call-9"]),
        _event(session, {"type": "tool_started", "tool_call_id": "call-9", "tool_name": "edit_file"}),
        _event(
            session,
            {"type": "tool_finished", "tool_call_id": "call-9", "tool_name": "edit_file", "result": "patched"},
        ),
        _usage_event(session, request_id="req-2", tool_call_ids=[]),
    ]

    trajectory = convert_events_to_trajectory(parse_jsonl_events("\n".join(json.dumps(e) for e in events)))
    assert trajectory is not None
    agent_steps = [step for step in trajectory.steps if step.source == "agent"]
    assert len(agent_steps) == 2
    first, second = agent_steps
    assert first.tool_calls is not None and first.tool_calls[0].tool_call_id == "call-9"
    assert first.observation is not None
    assert first.observation.results[0].content == "patched"
    assert first.metrics is not None and first.metrics.prompt_tokens == 100
    assert second.message == ""
    assert second.metrics is not None and second.metrics.prompt_tokens == 100
    assert second.tool_calls is None


def test_compaction_events_count_summarization(tmp_path: Path):
    session = "sess-1"
    events = [
        _event(session, {"type": "turn_started", "model": "m"}),
        _event(session, {"type": "compaction_started"}),
        _event(session, {"type": "compaction_finished"}),
        _usage_event(session),
    ]
    trajectory = convert_events_to_trajectory(parse_jsonl_events("\n".join(json.dumps(e) for e in events)))
    assert trajectory is not None
    assert trajectory.final_metrics.extra["summarization_count"] == 1


def test_failure_messages_collected(tmp_path: Path):
    session = "sess-1"
    events = [
        _event(session, {"type": "turn_failed", "message": "boom", "provider_response_body": ""}),
        _event(session, {"type": "agent_failed", "parent_agent_session_id": session, "name": "child", "message": "child boom"}),
    ]
    parsed = parse_jsonl_events("\n".join(json.dumps(e) for e in events))
    assert collect_error_messages(parsed) == ["boom", "child boom"]


def test_parser_tolerates_unknown_and_malformed_lines(tmp_path: Path):
    session = "sess-1"
    good = json.dumps(_event(session, {"type": "turn_started", "model": "m"}))
    log = tmp_path / "parrot.jsonl"
    log.write_text(f"{good}\nnot json\n\n" + json.dumps({"future": {"type": "brand_new_kind", "x": 1}}) + "\n")
    events = parse_jsonl_events(log.read_text())
    assert len(events) == 2
    trajectory = convert_events_to_trajectory(events)
    assert trajectory is None  # no agent content


def test_empty_log_yields_no_trajectory(tmp_path: Path):
    log = tmp_path / "parrot.jsonl"
    log.write_text("")
    assert parse_jsonl_events(log.read_text()) == []
    assert convert_events_to_trajectory([]) is None


def test_malformed_arguments_fall_back_to_raw(tmp_path: Path):
    session = "sess-1"
    events = [
        _tool_call_chunk(session, "call-1", "shell", '{"command": '),
        _usage_event(session, tool_call_ids=["call-1"]),
    ]
    trajectory = convert_events_to_trajectory(parse_jsonl_events("\n".join(json.dumps(e) for e in events)))
    assert trajectory is not None
    step = [s for s in trajectory.steps if s.source == "agent"][0]
    assert step.tool_calls[0].arguments == {"raw": '{"command": '}


def test_populate_context_writes_trajectory_and_metrics(tmp_path: Path):
    session = "sess-1"
    events = [
        _event(session, {"type": "text_chunk", "fragment": "hi"}),
        _usage_event(session),
    ]
    log = _write_log(tmp_path / "parrot.jsonl", events)

    agent = ParrotAgent(logs_dir=tmp_path, model_name="openai/gpt-5.5", version="9.9.9")
    from pier.models.agent.context import AgentContext

    context = AgentContext()
    agent.populate_context_post_run(context)

    trajectory_path = tmp_path / "trajectory.json"
    assert trajectory_path.exists()
    written = json.loads(trajectory_path.read_text())
    assert written["agent"]["name"] == "parrot"
    assert written["agent"]["version"] == "9.9.9"
    assert context.n_input_tokens == 100
    assert context.n_output_tokens == 40
    assert context.n_cache_tokens == 20
    assert context.peak_context_tokens == 120


def test_populate_context_without_log_is_noop(tmp_path: Path):
    agent = ParrotAgent(logs_dir=tmp_path, model_name="openai/gpt-5.5")
    from pier.models.agent.context import AgentContext

    context = AgentContext()
    agent.populate_context_post_run(context)
    assert not (tmp_path / "trajectory.json").exists()
    assert context.is_empty()
