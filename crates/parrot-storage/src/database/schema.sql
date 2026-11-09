CREATE TABLE IF NOT EXISTS event (
    sequence      INTEGER PRIMARY KEY AUTOINCREMENT,
    id            TEXT NOT NULL UNIQUE,
    agent_session TEXT NOT NULL,
    payload       BLOB NOT NULL,
    created_at    TEXT NOT NULL,
    request_id    TEXT,
    tool_call_id  TEXT,
    history_anchor INTEGER CHECK (history_anchor >= 0)
);

CREATE TABLE IF NOT EXISTS conversation_item (
    sequence      INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_session TEXT NOT NULL,
    origin        TEXT NOT NULL,
    role          TEXT NOT NULL,
    tool_call_id  TEXT NOT NULL,
    created_at    TEXT NOT NULL,
    final_phase   INTEGER NOT NULL DEFAULT 0,
    provider_id   TEXT NOT NULL DEFAULT '',
    provider_content TEXT NOT NULL DEFAULT '',
    tool_owner_sequence INTEGER REFERENCES conversation_item(sequence) ON DELETE SET NULL,
    mode_status_revision INTEGER CHECK (mode_status_revision >= 0),
    CHECK (
        tool_owner_sequence IS NULL
        OR (
            tool_owner_sequence <> sequence
            AND origin = 'tool'
            AND (role = 'tool' OR (role = 'user' AND tool_call_id = ''))
        )
    )
);

CREATE UNIQUE INDEX IF NOT EXISTS conversation_mode_status
    ON conversation_item (agent_session, mode_status_revision)
    WHERE mode_status_revision IS NOT NULL;

CREATE INDEX IF NOT EXISTS conversation_by_session
    ON conversation_item (agent_session, sequence);

CREATE UNIQUE INDEX IF NOT EXISTS conversation_owned_result
    ON conversation_item (agent_session, tool_call_id)
    WHERE tool_owner_sequence IS NOT NULL AND role = 'tool';

CREATE UNIQUE INDEX IF NOT EXISTS conversation_owned_synthetic
    ON conversation_item (agent_session, tool_owner_sequence)
    WHERE tool_owner_sequence IS NOT NULL AND role = 'user' AND origin = 'tool';

CREATE INDEX IF NOT EXISTS conversation_by_tool_owner
    ON conversation_item (agent_session, tool_owner_sequence, sequence)
    WHERE tool_owner_sequence IS NOT NULL;

CREATE TABLE IF NOT EXISTS conversation_part (
    item_sequence INTEGER NOT NULL REFERENCES conversation_item(sequence) ON DELETE CASCADE,
    position      INTEGER NOT NULL CHECK (position >= 0),
    kind          TEXT NOT NULL,
    text          TEXT NOT NULL,
    artifact_id   TEXT NOT NULL,
    media_type    TEXT NOT NULL,
    display_name  TEXT NOT NULL,
    PRIMARY KEY (item_sequence, position)
);

CREATE INDEX IF NOT EXISTS conversation_part_by_artifact ON conversation_part (artifact_id);

CREATE TABLE IF NOT EXISTS conversation_tool_call (
    item_sequence INTEGER NOT NULL REFERENCES conversation_item(sequence) ON DELETE CASCADE,
    position      INTEGER NOT NULL CHECK (position >= 0),
    id            TEXT NOT NULL,
    name          TEXT NOT NULL,
    arguments     TEXT NOT NULL,
    PRIMARY KEY (item_sequence, position)
);

CREATE TABLE IF NOT EXISTS input (
    sequence      INTEGER PRIMARY KEY AUTOINCREMENT,
    id            TEXT NOT NULL UNIQUE,
    agent_session TEXT NOT NULL,
    message_id    TEXT NOT NULL,
    content       TEXT NOT NULL,
    delivery      TEXT NOT NULL,
    status        TEXT NOT NULL,
    created_at    TEXT NOT NULL,
    promoted_at   TEXT
);

-- The sender's message id is what makes admission idempotent, so
-- a re-send after a dropped connection cannot admit twice.
CREATE UNIQUE INDEX IF NOT EXISTS input_by_message ON input (agent_session, message_id);

CREATE INDEX IF NOT EXISTS input_pending ON input (agent_session, status, sequence);

CREATE TABLE IF NOT EXISTS input_part (
    input_id      TEXT NOT NULL REFERENCES input(id) ON DELETE CASCADE,
    position      INTEGER NOT NULL CHECK (position >= 0),
    kind          TEXT NOT NULL,
    text          TEXT NOT NULL,
    artifact_id   TEXT NOT NULL,
    media_type    TEXT NOT NULL,
    display_name  TEXT NOT NULL,
    PRIMARY KEY (input_id, position)
);

