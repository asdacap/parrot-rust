import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import textwrap
import unittest


REPOSITORY = Path(__file__).resolve().parents[2]
COMMON_GIT = Path(subprocess.check_output(
    ["git", "rev-parse", "--path-format=absolute", "--git-common-dir"],
    cwd=REPOSITORY, text=True,
).strip())
TEMP_ROOT = COMMON_GIT.parent / ".tmp"
TEMP_ROOT.mkdir(exist_ok=True)

TOOL = """#!/usr/bin/env python3
import os
from pathlib import Path
import sys
import time

root = Path(os.environ["CHECK_EVIDENCE"])
arguments = sys.argv[1:]
if Path(sys.argv[0]).name == "uname":
    print(os.environ["CHECK_PLATFORM"])
    sys.exit(0)
if Path(sys.argv[0]).name == "bash":
    assert (root / "package").exists(), "smoke requires a built package"
    (root / "smoke").touch()
    sys.exit(0)
if "--command" not in arguments:
    assert all((root / f"{name}.done").exists() for name in
               ("python", "workspace", "contracts", "frontend")), "package started before tests finished"
    (root / "package").touch()
    print(root)
    sys.exit(0)
command = arguments[arguments.index("--command") + 1:]
if command == ["bash", "-c", "command -v bash"]:
    print(root / "tools" / "bash")
elif "ci" in command:
    (root / "installed").touch()
elif command[0] == "cargo" and "--example" in command:
    assert command[command.index("--example") + 1] == "t17-crash-probe"
    probe = Path(os.environ["CARGO_TARGET_DIR"]) / "debug/examples/t17-crash-probe"
    probe.parent.mkdir(parents=True, exist_ok=True)
    probe.touch()
elif "build" in command:
    assert (root / "installed").exists(), "frontend build requires installed dependencies"
    (root / "assets").touch()
elif command[0] == "cargo" and command[1] == "fmt":
    pass
elif command[0] == "cargo" and command[1] == "clippy":
    assert (root / "assets").exists(), "Rust compilation requires frontend assets"
elif "--doc" in command:
    name = "contracts" if "--manifest-path" in command else "workspace"
    assert (root / f"{name}.done").exists(), "doctests must follow their executable tests"
    (root / f"{name}.docs").touch()
elif "sandbox_linux" in command or "native::" in command or command[0] == "bash":
    sandbox = "sandbox_linux" in command or "sandbox/" in command[1]
    (root / ("sandbox" if sandbox else "terminal")).touch()
else:
    if command[0] == "python3":
        name = "python"
    elif command[0] == "npm":
        name = "frontend"
    else:
        name = "contracts" if "--manifest-path" in command else "workspace"
        assert (root / "assets").exists(), "Rust tests require frontend assets"
        if name == "workspace":
            assert os.environ.get("T57_TEST_SHELL") == str(root / "tools/bash"), "process tests require Nix Bash"
            probe = Path(os.environ["CARGO_TARGET_DIR"]) / "debug/examples/t17-crash-probe"
            assert os.environ.get("T17_CRASH_PROBE_BINARY") == str(probe) and probe.exists(), "crash tests require a built probe"
            assert os.environ.get("PARROT_T64_BROWSER_MODULES") == str(Path.cwd() / "web/node_modules"), "browser tests require installed modules"
        (root / f"{name}.target").write_text(os.environ["CARGO_TARGET_DIR"])
    (root / f"{name}.started").touch()
    deadline = time.monotonic() + 5
    while not all((root / f"{peer}.started").exists() for peer in
                  ("python", "workspace", "contracts", "frontend")):
        if time.monotonic() >= deadline:
            sys.exit("independent test suites did not start concurrently")
        time.sleep(0.01)
    (root / f"{name}.done").touch()
    sys.exit(23 if name == os.environ.get("CHECK_FAIL_SUITE") else 0)
"""


class ParallelChecksTests(unittest.TestCase):
    def run_workflow(self, failing_suite="", platform="Linux"):
        directory = tempfile.TemporaryDirectory(
            prefix="parallel-ci-", dir=TEMP_ROOT,
        )
        self.addCleanup(directory.cleanup)
        root = Path(directory.name)
        tools = root / "tools"
        tools.mkdir()
        for name in ("nix", "bash", "uname"):
            tool = tools / name
            tool.write_text(TOOL)
            tool.chmod(0o755)
        workflow = (REPOSITORY / ".github/workflows/check.yml").read_text()
        shell = textwrap.dedent(workflow.split("        run: |\n", 1)[1])
        shell = re.sub(
            r"\$\{\{ matrix\.(profile|package|target) \}\}", platform.lower(), shell,
        )
        environment = dict(os.environ)
        environment.update(
            CHECK_EVIDENCE=str(root),
            CHECK_FAIL_SUITE=failing_suite,
            CHECK_PLATFORM=platform,
            PARROT_CI_CACHE_ROOT=str(root / "cache"),
            PARROT_CI_CACHE_KEY="fixture",
            RUNNER_TEMP=str(root),
            GITHUB_RUN_ID="fixture",
            GITHUB_RUN_ATTEMPT="1",
            PATH=str(tools) + os.pathsep + os.environ["PATH"],
        )
        output = subprocess.run(
            [shutil.which("bash"), "-c", shell], cwd=REPOSITORY,
            env=environment, capture_output=True, text=True, timeout=30,
        )
        return root, output

    def test_independent_suites_overlap_and_preserve_required_checks(self):
        for platform in ("Linux", "Darwin"):
            with self.subTest(platform=platform):
                root, output = self.run_workflow(platform=platform)
                self.assertEqual(output.returncode, 0, output.stdout + output.stderr)
                for evidence in (
                    "workspace.docs", "contracts.docs", "sandbox", "terminal", "smoke",
                ):
                    self.assertTrue((root / evidence).exists(), evidence)
                workspace = Path((root / "workspace.target").read_text())
                contracts = Path((root / "contracts.target").read_text())
                self.assertNotEqual(workspace, contracts)
                self.assertIn(platform.lower(), workspace.parts)
                self.assertIn(platform.lower(), contracts.parts)

    def test_failed_suite_fails_ci_after_every_started_suite_finishes(self):
        root, output = self.run_workflow("workspace")
        self.assertNotEqual(output.returncode, 0, output.stdout + output.stderr)
        for name in ("python", "workspace", "contracts", "frontend"):
            self.assertTrue((root / f"{name}.done").exists(), name)
        self.assertFalse((root / "package").exists())
        self.assertFalse((root / "smoke").exists())


if __name__ == "__main__":
    unittest.main()
