"""Convert the Parrot one-shot JSONL event log into a Pier ATIF trajectory.

The log is produced by ``parrot chat --jsonl <path>``: one JSON object per
line, in stream order, shaped as

    {"id": "<event id>", "agent_session_id": "<session>", "payload": {...}}

where ``payload`` carries a ``type`` key holding the snake_case oneof field
name from ``proto/parrot.proto`` plus that message's snake_case fields. Proto
enums serialize as integers (``reasoning_chunk.kind``: 0 raw, 1 summary).
Optional proto fields are omitted when unset. Unknown payload types and keys
are ignored, so the parser tolerates protocol evolution.
"""

import json
from typing import Any

from pier.models.trajectories import (
    Agent,
    FinalMetrics,
    Metrics,
    Observation,
    ObservationResult,
    Step,
    ToolCall,
    Trajectory,
)
from pier.utils.trajectory_metrics import extra_with_context_metrics

def parse_jsonl_events(text: str) -> list[dict[str, Any]]:
    """Parse a Parrot JSONL event log into a list of event objects."""
    events: list[dict[str, Any]] = []
    for line in text.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(event, dict):
            events.append(event)
    return events


def collect_error_messages(events: list[dict[str, Any]]) -> list[str]:
    """Return failure messages recorded by turn_failed and agent_failed events."""
    messages: list[str] = []
    for event in events:
        payload = event.get("payload") or {}
        kind = payload.get("type")
        if kind == "turn_failed" and payload.get("message"):
            messages.append(str(payload["message"]))
        elif kind == "agent_failed" and payload.get("message"):
            messages.append(str(payload["message"]))
    return messages


def _main_session_id(events: list[dict[str, Any]]) -> str | None:
    """Pick the main agent session: the first turn_started, else the first event."""
    for event in events:
        payload = event.get("payload") or {}
        if payload.get("type") == "turn_started":
            session = event.get("agent_session_id")
            if session:
                return str(session)
    for event in events:
        session = event.get("agent_session_id")
        if session:
            return str(session)
    return None


def _payload_type(event: dict[str, Any]) -> str | None:
    payload = event.get("payload")
    if not isinstance(payload, dict):
        return None
    kind = payload.get("type")
    return str(kind) if isinstance(kind, str) else None


class _Segment:
    """One provider request's worth of streamed events."""

    def __init__(self) -> None:
        self.text_parts: list[str] = []
        self.reasoning_parts: list[str] = []
        self.tool_calls: dict[str, dict[str, Any]] = {}
        self.tool_order: list[str] = []
        self.observations: dict[str, str] = {}
        self.model: str | None = None
        self.usage: dict[str, Any] | None = None

    def is_empty(self) -> bool:
        return not (
            self.text_parts
            or self.reasoning_parts
            or self.tool_order
            or self.observations
            or self.usage
        )


