#!/usr/bin/env python3
"""Check that native conversation additions implement the generic core contract."""

from pathlib import Path
import re
import sys


ROOT = Path(__file__).resolve().parents[1]
FUNCTION = re.compile(r"\bfn\s+(\w+)\s*(?:<[^;{}]*>)?\s*\(")
ADDITION = re.compile(r"\bconversation\s*\.\s*add\s*\(")
SPECIALIZED = {
    "record_input_context",
    "record_steer",
    "record_context_reminder",
    "publish_context_reminder",
    "record_retry",
    "record_tool_calls",
    "record_completion_blocker_feedback",
    "record_tool_results",
    "complete_turn",
}


def main():
    failures = []
    additions = 0
    for path in sorted((ROOT / "crates/parrot-runtime/src").rglob("*.rs")):
        source = path.read_text()
        functions = list(FUNCTION.finditer(source))
        for addition in ADDITION.finditer(source):
            additions += 1
            owner = next(
                (function.group(1) for function in reversed(functions)
                 if function.start() < addition.start()),
                None,
            )
            if owner != "apply_conversation_addition":
                line = source.count("\n", 0, addition.start()) + 1
                failures.append(f"{path.relative_to(ROOT)}:{line} adds conversation in {owner}")
    protocol = (ROOT / "crates/parrot-core/src/protocol.rs").read_text()
    methods = {match.group(1) for match in FUNCTION.finditer(protocol)}
    for method in sorted(SPECIALIZED & methods):
        failures.append(f"States still exposes specialized mutation {method}")
    if "apply_conversation_addition" not in methods:
        failures.append("States has no generic apply_conversation_addition method")
    if not additions:
        failures.append("No native implementation of the generic addition was checked")
    if failures:
        print("\n".join(failures), file=sys.stderr)
        return 1
    print(f"PASS {additions} native additions are confined to generic apply_conversation_addition implementations")
    print("PASS States exposes no specialized conversation-add methods")
    return 0


if __name__ == "__main__":
    sys.exit(main())
