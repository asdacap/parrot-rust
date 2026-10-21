CREATE TABLE agent_lineage (
    agent_session        TEXT PRIMARY KEY,
    parent_agent_session TEXT NOT NULL,
    name                 TEXT NOT NULL,
    event_sequence       INTEGER NOT NULL REFERENCES event(sequence)
);
