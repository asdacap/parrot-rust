//! Durable usage facts, legacy replacement projections and source replay.
//! All functions execute inside the caller's whole repository transaction.

use super::{AgentLineageRecord, EventError, SessionUsage};
use crate::diagnostics::statistics::AgentStatisticsReplay;
use parrot_domain::protocol::messages::parrot::{
    AgentStatisticsUpdatedEvent, Event, RequestUsageRecorded,
};
use parrot_domain::values::accounting::{
    AgentSessionStatistics, AgentSessionStatisticsSnapshot, AgentUsageIncrement, AgentUsageKey,
    AgentUsageTotals,
};
use protobuf::Message;
use rusqlite::{OptionalExtension, Transaction, params};
use std::collections::{HashMap, HashSet};

pub fn append_usage_fact(
    tx: &Transaction<'_>,
    event: &Event,
    clock: &dyn Fn() -> String,
) -> Result<i64, EventError> {
    if !event.has_request_usage_recorded() && !event.has_tool_execution_started() {
        return Err(EventError::Argument(
            "An individual usage fact is required.".into(),
        ));
    }
    let existing: Option<(i64, Vec<u8>)> = tx
        .query_row(
            "SELECT sequence, payload FROM event WHERE id = ?1;",
            [&event.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((revision, bytes)) = existing {
        if !events_semantically_equal(&Event::parse_from_bytes(&bytes)?, event) {
            return Err(EventError::Invalid(
                "A usage fact identity was reused with different content.".into(),
            ));
        }
        return Ok(revision);
    }
    let (sql, key) = if event.has_request_usage_recorded() {
        (
            "SELECT EXISTS (SELECT 1 FROM event WHERE agent_session = ?1 AND request_id = ?2);",
            event.request_usage_recorded().request_id.as_str(),
        )
    } else {
        (
            "SELECT EXISTS (SELECT 1 FROM event WHERE agent_session = ?1 AND tool_call_id = ?2);",
            event.tool_execution_started().tool_call_id.as_str(),
        )
    };
    if tx.query_row(sql, params![event.agent_session_id, key], |row| {
        row.get::<_, bool>(0)
    })? {
        return Err(EventError::Invalid(
            "A request or tool execution already has a usage fact with another event identity."
                .into(),
        ));
    }
    let revision = super::record(tx, event, None, &clock())?.sequence;
    if event.has_request_usage_recorded() {
        super::projection::ensure_agent_history_projection(tx)?;
        tx.execute(
            "INSERT INTO request_history_anchor (event_sequence, agent_session, history_sequence) SELECT ?1, ?2, COALESCE(MAX(sequence), 0) FROM agent_history WHERE agent_session = ?2;",
            params![revision, event.agent_session_id],
        )?;
    }
    Ok(revision)
}

// C# generated protobuf equality compares double fields by their bit patterns,
// including signed zero and NaN payloads. All other generated fields (including
// unknowns and optional effort presence) retain normal message equality.
pub(super) fn events_semantically_equal(left: &Event, right: &Event) -> bool {
    if left.has_request_usage_recorded() && right.has_request_usage_recorded() {
        let a = left.request_usage_recorded();
        let b = right.request_usage_recorded();
        if a.input_cost.to_bits() != b.input_cost.to_bits()
            || a.output_cost.to_bits() != b.output_cost.to_bits()
        {
            return false;
        }
        let mut left = left.clone();
        let mut right = right.clone();
        left.mut_request_usage_recorded().input_cost = 0.0;
        left.mut_request_usage_recorded().output_cost = 0.0;
        right.mut_request_usage_recorded().input_cost = 0.0;
        right.mut_request_usage_recorded().output_cost = 0.0;
        left == right
    } else if left.has_agent_statistics_updated() && right.has_agent_statistics_updated() {
        let a = left.agent_statistics_updated();
        let b = right.agent_statistics_updated();
        if a.input_cost.to_bits() != b.input_cost.to_bits()
            || a.output_cost.to_bits() != b.output_cost.to_bits()
        {
            return false;
        }
        let mut left = left.clone();
        let mut right = right.clone();
        left.mut_agent_statistics_updated().input_cost = 0.0;
        left.mut_agent_statistics_updated().output_cost = 0.0;
        right.mut_agent_statistics_updated().input_cost = 0.0;
        right.mut_agent_statistics_updated().output_cost = 0.0;
        left == right
    } else if left.has_session_usage_snapshot() && right.has_session_usage_snapshot() {
        let a = left.session_usage_snapshot();
        let b = right.session_usage_snapshot();
        if a.input_cost.to_bits() != b.input_cost.to_bits()
            || a.output_cost.to_bits() != b.output_cost.to_bits()
        {
            return false;
        }
        let mut left = left.clone();
        let mut right = right.clone();
        left.mut_session_usage_snapshot().input_cost = 0.0;
        left.mut_session_usage_snapshot().output_cost = 0.0;
        right.mut_session_usage_snapshot().input_cost = 0.0;
        right.mut_session_usage_snapshot().output_cost = 0.0;
        left == right
    } else {
        left == right
    }
}

pub fn find_request_usage(
    tx: &Transaction<'_>,
    agent: &str,
    call: &str,
) -> Result<Option<RequestUsageRecorded>, EventError> {
    let mut read = tx.prepare("SELECT payload FROM event WHERE agent_session = ?1 AND request_id IS NOT NULL ORDER BY sequence DESC;")?;
    let mut rows = read.query([agent])?;
    while let Some(row) = rows.next()? {
        let event = Event::parse_from_bytes(&row.get::<_, Vec<u8>>(0)?)?;
        let request = event.request_usage_recorded();
        if request.tool_call_ids.iter().any(|id| id == call) {
            return Ok(Some(request.clone()));
        }
    }
    Ok(None)
}

pub fn agent_lineage(tx: &Transaction<'_>) -> Result<Vec<AgentLineageRecord>, EventError> {
    let mut read = tx.prepare("SELECT agent_session, parent_agent_session, name FROM agent_lineage ORDER BY event_sequence;")?;
    Ok(read
        .query_map([], |row| {
            Ok(AgentLineageRecord {
                agent_session_id: row.get(0)?,
                parent_agent_session_id: row.get(1)?,
                name: row.get(2)?,
            })
        })?
        .collect::<Result<_, _>>()?)
}

fn read_statistics_events(tx: &Transaction<'_>) -> Result<Vec<(i64, Event)>, EventError> {
    let mut read = tx.prepare("SELECT sequence, payload FROM event ORDER BY sequence;")?;
    let mut rows = read.query([])?;
    let mut events = Vec::new();
    while let Some(row) = rows.next()? {
        events.push((
            row.get(0)?,
            Event::parse_from_bytes(&row.get::<_, Vec<u8>>(1)?)?,
        ));
    }
    Ok(events)
}

fn record_parent(
    parents: &mut HashMap<String, String>,
    parent_order: &mut Vec<String>,
    agent: &str,
    parent: &str,
) -> Result<(), EventError> {
    if let Some(existing) = parents.get(agent) {
        if existing != parent {
            return Err(EventError::Invalid(
                "Historical agent lineage contains conflicting parents.".into(),
            ));
        }
    } else {
        parent_order.push(agent.to_owned());
    }
    parents.insert(agent.to_owned(), parent.to_owned());
    Ok(())
}

fn ensure_agent(
    agents: &mut HashMap<String, AgentSessionStatistics>,
    agent_order: &mut Vec<String>,
    agent: &str,
) {
    if let std::collections::hash_map::Entry::Vacant(entry) = agents.entry(agent.to_owned()) {
        agent_order.push(agent.to_owned());
        entry.insert(AgentSessionStatistics::new(
            AgentSessionStatisticsSnapshot::default(),
        ));
    }
}

pub fn replay_statistics(tx: &Transaction<'_>) -> Result<AgentStatisticsReplay, EventError> {
    let facts = read_statistics_events(tx)?;
    let mut parents = HashMap::new();
    let mut parent_order = Vec::new();
    let mut agents = HashMap::new();
    // .NET dictionaries enumerate in insertion order. Preserve that order for
    // floating-point accumulation and source-first validation diagnostics.
    let mut agent_order = Vec::new();
    let mut baselines = HashMap::<String, (i64, AgentStatisticsUpdatedEvent)>::new();
    let mut baseline_order = Vec::new();
    for (revision, event) in &facts {
        let agent = &event.agent_session_id;
        ensure_agent(&mut agents, &mut agent_order, agent);
        let parent = if event.has_agent_started() {
            Some(event.agent_started().parent_agent_session_id.as_str())
        } else if event.has_agent_finished() {
            Some(event.agent_finished().parent_agent_session_id.as_str())
        } else if event.has_agent_failed() {
            Some(event.agent_failed().parent_agent_session_id.as_str())
        } else if event.has_request_usage_recorded() {
            Some(
                event
                    .request_usage_recorded()
                    .parent_agent_session_id
                    .as_str(),
            )
        } else if event.has_tool_execution_started() {
            Some(
                event
                    .tool_execution_started()
                    .parent_agent_session_id
                    .as_str(),
            )
        } else {
            None
        };
        if let Some(parent) = parent {
            record_parent(&mut parents, &mut parent_order, agent, parent)?;
        }
        if event.has_agent_statistics_updated() {
            if !baselines.contains_key(agent) {
                baseline_order.push(agent.clone());
            }
            baselines.insert(
                agent.clone(),
                (*revision, event.agent_statistics_updated().clone()),
            );
        }
    }
    for agent in &baseline_order {
        let value = &baselines[agent].1;
        agents[agent].add_own(&AgentUsageIncrement {
            key: AgentUsageKey::legacy(),
            totals: AgentUsageTotals {
                input_tokens: value.input_tokens,
                cached_input_tokens: value.cached_input_tokens,
                output_tokens: value.output_tokens,
                input_cost: value.input_cost,
                output_cost: value.output_cost,
                tool_calls: 0,
            },
            context_size: Some(value.context_size),
            context_limit: Some(value.context_limit),
        })?;
    }
    let mut requests = HashSet::new();
    let mut tools = HashSet::new();
    for (revision, event) in &facts {
        let agent = &event.agent_session_id;
        if event.has_request_usage_recorded() {
            let request = event.request_usage_recorded();
            if requests.insert((agent.clone(), request.request_id.clone()))
                && baselines
                    .get(agent)
                    .is_none_or(|(baseline, _)| revision > baseline)
            {
                agents[agent].add_own(&AgentUsageIncrement {
                    key: AgentUsageKey::from_request(
                        &request.provider,
                        &request.model,
                        request.effort.clone(),
                    ),
                    totals: AgentUsageTotals {
                        input_tokens: request.input_tokens,
                        cached_input_tokens: request.cached_input_tokens,
                        output_tokens: request.output_tokens,
                        input_cost: request.input_cost,
                        output_cost: request.output_cost,
                        tool_calls: 0,
                    },
                    context_size: Some(request.context_size),
                    context_limit: Some(request.context_limit),
                })?;
            }
        } else if event.has_tool_execution_started() {
            let tool = event.tool_execution_started();
            if tools.insert((agent.clone(), tool.tool_call_id.clone())) {
                agents[agent].add_own(&AgentUsageIncrement::from_tool_start(
                    AgentUsageKey::from_request(&tool.provider, &tool.model, tool.effort.clone()),
                ))?;
            }
        }
    }
    let legacy_starts: HashSet<(String, String)> = facts
        .iter()
        .filter(|(_, event)| event.has_tool_started())
        .map(|(_, event)| {
            (
                event.agent_session_id.clone(),
                event.tool_started().tool_call_id.clone(),
            )
        })
        .collect();
    for (_, event) in &facts {
        if event.has_tool_finished() {
            let key = (
                event.agent_session_id.clone(),
                event.tool_finished().tool_call_id.clone(),
            );
            if legacy_starts.contains(&key) && tools.insert(key) {
                agents[&event.agent_session_id].add_own(&AgentUsageIncrement::from_tool_start(
                    AgentUsageKey::legacy(),
                ))?;
            }
        }
    }
    let incomplete_legacy_tool_counts = legacy_starts
        .into_iter()
        .filter(|key| !tools.contains(key))
        .map(|(agent, _)| agent)
        .collect();
    let main: Option<String> = tx
        .query_row(
            "SELECT agent_session FROM session_state LIMIT 1;",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(main) = main {
        record_parent(&mut parents, &mut parent_order, &main, "")?;
    }
    for agent in &agent_order {
        if !agents[agent].capture().own.models.is_empty() && !parents.contains_key(agent) {
            return Err(EventError::Invalid(format!(
                "Historical usage has no parent lineage for agent {agent}."
            )));
        }
    }
    for agent in &parent_order {
        let parent = &parents[agent];
        if !parent.is_empty() {
            if !parents.contains_key(parent) {
                return Err(EventError::Invalid(format!(
                    "Historical ancestor {parent} has no parent lineage."
                )));
            }
            ensure_agent(&mut agents, &mut agent_order, parent);
        }
    }
    for agent in &agent_order {
        let mut visited = HashSet::from([agent.clone()]);
        let mut ancestor = agent.as_str();
        while let Some(parent) = parents.get(ancestor).filter(|parent| !parent.is_empty()) {
            if !visited.insert(parent.clone()) {
                return Err(EventError::Invalid(
                    "Historical agent lineage contains a cycle.".into(),
                ));
            }
            for (key, totals) in agents[agent].capture().own.models {
                agents[parent].add_descendant(&AgentUsageIncrement {
                    key,
                    totals,
                    context_size: None,
                    context_limit: None,
                })?;
            }
            ancestor = parent;
        }
    }
    Ok(AgentStatisticsReplay {
        revision: facts.last().map_or(0, |(revision, _)| *revision),
        agents: agents
            .into_iter()
            .map(|(agent, statistics)| (agent, statistics.capture()))
            .collect(),
        parents,
        incomplete_legacy_tool_counts,
    })
}

pub fn ensure_usage_projection(tx: &Transaction<'_>) -> Result<(), EventError> {
    let current: Option<i64> = tx
        .query_row(
            "SELECT version FROM projection_version WHERE name = 'agent-usage';",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if current.is_some_and(|version| version >= 1) {
        return Ok(());
    }
    let mut latest = HashMap::new();
    let mut order = Vec::new();
    let mut read =
        tx.prepare("SELECT sequence, agent_session, payload FROM event ORDER BY sequence;")?;
    let mut rows = read.query([])?;
    while let Some(row) = rows.next()? {
        let event = Event::parse_from_bytes(&row.get::<_, Vec<u8>>(2)?)?;
        if event.has_agent_statistics_updated() {
            let agent: String = row.get(1)?;
            if !latest.contains_key(&agent) {
                order.push(agent.clone());
            }
            latest.insert(
                agent,
                (
                    row.get::<_, i64>(0)?,
                    event.agent_statistics_updated().clone(),
                ),
            );
        }
    }
    tx.execute("DELETE FROM agent_usage;", [])?;
    for agent in order {
        let (revision, value) = &latest[&agent];
        project_usage(tx, &agent, *revision, value)?;
    }
    tx.execute(
        "INSERT OR REPLACE INTO projection_version (name, version) VALUES ('agent-usage', 1);",
        [],
    )?;
    Ok(())
}

pub fn project_usage(
    tx: &Transaction<'_>,
    agent: &str,
    revision: i64,
    value: &AgentStatisticsUpdatedEvent,
) -> Result<(), EventError> {
    tx.execute("INSERT INTO agent_usage (agent_session, revision, input_tokens, cached_input_tokens, output_tokens, context_size, context_limit, input_cost, output_cost) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) ON CONFLICT(agent_session) DO UPDATE SET revision = excluded.revision, input_tokens = excluded.input_tokens, cached_input_tokens = excluded.cached_input_tokens, output_tokens = excluded.output_tokens, context_size = excluded.context_size, context_limit = excluded.context_limit, input_cost = excluded.input_cost, output_cost = excluded.output_cost;", params![agent, revision, value.input_tokens, value.cached_input_tokens, value.output_tokens, value.context_size, value.context_limit, value.input_cost, value.output_cost])?;
    Ok(())
}

pub fn capture_usage(tx: &Transaction<'_>) -> Result<SessionUsage, EventError> {
    let main: Option<String> = tx
        .query_row(
            "SELECT agent_session FROM session_state LIMIT 1;",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let mut usage = SessionUsage::default();
    let mut revision = 0_i64;
    let mut read = tx.prepare("SELECT agent_session, revision, input_tokens, cached_input_tokens, output_tokens, context_size, context_limit, input_cost, output_cost FROM agent_usage;")?;
    let mut rows = read.query([])?;
    while let Some(row) = rows.next()? {
        revision = revision.max(row.get(1)?);
        usage.input_tokens = usage
            .input_tokens
            .checked_add(row.get(2)?)
            .ok_or(EventError::Overflow)?;
        usage.cached_input_tokens = usage
            .cached_input_tokens
            .checked_add(row.get(3)?)
            .ok_or(EventError::Overflow)?;
        usage.output_tokens = usage
            .output_tokens
            .checked_add(row.get(4)?)
            .ok_or(EventError::Overflow)?;
        usage.input_cost += row.get::<_, f64>(7)?;
        usage.output_cost += row.get::<_, f64>(8)?;
        if main.as_deref() == Some(row.get::<_, String>(0)?.as_str()) {
            usage.context_size = row.get(5)?;
            usage.context_limit = row.get(6)?;
        }
    }
    if revision == 0 {
        return Ok(SessionUsage::default());
    }
    usage.revision = u64::try_from(revision).map_err(|_| EventError::Overflow)?;
    Ok(usage)
}
