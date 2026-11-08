#!/usr/bin/env python3
"""Coordinate waiting work items through shared, locked JSON files."""

import argparse
from contextlib import contextmanager
import fcntl
import json
import os
from pathlib import Path
import sys
import tempfile
import time


@contextmanager
def lock_queue(path):
    with path.with_name(path.name + ".lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        yield


def read_state(path):
    with path.open(encoding="utf-8") as source:
        state = json.load(source)
    fields = {"schema_version", "description", "closed", "enqueued_total", "items"}
    if not isinstance(state, dict) or set(state) != fields:
        raise ValueError("queue state must contain exactly the version 1 fields")
    if type(state["schema_version"]) is not int or state["schema_version"] != 1:
        raise ValueError("unsupported queue schema_version")
    if not isinstance(state["description"], str) or type(state["closed"]) is not bool:
        raise ValueError("invalid queue description or closed state")
    items = state["items"]
    if not isinstance(items, list) or any(not isinstance(item, str) for item in items):
        raise ValueError("queue items must be strings")
    if type(state["enqueued_total"]) is not int or state["enqueued_total"] < len(items):
        raise ValueError("invalid queue enqueued_total")
    return state


def publish_state(path, state):
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="w", encoding="utf-8", dir=path.parent,
            prefix="." + path.name + ".", delete=False,
        ) as output:
            temporary = Path(output.name)
            json.dump(state, output, ensure_ascii=False)
            output.write("\n")
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def describe(path, state):
    remaining = len(state["items"])
    return {
        "path": str(path),
        "description": state["description"],
        "closed": state["closed"],
        "enqueued_total": state["enqueued_total"],
        "taken": state["enqueued_total"] - remaining,
        "remaining": remaining,
        "drained": state["closed"] and remaining == 0,
    }


def create(path, description):
    path.parent.mkdir(parents=True, exist_ok=True)
    with lock_queue(path):
        if path.exists():
            raise ValueError("queue already exists")
        state = {
            "schema_version": 1, "description": description, "closed": False,
            "enqueued_total": 0, "items": [],
        }
        publish_state(path, state)
        return describe(path, state)


def push(path, items, close):
    if not isinstance(items, list) or any(not isinstance(item, str) for item in items):
        raise ValueError("items must be a JSON array of strings")
    with lock_queue(path):
        state = read_state(path)
        if state["closed"]:
            raise ValueError("queue is closed")
        state["items"].extend(items)
        state["enqueued_total"] += len(items)
        state["closed"] = close
        publish_state(path, state)
        return describe(path, state)


def take(path, count, wait_ms):
    deadline = time.monotonic() + wait_ms / 1000
    while True:
        with lock_queue(path):
            state = read_state(path)
            if state["items"]:
                items = state["items"][:count]
                del state["items"][:count]
                publish_state(path, state)
                return {**describe(path, state), "items": items}
            if state["closed"] or time.monotonic() >= deadline:
                return {**describe(path, state), "items": []}
        time.sleep(min(0.05, max(0, deadline - time.monotonic())))


def info(path):
    with lock_queue(path):
        return describe(path, read_state(path))


def close_queue(path):
    with lock_queue(path):
        state = read_state(path)
        if not state["closed"]:
            state["closed"] = True
            publish_state(path, state)
        return describe(path, state)


def summarize(paths):
    if len(set(paths)) != len(paths):
        raise ValueError("summary paths must be distinct")
    queues = [info(path) for path in paths]
    return {
        "queues": queues,
        "enqueued_total": sum(queue["enqueued_total"] for queue in queues),
        "taken": sum(queue["taken"] for queue in queues),
        "remaining": sum(queue["remaining"] for queue in queues),
        "closed": all(queue["closed"] for queue in queues),
        "drained": all(queue["drained"] for queue in queues),
    }


def absolute_path(value):
    path = Path(value)
    if not path.is_absolute():
        raise argparse.ArgumentTypeError("queue path must be absolute")
    return path


def positive_integer(value):
    number = int(value)
    if number < 1:
        raise argparse.ArgumentTypeError("count must be positive")
    return number


def bounded_wait(value):
    number = int(value)
    if not 0 <= number <= 300000:
        raise argparse.ArgumentTypeError("wait-ms must be between 0 and 300000")
    return number


def parse_arguments():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="operation", required=True)
    for name in ("create", "push", "take", "info", "close"):
        command = commands.add_parser(name)
        command.add_argument("path", type=absolute_path)
        if name == "create":
            command.add_argument("--description", default="")
        elif name == "push":
            source = command.add_mutually_exclusive_group(required=True)
            source.add_argument("--items", help="JSON array of string items")
            source.add_argument("--source-file", type=Path, help="one item per nonblank UTF-8 line")
            command.add_argument("--close", action="store_true")
        elif name == "take":
            command.add_argument("--count", type=positive_integer, default=1)
            command.add_argument("--wait-ms", type=bounded_wait, default=0)
    summary = commands.add_parser("summary")
    summary.add_argument("paths", type=absolute_path, nargs="+")
    return parser.parse_args()


def run(arguments):
    if arguments.operation == "create":
        return create(arguments.path, arguments.description)
    if arguments.operation == "push":
        if arguments.source_file is not None:
            with arguments.source_file.open(encoding="utf-8") as source:
                items = [line.rstrip("\r\n") for line in source if line.strip()]
        else:
            items = json.loads(arguments.items)
        return push(arguments.path, items, arguments.close)
    if arguments.operation == "take":
        return take(arguments.path, arguments.count, arguments.wait_ms)
    if arguments.operation == "info":
        return info(arguments.path)
    if arguments.operation == "close":
        return close_queue(arguments.path)
    return summarize(arguments.paths)


def main():
    arguments = parse_arguments()
    try:
        result = run(arguments)
        print(json.dumps(result, ensure_ascii=False))
        return 0
    except (OSError, ValueError) as error:
        print(f"queue: {error}. Inspect state before retrying an interrupted mutation.", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
