import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock


REPOSITORY = Path(__file__).resolve().parents[3]
HELPER = Path(os.environ.get(
    "PARROT_QUEUE_HELPER", REPOSITORY / "assets/skills/queue/scripts/queue.py",
))
COMMON_GIT = Path(subprocess.check_output(
    ["git", "rev-parse", "--path-format=absolute", "--git-common-dir"],
    cwd=REPOSITORY, text=True,
).strip())
TEMP_ROOT = COMMON_GIT.parent / ".tmp"
TEMP_ROOT.mkdir(exist_ok=True)


class QueueTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="queue-skill-", dir=TEMP_ROOT)
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.path = self.root / "owner/scratch/queues/work.json"
        self.call("create", self.path, "--description", "Shared work")

    def start(self, *arguments):
        process = subprocess.Popen(
            [sys.executable, "-B", str(HELPER), *map(str, arguments)],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )
        self.addCleanup(self.stop, process)
        return process

    @staticmethod
    def stop(process):
        if process.poll() is None:
            process.kill()
        process.communicate()

    def finish(self, process, success=True):
        output, error = process.communicate(timeout=10)
        if success:
            self.assertEqual(process.returncode, 0, error)
            return json.loads(output)
        self.assertNotEqual(process.returncode, 0)
        self.assertEqual(output, "")
        return error

    def call(self, *arguments, success=True):
        return self.finish(self.start(*arguments), success=success)

    def push(self, items, *options):
        return self.call("push", self.path, "--items", json.dumps(items), *options)

    def read_bytes(self):
        return self.path.read_bytes()

    def test_reopen_fifo_counts_and_drained_close(self):
        self.push([str(item) for item in range(10)])
        result = self.call("take", self.path, "--count", 7)
        self.assertEqual(result["items"], [str(item) for item in range(7)])
        self.assertEqual(
            (result["taken"], result["enqueued_total"], result["remaining"]), (7, 10, 3),
        )
        self.assertFalse(result["drained"])
        self.assertEqual(self.call("info", self.path)["remaining"], 3)
        self.call("close", self.path)
        self.assertEqual(self.call("take", self.path, "--count", 10)["items"], ["7", "8", "9"])
        started = time.monotonic()
        result = self.call("take", self.path, "--wait-ms", 30000)
        self.assertLess(time.monotonic() - started, 2)
        self.assertEqual(result["items"], [])
        self.assertTrue(result["drained"])
        self.assertEqual(self.call("close", self.path), self.call("info", self.path))

    def test_source_file_filters_blank_lines_and_preserves_item_text(self):
        source = self.root / "input.txt"
        source.write_text("alpha\n\n  \n βeta \r\n", encoding="utf-8")
        self.call("push", self.path, "--source-file", source, "--close")
        result = self.call("take", self.path, "--count", 3)
        self.assertEqual(result["items"], ["alpha", " βeta "])
        self.assertTrue(result["drained"])

    def test_open_empty_timeout_and_later_input(self):
        started = time.monotonic()
        result = self.call("take", self.path, "--wait-ms", 150)
        self.assertGreaterEqual(time.monotonic() - started, 0.14)
        self.assertEqual(result["items"], [])
        self.assertFalse(result["closed"])
        waiting = self.start("take", self.path, "--wait-ms", 2000)
        time.sleep(0.1)
        self.assertIsNone(waiting.poll())
        self.push(["arrived"])
        self.assertEqual(self.finish(waiting)["items"], ["arrived"])

    def test_wait_releases_lock_for_producer_close(self):
        waiting = self.start("take", self.path, "--wait-ms", 2000)
        time.sleep(0.1)
        self.assertIsNone(waiting.poll())
        self.call("close", self.path)
        self.assertTrue(self.finish(waiting)["drained"])

    def test_concurrent_producers_consumers_and_readers(self):
        expected = [f"{producer}:{item}" for producer in range(4) for item in range(25)]
        producers = [
            self.start("push", self.path, "--items", json.dumps(expected[index:index + 25]))
            for index in range(0, 100, 25)
        ]
        consumers = [self.start("take", self.path, "--count", 5, "--wait-ms", 2000) for _ in range(20)]
        readers = [self.start("info", self.path) for _ in range(5)]
        for producer in producers:
            self.finish(producer)
        for reader in readers:
            state = self.finish(reader)
            self.assertEqual(state["taken"] + state["remaining"], state["enqueued_total"])
        self.call("close", self.path)
        taken = [item for consumer in consumers for item in self.finish(consumer)["items"]]
        taken.extend(self.call("take", self.path, "--count", 100)["items"])
        self.assertCountEqual(taken, expected)
        self.assertEqual(len(taken), len(set(taken)))
        state = self.call("info", self.path)
        self.assertEqual((state["enqueued_total"], state["taken"], state["remaining"]), (100, 100, 0))
        self.assertTrue(state["drained"])

    def test_zero_dynamic_and_multiple_queue_summary(self):
        empty = self.call("summary", self.path)
        self.assertEqual((empty["taken"], empty["enqueued_total"], empty["remaining"]), (0, 0, 0))
        self.assertFalse(empty["drained"])
        self.push(["a", "b"])
        self.call("take", self.path)
        self.push(["c"])
        other = self.path.with_name("other.json")
        self.call("create", other)
        self.call("push", other, "--items", '["d","e"]', "--close")
        result = self.call("summary", self.path, other)
        self.assertEqual((result["taken"], result["enqueued_total"], result["remaining"]), (1, 5, 4))
        self.assertEqual(len(result["queues"]), 2)
        self.assertFalse(result["closed"])
        self.assertIn("distinct", self.call("summary", self.path, self.path, success=False))

    def test_invalid_operations_preserve_existing_file(self):
        self.push(["preserve"])
        original = self.read_bytes()
        for arguments in (
            ("create", self.path),
            ("push", self.path, "--items", '[1]'),
            ("push", self.path, "--items", '{}'),
            ("take", self.path, "--count", 0),
            ("take", self.path, "--wait-ms", -1),
            ("take", self.path, "--wait-ms", 300001),
            ("info", "relative.json"),
            ("info", self.path.with_name("missing.json")),
        ):
            with self.subTest(arguments=arguments):
                self.call(*arguments, success=False)
                self.assertEqual(self.read_bytes(), original)
        self.call("close", self.path)
        closed = self.read_bytes()
        self.assertIn("closed", self.push_error())
        self.assertEqual(self.read_bytes(), closed)

    def push_error(self):
        return self.call("push", self.path, "--items", '["rejected"]', success=False)

    def test_invalid_state_is_never_reset(self):
        valid = json.loads(self.read_bytes())
        cases = [
            b"partial {", b"[]", json.dumps({**valid, "schema_version": 2}).encode(),
            json.dumps({**valid, "enqueued_total": -1}).encode(),
            json.dumps({**valid, "closed": "false"}).encode(),
            json.dumps({**valid, "items": [1]}).encode(),
            json.dumps({**valid, "items": ["lost"], "enqueued_total": 0}).encode(),
        ]
        for content in cases:
            with self.subTest(content=content):
                self.path.write_bytes(content)
                for operation in ("info", "take", "close"):
                    self.call(operation, self.path, success=False)
                    self.assertEqual(self.read_bytes(), content)
                self.call("push", self.path, "--items", '[]', success=False)
                self.assertEqual(self.read_bytes(), content)

    def test_failed_publication_preserves_state_and_cleans_staging(self):
        specification = importlib.util.spec_from_file_location("queue_helper", HELPER)
        helper = importlib.util.module_from_spec(specification)
        specification.loader.exec_module(helper)
        original = self.read_bytes()
        with mock.patch.object(helper.os, "replace", side_effect=OSError("injected replace failure")):
            with self.assertRaisesRegex(OSError, "injected"):
                helper.push(self.path, ["not committed"], False)
        self.assertEqual(self.read_bytes(), original)
        self.assertEqual(sorted(path.name for path in self.path.parent.iterdir()), ["work.json", "work.json.lock"])

    def test_process_exit_releases_stable_lock(self):
        lock = self.path.with_name(self.path.name + ".lock")
        original_inode = lock.stat().st_ino
        holder = subprocess.Popen(
            [sys.executable, "-B", "-c",
             "import fcntl,sys; f=open(sys.argv[1], 'a'); fcntl.flock(f, fcntl.LOCK_EX); "
             "print('locked', flush=True); sys.stdin.read()", str(lock)],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )
        self.addCleanup(self.stop, holder)
        self.assertEqual(holder.stdout.readline().strip(), "locked")
        holder.kill()
        holder.communicate(timeout=5)
        self.push(["after exit"])
        self.assertEqual(self.call("take", self.path)["items"], ["after exit"])
        self.assertEqual(lock.stat().st_ino, original_inode)


if __name__ == "__main__":
    unittest.main()
