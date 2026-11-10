#!/usr/bin/env python3
"""Check that native core implementations do not retain executor proxies."""

from pathlib import Path
import re
import sys


ROOT = Path(__file__).resolve().parents[1]
ENGINE = ROOT / "crates/parrot-runtime/src/agent_turn/engine"
FORBIDDEN = re.compile(
    r"\b(?:RuntimeBackend|TurnTools|ToolSettlement|SummaryProviderAdapter)\b"
    r"|(?<!::)\bProviderCall\b|\b(?:self|states)\s*\.\s*backend\b"
)


def main():
    failures = []
    modules = ENGINE / "core_loop"
    paths = [modules / name for name in (
        "states.rs", "conversation.rs", "tools.rs", "provider.rs", "events.rs"
    )]
    paths.extend(
        path for directory in ("tools", "provider")
        for path in sorted((modules / directory).rglob("*.rs"))
        if "tests" not in path.relative_to(modules).parts and path.stem != "tests"
    )
    for path in paths:
        source = path.read_text().split("#[cfg(test)]", 1)[0]
        for match in FORBIDDEN.finditer(source):
            line = source.count("\n", 0, match.start()) + 1
            failures.append(f"{path.relative_to(ROOT)}:{line} retains {match.group()}")
    composition = (ENGINE / "core_loop.rs").read_text()
    for declaration in re.finditer(
        r"struct\s+Runtime(?:States|Tools|Provider|EventPublisher)\b[^{}]*\{[^{}]*\}",
        composition,
    ):
        if FORBIDDEN.search(declaration.group()):
            failures.append("Core implementation declaration retains an executor proxy")
    if failures:
        print("\n".join(failures), file=sys.stderr)
        return 1
    print("PASS native core implementations retain no backend or inner executor proxies")
    return 0


if __name__ == "__main__":
    sys.exit(main())
