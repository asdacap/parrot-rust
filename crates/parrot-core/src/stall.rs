//! Stall detection over the step fingerprint recorded in the agent state.

use crate::{AgentState, ProviderResponse, ToolResult};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Tool names a stalling run treats as legitimate waiting instead of repetition.
const WAITING_TOOLS: [&str; 2] = ["wait", "monitor"];

/// Records one settled step and extends or breaks the identical-step run.
/// A step qualifies when it promoted no input and called no waiting tool.
pub(crate) fn record_step(
    agent_state: &mut AgentState,
    response: &ProviderResponse,
    results: &[ToolResult],
    steered: bool,
) {
    let fingerprint = fingerprint(response, results);
    let waiting = response
        .tool_calls
        .iter()
        .any(|call| WAITING_TOOLS.contains(&call.name.as_str()));
    let qualifying = !steered && !waiting;
    agent_state.stall_run = if qualifying {
        if agent_state.stall_fingerprint == Some(fingerprint) {
            agent_state.stall_run + 1
        } else {
            1
        }
    } else {
        0
    };
    agent_state.stall_fingerprint = Some(fingerprint);
}

/// Returns the stall index of the recorded run when it exceeds the threshold.
/// The index counts identical steps beyond the first threshold-many.
pub(crate) fn stall_index(agent_state: &AgentState, threshold: i32) -> Option<i32> {
    let run = agent_state.stall_run;
    (run > threshold).then(|| run - threshold)
}

fn fingerprint(response: &ProviderResponse, results: &[ToolResult]) -> u64 {
    let mut hasher = DefaultHasher::new();
    response.assistant_text.hash(&mut hasher);
    for call in &response.tool_calls {
        call.name.hash(&mut hasher);
        call.arguments_json.hash(&mut hasher);
    }
    for result in results {
        result.tool_name.hash(&mut hasher);
        result.status.hash(&mut hasher);
        result.text.hash(&mut hasher);
        result.error_message.hash(&mut hasher);
    }
    hasher.finish()
}
