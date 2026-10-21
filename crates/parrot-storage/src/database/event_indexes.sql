CREATE INDEX IF NOT EXISTS event_by_session
    ON event (agent_session, sequence);

CREATE INDEX IF NOT EXISTS event_request
    ON event (agent_session, request_id) WHERE request_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS event_request_by_sequence
    ON event (agent_session, sequence) WHERE request_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS event_tool_call
    ON event (agent_session, tool_call_id) WHERE tool_call_id IS NOT NULL;
