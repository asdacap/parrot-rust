//! Durable usage facts and source replay.
//! All functions execute inside the caller's whole repository transaction.

use super::{EventError, EventLoadProgress};
use crate::diagnostics::statistics::AgentStatisticsReplay;
use parrot_domain::protocol::messages::parrot::{Event, RequestUsageRecorded};
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
        tx.execute(
            "UPDATE event SET history_anchor = (SELECT COALESCE(MAX(sequence), 0) FROM agent_history WHERE agent_session = ?2) WHERE sequence = ?1;",
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

fn read_statistics_events(
    tx: &Transaction<'_>,
    progress: Option<&EventLoadProgress>,
) -> Result<Vec<(i64, Event)>, EventError> {
    let mut read = tx.prepare("SELECT sequence, payload FROM event ORDER BY sequence;")?;
    let mut rows = read.query([])?;
    let mut events = Vec::new();
    while let Some(row) = rows.next()? {
        events.push((
            row.get(0)?,
            Event::parse_from_bytes(&row.get::<_, Vec<u8>>(1)?)?,
        ));
        if let Some(progress) = progress {
            progress(events.len());
        }
    }
    if events.is_empty() {
        if let Some(progress) = progress {
            progress(0);
        }
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
    replay_statistics_with_progress(tx, None)
}

pub fn replay_statistics_with_progress(
    tx: &Transaction<'_>,
    progress: Option<EventLoadProgress>,
) -> Result<AgentStatisticsReplay, EventError> {
    let facts = read_statistics_events(tx, progress.as_ref())?;
    let mut parents = HashMap::new();
    let mut parent_order = Vec::new();
    let mut agents = HashMap::new();
    // .NET dictionaries enumerate in insertion order. Preserve that order for
    // floating-point accumulation and source-first validation diagnostics.
    let mut agent_order = Vec::new();
    for (_, event) in &facts {
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
    }
    let mut requests = HashSet::new();
    let mut tools = HashSet::new();
    for (_, event) in &facts {
        let agent = &event.agent_session_id;
        if event.has_request_usage_recorded() {
            let request = event.request_usage_recorded();
            if requests.insert((agent.clone(), request.request_id.clone())) {
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
