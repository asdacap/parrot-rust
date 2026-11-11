CREATE INDEX IF NOT EXISTS conversation_latest_assistant
    ON conversation_item (agent_session, sequence DESC)
    WHERE origin = 'model' AND role = 'assistant' AND kind <> 'compaction';
