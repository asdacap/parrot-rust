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
    kind          TEXT NOT NULL DEFAULT 'message',
    tool_call_id  TEXT NOT NULL,
    tool_result_name TEXT,
    tool_result_status TEXT,
    created_at    TEXT NOT NULL,
    final_phase   INTEGER NOT NULL DEFAULT 0,
    context_reminder TEXT CHECK (
        json_extract(context_reminder, '$.context_limit') > 0
        AND json_extract(context_reminder, '$.percentage') > 0
    ),
    provider_id   TEXT NOT NULL DEFAULT '',
    provider_content TEXT NOT NULL DEFAULT '',
    tool_owner_sequence INTEGER REFERENCES conversation_item(sequence) ON DELETE SET NULL,
    mode_status_revision INTEGER CHECK (mode_status_revision >= 0),
    compaction_watermark INTEGER CHECK (compaction_watermark >= 0),
    compaction_status_sequence INTEGER UNIQUE REFERENCES conversation_item(sequence) ON DELETE SET NULL,
    CHECK (
        (kind = 'compaction' AND compaction_watermark IS NOT NULL)
        OR
        (kind <> 'compaction' AND compaction_watermark IS NULL AND compaction_status_sequence IS NULL)
    ),
    CHECK (
        (tool_result_name IS NULL AND tool_result_status IS NULL)
        OR (
            tool_result_name IS NOT NULL AND tool_result_status IS NOT NULL
            AND origin = 'tool' AND role = 'tool'
        )
    ),
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

CREATE INDEX IF NOT EXISTS conversation_context_reminder_by_session
    ON conversation_item (agent_session, sequence DESC)
    WHERE context_reminder IS NOT NULL;

CREATE INDEX IF NOT EXISTS conversation_context_reminder_by_model
    ON conversation_item (
        agent_session,
        json_extract(context_reminder, '$.canonical_model'),
        json_extract(context_reminder, '$.context_limit'),
        sequence DESC
    )
    WHERE context_reminder IS NOT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS conversation_tool_result
    ON conversation_item (agent_session, tool_call_id)
    WHERE tool_result_status IS NOT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS conversation_owned_result
    ON conversation_item (agent_session, tool_call_id)
    WHERE tool_owner_sequence IS NOT NULL AND role = 'tool';

CREATE UNIQUE INDEX IF NOT EXISTS conversation_owned_synthetic
    ON conversation_item (agent_session, tool_owner_sequence)
    WHERE tool_owner_sequence IS NOT NULL AND role = 'user' AND origin = 'tool';

CREATE INDEX IF NOT EXISTS conversation_by_tool_owner
    ON conversation_item (agent_session, tool_owner_sequence, sequence)
    WHERE tool_owner_sequence IS NOT NULL;

CREATE INDEX IF NOT EXISTS conversation_checkpoint_results
    ON conversation_item (agent_session, tool_owner_sequence, tool_call_id)
    WHERE role = 'tool' AND tool_result_name = 'set_checkpoint'
        AND tool_result_status = 'finished';

CREATE UNIQUE INDEX IF NOT EXISTS conversation_compaction
    ON conversation_item (agent_session, compaction_watermark) WHERE kind = 'compaction';

CREATE INDEX IF NOT EXISTS conversation_latest_compaction
    ON conversation_item (agent_session, sequence DESC) WHERE kind = 'compaction';

CREATE TABLE IF NOT EXISTS conversation_part (
    item_sequence INTEGER NOT NULL REFERENCES conversation_item(sequence) ON DELETE CASCADE,
    position      INTEGER NOT NULL CHECK (position >= 0),
    kind          TEXT NOT NULL,
    text          TEXT NOT NULL,
    artifact_id   TEXT NOT NULL,
    media_type    TEXT NOT NULL,
    display_name  TEXT NOT NULL,
    tool_call_id  TEXT NOT NULL DEFAULT '',
    tool_name     TEXT NOT NULL DEFAULT '',
    arguments     TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (item_sequence, position)
);

CREATE INDEX IF NOT EXISTS conversation_part_by_artifact ON conversation_part (artifact_id);

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
