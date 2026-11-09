#!/usr/bin/env python3
import sqlite3
from pathlib import Path


root = Path(__file__).resolve().parents[1]
connection = sqlite3.connect(":memory:")
connection.execute("PRAGMA foreign_keys = ON")
connection.executescript(
    (root / "crates/parrot-storage/src/database/schema.sql").read_text()
)
tables = {
    row[0]
    for row in connection.execute(
        "SELECT name FROM sqlite_schema WHERE type = 'table'"
    )
}
removed = {"tool_execution_terminal", "tool_execution_result_part"}
assert not tables & removed, f"Removed result tables still exist: {tables & removed}"
connection.executescript(
    """
    INSERT INTO conversation_item
        (sequence, agent_session, origin, role, tool_call_id, created_at)
        VALUES (1, 'agent', 'model', 'assistant', '', 'at');
    INSERT INTO conversation_item
        (sequence, agent_session, origin, role, tool_call_id, created_at,
         tool_owner_sequence, tool_result_name, tool_result_status)
        VALUES (2, 'agent', 'tool', 'tool', 'call', 'at', 1, 'shell', 'finished');
    INSERT INTO conversation_part
        (item_sequence, position, kind, text, artifact_id, media_type, display_name)
        VALUES (2, 0, 'text', 'one authoritative result', '', '', '');
    DELETE FROM conversation_item WHERE sequence = 1;
    """
)
assert connection.execute(
    "SELECT tool_owner_sequence, tool_result_name, tool_result_status "
    "FROM conversation_item WHERE sequence = 2"
).fetchone() == (None, "shell", "finished"), "Owner deletion lost the saved result"
assert connection.execute(
    "SELECT text FROM conversation_part WHERE item_sequence = 2 ORDER BY position"
).fetchall() == [("one authoritative result",)], "The result content changed"
try:
    connection.execute(
        "INSERT INTO conversation_item "
        "(agent_session, origin, role, tool_call_id, created_at, "
        "tool_result_name, tool_result_status) "
        "VALUES ('agent', 'tool', 'tool', 'call', 'at', 'shell', 'finished')"
    )
except sqlite3.IntegrityError:
    pass
else:
    raise AssertionError("The same tool call admitted a second execution result")
print("PASS. Both result tables are absent and conversation-owned results retain their identity.")