CREATE INDEX IF NOT EXISTS input_part_by_artifact ON input_part (artifact_id);

CREATE TABLE IF NOT EXISTS tool_execution_terminal (
    sequence      INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_session TEXT NOT NULL,
    tool_call_id  TEXT NOT NULL,
    tool_name     TEXT NOT NULL,
    status        TEXT NOT NULL,
    message       TEXT NOT NULL,
    created_at    TEXT NOT NULL,
    UNIQUE (agent_session, tool_call_id)
);

CREATE TABLE IF NOT EXISTS tool_execution_result_part (
    terminal_sequence INTEGER NOT NULL REFERENCES tool_execution_terminal(sequence) ON DELETE CASCADE,
    position          INTEGER NOT NULL CHECK (position >= 0),
    kind              TEXT NOT NULL,
    text              TEXT NOT NULL,
    artifact_id       TEXT NOT NULL,
    media_type        TEXT NOT NULL,
    display_name      TEXT NOT NULL,
    PRIMARY KEY (terminal_sequence, position)
);

CREATE INDEX IF NOT EXISTS tool_execution_result_part_by_artifact
    ON tool_execution_result_part (artifact_id);

CREATE TABLE IF NOT EXISTS agent_history (
    sequence              INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_session         TEXT NOT NULL,
    kind                  TEXT NOT NULL CHECK (kind IN ('message', 'compaction')),
    conversation_sequence INTEGER REFERENCES conversation_item(sequence) ON DELETE CASCADE,
    summary               TEXT NOT NULL,
    watermark             INTEGER NOT NULL CHECK (watermark >= 0),
    created_at            TEXT NOT NULL,
    status_conversation_sequence INTEGER UNIQUE REFERENCES conversation_item(sequence) ON DELETE SET NULL,
    CHECK (
        (kind = 'message' AND conversation_sequence IS NOT NULL AND summary = '' AND watermark = 0 AND status_conversation_sequence IS NULL)
        OR
        (kind = 'compaction' AND conversation_sequence IS NULL)
    ),
    UNIQUE (conversation_sequence)
);

CREATE INDEX IF NOT EXISTS agent_history_by_session
    ON agent_history (agent_session, sequence);

CREATE UNIQUE INDEX IF NOT EXISTS agent_history_compaction
    ON agent_history (agent_session, watermark) WHERE kind = 'compaction';

CREATE INDEX IF NOT EXISTS agent_history_latest_compaction
    ON agent_history (agent_session, sequence DESC) WHERE kind = 'compaction';

CREATE TABLE IF NOT EXISTS history_checkpoint (
    sequence           INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_session      TEXT NOT NULL,
    title              TEXT NOT NULL,
    assistant_sequence INTEGER NOT NULL REFERENCES conversation_item(sequence) ON DELETE CASCADE,
    tool_call_id        TEXT NOT NULL,
    created_at          TEXT NOT NULL,
    UNIQUE (agent_session, tool_call_id)
);

CREATE INDEX IF NOT EXISTS history_checkpoint_by_title
    ON history_checkpoint (agent_session, title, sequence DESC);

CREATE TABLE IF NOT EXISTS context_reminder (
    sequence        INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_session   TEXT NOT NULL,
    canonical_model TEXT NOT NULL,
    context_limit   INTEGER NOT NULL CHECK (context_limit > 0),
    percentage      INTEGER NOT NULL CHECK (percentage > 0),
    created_at      TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS context_reminder_by_session
    ON context_reminder (agent_session, sequence DESC);

CREATE TABLE IF NOT EXISTS image_artifact (
    artifact_id      TEXT PRIMARY KEY,
    media_type       TEXT NOT NULL,
    byte_length      INTEGER NOT NULL CHECK (byte_length >= 0),
    width            INTEGER NOT NULL CHECK (width > 0),
    height           INTEGER NOT NULL CHECK (height > 0),
    frame_count      INTEGER NOT NULL CHECK (frame_count > 0),
    aggregate_pixels INTEGER NOT NULL CHECK (aggregate_pixels > 0),
    display_name     TEXT NOT NULL,
    origin           TEXT NOT NULL,
    created_at       TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS image_upload (
    upload_id   TEXT PRIMARY KEY,
    artifact_id TEXT NOT NULL REFERENCES image_artifact (artifact_id),
    created_at  TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS image_artifact_unreferenced ON image_artifact (created_at);
