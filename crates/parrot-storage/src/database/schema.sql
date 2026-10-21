CREATE TABLE IF NOT EXISTS event (
    sequence      INTEGER PRIMARY KEY AUTOINCREMENT,
    id            TEXT NOT NULL UNIQUE,
    agent_session TEXT NOT NULL,
    payload       BLOB NOT NULL,
    created_at    TEXT NOT NULL,
    request_id    TEXT,
    tool_call_id  TEXT
);

CREATE TABLE IF NOT EXISTS message (
    sequence      INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_session TEXT NOT NULL,
    role          TEXT NOT NULL,
    content       TEXT NOT NULL,
    created_at    TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS message_by_session ON message (agent_session, sequence);

CREATE TABLE IF NOT EXISTS conversation_item (
    sequence      INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_session TEXT NOT NULL,
    origin        TEXT NOT NULL,
    role          TEXT NOT NULL,
    tool_call_id  TEXT NOT NULL,
    created_at    TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS conversation_by_session
    ON conversation_item (agent_session, sequence);

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

CREATE TABLE IF NOT EXISTS conversation_tool_call (
    item_sequence INTEGER NOT NULL REFERENCES conversation_item(sequence) ON DELETE CASCADE,
    position      INTEGER NOT NULL CHECK (position >= 0),
    id            TEXT NOT NULL,
    name          TEXT NOT NULL,
    arguments     TEXT NOT NULL,
    PRIMARY KEY (item_sequence, position)
);

CREATE TABLE IF NOT EXISTS conversation_provider_content (
    item_sequence INTEGER PRIMARY KEY REFERENCES conversation_item(sequence) ON DELETE CASCADE,
    provider      TEXT NOT NULL,
    content       TEXT NOT NULL
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

CREATE TABLE IF NOT EXISTS tool_batch_result (
    agent_session     TEXT NOT NULL,
    assistant_sequence INTEGER NOT NULL REFERENCES conversation_item(sequence) ON DELETE CASCADE,
    tool_call_id      TEXT NOT NULL,
    item_sequence     INTEGER NOT NULL REFERENCES conversation_item(sequence) ON DELETE CASCADE,
    PRIMARY KEY (agent_session, assistant_sequence, tool_call_id),
    UNIQUE (agent_session, tool_call_id)
);

CREATE TABLE IF NOT EXISTS tool_batch_synthetic (
    agent_session      TEXT NOT NULL,
    assistant_sequence INTEGER NOT NULL REFERENCES conversation_item(sequence) ON DELETE CASCADE,
    item_sequence      INTEGER NOT NULL REFERENCES conversation_item(sequence) ON DELETE CASCADE,
    PRIMARY KEY (agent_session, assistant_sequence),
    UNIQUE (item_sequence)
);

CREATE TABLE IF NOT EXISTS compaction_snapshot (
    agent_session TEXT PRIMARY KEY,
    summary       TEXT NOT NULL,
    watermark     INTEGER NOT NULL CHECK (watermark >= 0),
    created_at    TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS compaction_status (
    agent_session               TEXT NOT NULL,
    watermark                   INTEGER NOT NULL CHECK (watermark >= 0),
    status_conversation_sequence INTEGER NOT NULL REFERENCES conversation_item(sequence) ON DELETE CASCADE,
    PRIMARY KEY (agent_session, watermark),
    UNIQUE (status_conversation_sequence)
);

CREATE TABLE IF NOT EXISTS agent_history (
    sequence              INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_session         TEXT NOT NULL,
    kind                  TEXT NOT NULL CHECK (kind IN ('message', 'compaction')),
    conversation_sequence INTEGER REFERENCES conversation_item(sequence) ON DELETE CASCADE,
    summary               TEXT NOT NULL,
    watermark             INTEGER NOT NULL CHECK (watermark >= 0),
    created_at            TEXT NOT NULL,
    CHECK (
        (kind = 'message' AND conversation_sequence IS NOT NULL AND summary = '' AND watermark = 0)
        OR
        (kind = 'compaction' AND conversation_sequence IS NULL)
    ),
    UNIQUE (conversation_sequence)
);

CREATE TABLE IF NOT EXISTS request_history_anchor (
    event_sequence INTEGER PRIMARY KEY REFERENCES event(sequence),
    agent_session TEXT NOT NULL,
    history_sequence INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS agent_history_by_session
    ON agent_history (agent_session, sequence);

CREATE UNIQUE INDEX IF NOT EXISTS agent_history_compaction
    ON agent_history (agent_session, watermark) WHERE kind = 'compaction';

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

CREATE TABLE IF NOT EXISTS mode_change (
    sequence      INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_session TEXT NOT NULL,
    mode          TEXT NOT NULL,
    created_at    TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS mode_change_by_session
    ON mode_change (agent_session, sequence);

CREATE TABLE IF NOT EXISTS status_prompt (
    sequence             INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_session        TEXT NOT NULL,
    mode_change_sequence INTEGER NOT NULL,
    created_at           TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS status_prompt_by_session
    ON status_prompt (agent_session, sequence);

CREATE TABLE IF NOT EXISTS final_provider_request_prompt (
    agent_session TEXT PRIMARY KEY,
    created_at    TEXT NOT NULL
);

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

CREATE TABLE IF NOT EXISTS session_state (
    user_session  TEXT PRIMARY KEY,
    agent_session TEXT NOT NULL,
    mode          TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS agent_usage (
    agent_session      TEXT PRIMARY KEY,
    revision           INTEGER NOT NULL CHECK (revision > 0),
    input_tokens       INTEGER NOT NULL,
    cached_input_tokens INTEGER NOT NULL,
    output_tokens      INTEGER NOT NULL,
    context_size       INTEGER NOT NULL,
    context_limit      INTEGER NOT NULL,
    input_cost         REAL NOT NULL,
    output_cost        REAL NOT NULL
);

CREATE TABLE IF NOT EXISTS image_content (
    sha256           TEXT PRIMARY KEY,
    media_type       TEXT NOT NULL,
    byte_length      INTEGER NOT NULL CHECK (byte_length >= 0),
    width            INTEGER NOT NULL CHECK (width > 0),
    height           INTEGER NOT NULL CHECK (height > 0),
    frame_count      INTEGER NOT NULL CHECK (frame_count > 0),
    aggregate_pixels INTEGER NOT NULL CHECK (aggregate_pixels > 0)
);

CREATE TABLE IF NOT EXISTS image_artifact (
    artifact_id  TEXT PRIMARY KEY,
    sha256       TEXT NOT NULL REFERENCES image_content (sha256),
    display_name TEXT NOT NULL,
    origin       TEXT NOT NULL,
    created_at   TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS image_upload (
    upload_id   TEXT PRIMARY KEY,
    artifact_id TEXT NOT NULL REFERENCES image_artifact (artifact_id),
    created_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS image_artifact_reference (
    artifact_id  TEXT NOT NULL REFERENCES image_artifact (artifact_id),
    reference_id TEXT NOT NULL,
    PRIMARY KEY (artifact_id, reference_id)
);

CREATE INDEX IF NOT EXISTS image_artifact_unreferenced
    ON image_artifact (created_at);

CREATE TABLE IF NOT EXISTS projection_version (
    name    TEXT PRIMARY KEY,
    version INTEGER NOT NULL
);