class _Converter:
    def __init__(self, events: list[dict[str, Any]]) -> None:
        self.events = events
        self.session_id = _main_session_id(events)
        # tool_call_id -> index into self.segments, filled from the
        # request_usage_recorded that owns each call.
        self.call_owner: dict[str, int] = {}

    def in_scope(self, event: dict[str, Any]) -> bool:
        session = event.get("agent_session_id")
        return self.session_id is None or not session or session == self.session_id

    def convert(self) -> Trajectory | None:
        steps: list[Step] = []
        user_pending: list[str] = []
        summarization_count = 0
        segment = _Segment()
        segments: list[_Segment] = []

        def close_segment() -> None:
            nonlocal segment
            if segment.usage is not None:
                for call_id in segment.usage.get("tool_call_ids") or []:
                    if isinstance(call_id, str) and call_id:
                        self.call_owner.setdefault(call_id, len(segments))
            if not segment.is_empty():
                segments.append(segment)
            segment = _Segment()

        for event in self.events:
            if not self.in_scope(event):
                continue
            kind = _payload_type(event)
            if kind is None:
                continue

            if kind == "input_admitted":
                content = event["payload"].get("content")
                if isinstance(content, str) and content:
                    user_pending.append(content)
            elif kind == "turn_started":
                model = event["payload"].get("model")
                if isinstance(model, str) and model:
                    segment.model = model
            elif kind == "text_chunk":
                fragment = event["payload"].get("fragment")
                if isinstance(fragment, str) and fragment:
                    segment.text_parts.append(fragment)
            elif kind == "reasoning_chunk":
                fragment = event["payload"].get("fragment")
                if isinstance(fragment, str) and fragment:
                    segment.reasoning_parts.append(fragment)
            elif kind == "tool_call_chunk":
                call_id = event["payload"].get("tool_call_id")
                name = event["payload"].get("tool_name")
                fragment = event["payload"].get("arguments_fragment") or ""
                if not isinstance(call_id, str) or not call_id:
                    continue
                call = segment.tool_calls.setdefault(
                    call_id, {"name": "", "arguments": ""}
                )
                if isinstance(name, str) and name:
                    call["name"] = name
                if isinstance(fragment, str):
                    call["arguments"] += fragment
                if call_id not in segment.tool_order:
                    segment.tool_order.append(call_id)
            elif kind in ("tool_started", "tool_finished", "tool_error", "tool_cancelled"):
                self._record_tool_event(kind, event["payload"], segment, segments)
            elif kind == "request_usage_recorded":
                segment.usage = event["payload"]
                if isinstance(event["payload"].get("model"), str):
                    segment.model = event["payload"]["model"]
                close_segment()
            elif kind == "compaction_started":
                summarization_count += 1
            # turn_ended / turn_failed / retry_notice / provider_call_usage /
            # session_usage_snapshot / agent_* / compaction_finished /
            # compaction_failed are observation-grade signals; the failure
            # message surfaces through collect_error_messages and the CLI's
            # nonzero exit, so they do not become steps here.

        close_segment()

        for message in user_pending:
            steps.append(Step(step_id=len(steps) + 1, source="user", message=message))

        total_prompt = 0
        total_completion = 0
        total_cached = 0
        total_cost = 0.0
        saw_usage = False
        peak_context: int | None = None

        for index, entry in enumerate(segments):
            step = self._build_step(index, entry, len(steps) + 1)
            if step is not None:
                steps.append(step)
            usage = entry.usage
            if usage is None:
                continue
            saw_usage = True
            total_prompt += int(usage.get("input_tokens") or 0)
            total_completion += int(usage.get("output_tokens") or 0)
            total_cached += int(usage.get("cached_input_tokens") or 0)
            total_cost += float(usage.get("input_cost") or 0) + float(
                usage.get("output_cost") or 0
            )
            context = int(usage.get("context_size") or 0)
            if context:
                peak_context = max(peak_context or 0, context)

        agent_steps = [step for step in steps if step.source == "agent"]
        if not agent_steps:
            return None

        if not saw_usage:
            peak_from_steps = _peak_prompt_tokens(agent_steps)
            peak_context = peak_from_steps
        elif peak_context is None:
            peak_context = _peak_prompt_tokens(agent_steps)

        final_metrics = FinalMetrics(
            total_prompt_tokens=total_prompt or None,
            total_completion_tokens=total_completion or None,
            total_cached_tokens=total_cached or None,
            total_cost_usd=total_cost or None,
            total_steps=len(agent_steps),
            extra=extra_with_context_metrics(
                None,
                peak_context_tokens=peak_context,
                summarization_count=summarization_count or None,
            ),
        )

        return Trajectory(
            schema_version="ATIF-v1.7",
            session_id=self.session_id or "unknown",
            agent=Agent(
                name="parrot",
                version="unknown",
                model_name=_model_hint(self.events),
            ),
            steps=steps,
            final_metrics=final_metrics,
        )

    def _record_tool_event(
        self, kind: str, payload: dict[str, Any], current: _Segment,
        segments: list[_Segment],
    ) -> None:
        call_id = payload.get("tool_call_id")
        if not isinstance(call_id, str) or not call_id:
            return
        owner = self.call_owner.get(call_id)
        target: _Segment
        if owner is not None and owner < len(segments):
            target = segments[owner]
        else:
            target = current
        if kind == "tool_finished":
            result = payload.get("result")
            if isinstance(result, str):
                target.observations[call_id] = result

    def _build_step(
        self, index: int, entry: _Segment, step_id: int
    ) -> Step | None:
        message = "".join(entry.text_parts)
        reasoning = "".join(entry.reasoning_parts)
        tool_calls = [
            ToolCall(
                tool_call_id=call_id,
                function_name=entry.tool_calls[call_id]["name"] or "unknown",
                arguments=_arguments_object(entry.tool_calls[call_id]["arguments"]),
            )
            for call_id in entry.tool_order
        ]
        observation = (
            Observation(
                results=[
                    ObservationResult(source_call_id=call_id, content=content)
                    for call_id, content in entry.observations.items()
                ]
            )
            if entry.observations
            else None
        )

        kwargs: dict[str, Any] = {
            "step_id": step_id,
            "source": "agent",
            "message": message,
        }
        if entry.model:
            kwargs["model_name"] = entry.model
        if reasoning:
            kwargs["reasoning_content"] = reasoning
        if tool_calls:
            kwargs["tool_calls"] = tool_calls
        if observation:
            kwargs["observation"] = observation
        if entry.usage is not None:
            kwargs["metrics"] = _metrics_from_usage(entry.usage)
            kwargs["llm_call_count"] = 1
        else:
            kwargs["llm_call_count"] = None

        if (
            not message
            and not reasoning
            and not tool_calls
            and observation is None
            and entry.usage is None
        ):
            return None
        return Step(**kwargs)


