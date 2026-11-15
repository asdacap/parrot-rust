#!/usr/bin/env python3
"""Check that compaction implementation stays behind the provider boundary."""

from pathlib import Path
import re
import sys
import tomllib


ROOT = Path(__file__).resolve().parents[1]
INTERNALS = re.compile(
    r"\b(?:CompactionPlan|CompactionConfiguration|FittedCompaction|SummaryResponse|"
    r"CompactionPlanner|PlanningInput|CompactionResources)\b"
    r"|\bfn\s+(?:summarize|finalize_compaction|fold_groups|spill_group|fit_compaction|"
    r"report_compaction_\w+)\b"
)


def main():
    failures = []
    for crate in ("parrot-core", "parrot-runtime"):
        directory = ROOT / "crates" / crate / "src"
        for path in sorted(directory.rglob("*.rs")):
            if "tests" in path.relative_to(directory).parts or path.stem.endswith("_tests"):
                continue
            source = path.read_text().split("#[cfg(test)]", 1)[0]
            for match in INTERNALS.finditer(source):
                line = source.count("\n", 0, match.start()) + 1
                failures.append(f"{path.relative_to(ROOT)}:{line} exposes {match.group()}")
    provider = ROOT / "crates/parrot-provider/Cargo.toml"
    manifest = tomllib.loads(provider.read_text())
    tables = [manifest]
    tables.extend(manifest.get("target", {}).values())
    for table in tables:
        for section in ("dependencies", "dev-dependencies", "build-dependencies"):
            for name, dependency in table.get(section, {}).items():
                package = dependency.get("package", name) if isinstance(dependency, dict) else name
                if package in ("parrot-runtime", "parrot-storage"):
                    failures.append(f"{provider.relative_to(ROOT)} depends on {package}")
    if failures:
        print("\n".join(failures), file=sys.stderr)
        return 1
    print("PASS core and runtime expose no compaction algorithm stages")
    print("PASS provider has no runtime or storage dependency")
    return 0


if __name__ == "__main__":
    sys.exit(main())
