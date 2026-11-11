"""Pier agent wrapper for the Parrot coding agent (parrotrust).

Use with Pier's custom-agent import path:

    pier run -p <tasks> --agent-import-path parrot_agent:ParrotAgent \
        --model <provider>/<model> --env docker
"""

from parrot_agent.agent import ParrotAgent
from parrot_agent.trajectory import (
    collect_error_messages,
    convert_events_to_trajectory,
    parse_jsonl_events,
)

__all__ = [
    "ParrotAgent",
    "collect_error_messages",
    "convert_events_to_trajectory",
    "parse_jsonl_events",
]