def _arguments_object(raw: str) -> dict[str, Any]:
    if not raw.strip():
        return {}
    try:
        parsed = json.loads(raw)
    except json.JSONDecodeError:
        return {"raw": raw}
    return parsed if isinstance(parsed, dict) else {"value": parsed}


def _metrics_from_usage(usage: dict[str, Any]) -> Metrics:
    input_cost = float(usage.get("input_cost") or 0)
    output_cost = float(usage.get("output_cost") or 0)
    extra: dict[str, Any] = {}
    context_size = int(usage.get("context_size") or 0)
    context_limit = int(usage.get("context_limit") or 0)
    if context_size:
        extra["context_tokens"] = context_size
    if context_limit:
        extra["context_limit"] = context_limit
    return Metrics(
        prompt_tokens=int(usage.get("input_tokens") or 0),
        completion_tokens=int(usage.get("output_tokens") or 0),
        cached_tokens=int(usage.get("cached_input_tokens") or 0) or None,
        cost_usd=(input_cost + output_cost) or None,
        extra=extra or None,
    )


def _peak_prompt_tokens(steps: list[Step]) -> int | None:
    peak: int | None = None
    for step in steps:
        if step.metrics is None or step.metrics.prompt_tokens is None:
            continue
        value = step.metrics.prompt_tokens
        peak = value if peak is None else max(peak, value)
    return peak


def _model_hint(events: list[dict[str, Any]]) -> str | None:
    for event in events:
        payload = event.get("payload") or {}
        if payload.get("type") == "request_usage_recorded":
            model = payload.get("model")
            if isinstance(model, str) and model:
                return model
    for event in events:
        payload = event.get("payload") or {}
        if payload.get("type") == "turn_started":
            model = payload.get("model")
            if isinstance(model, str) and model:
                return model
    return None


def convert_events_to_trajectory(
    events: list[dict[str, Any]],
    *,
    agent_name: str = "parrot",
    agent_version: str | None = None,
    model_name: str | None = None,
) -> Trajectory | None:
    """Convert parsed Parrot JSONL events into an ATIF trajectory.

    One agent step is emitted per provider request (bounded by
    ``request_usage_recorded``), plus user steps for admitted inputs. Tool
    calls and their observations are attributed to the request that owns each
    ``tool_call_id``; streamed text and reasoning fragments concatenate into
    the step message and reasoning content.
    """
    if not events:
        return None
    converter = _Converter(events)
    trajectory = converter.convert()
    if trajectory is None:
        return None
    trajectory.agent.name = agent_name
    if agent_version:
        trajectory.agent.version = agent_version
    if model_name:
        trajectory.agent.model_name = model_name
    return trajectory
