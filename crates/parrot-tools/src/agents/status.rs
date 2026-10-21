use std::collections::HashMap;
use std::fmt::Write;
use std::sync::Mutex;
use std::time::Duration;

use parrot_domain::values::activity::{
    AgentExecutionStatus, AgentSessionActivityEntryKind, AgentSessionActivitySnapshot, DrainState,
};
use parrot_domain::values::context::TokenEstimator;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusChild {
    pub session_id: String,
    pub parent_session_id: String,
    pub name: String,
    pub active: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusProcess {
    pub name: String,
    pub id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusSnapshot {
    pub session_id: String,
    pub name: String,
    pub output_path: String,
    pub activity: AgentSessionActivitySnapshot,
    pub children: Vec<StatusChild>,
    pub processes: Vec<StatusProcess>,
}

#[derive(Default)]
pub struct StatusFormatter {
    reported_through: Mutex<HashMap<String, i64>>,
}

impl StatusFormatter {
    pub fn format(&self, snapshot: &StatusSnapshot) -> String {
        let mut reported = self
            .reported_through
            .lock()
            .expect("status history lock poisoned");
        let reported_through = reported
            .get(&snapshot.session_id)
            .copied()
            .unwrap_or_default();
        if let Some(last) = snapshot.activity.recent.last() {
            reported.insert(snapshot.session_id.clone(), last.sequence);
        }
        drop(reported);

        let activity = &snapshot.activity;
        let mut report = format!(
            "Agent status\nName: {}\nLive output file: {}\nLifecycle: {}\nRequest session duration: {}",
            snapshot.name,
            snapshot.output_path,
            match activity.state {
                DrainState::Idle => "idle",
                DrainState::Running => "running",
                DrainState::Interrupting => "interrupting",
            },
            activity
                .request_session_duration
                .map(format_duration)
                .unwrap_or_else(|| "none".into())
        );
        if let Some(duration) = activity.current_provider_request_duration {
            write!(
                report,
                "\nCurrent provider request duration: {}",
                format_duration(duration)
            )
            .expect("string write");
        } else if let Some(duration) = activity.last_provider_request_duration {
            write!(
                report,
                "\nLast provider request duration: {}",
                format_duration(duration)
            )
            .expect("string write");
        } else {
            report.push_str("\nProvider request duration: none");
        }
        if let Some(tool) = &activity.current_tool {
            write!(report, "\nCurrent tool: {tool}").expect("string write");
        } else if let Some(age) = activity.latest_provider_activity_age {
            write!(
                report,
                "\nLatest provider activity: {} ago",
                format_duration(age)
            )
            .expect("string write");
        } else {
            report.push_str("\nCurrent activity: none");
        }
        if let Some(outcome) = &activity.terminal_outcome {
            let status = match outcome.status {
                AgentExecutionStatus::Succeeded => "succeeded",
                AgentExecutionStatus::Failed => "failed",
                AgentExecutionStatus::Canceled => "canceled",
            };
            write!(report, "\nLast outcome: {status}").expect("string write");
        }
        report.push_str("\nActive direct subagents:");
        let mut children: Vec<_> = snapshot
            .children
            .iter()
            .filter(|child| child.active && child.parent_session_id == snapshot.session_id)
            .collect();
        children.sort_by(|left, right| left.session_id.cmp(&right.session_id));
        if children.is_empty() {
            report.push_str(" none");
        } else {
            for child in children {
                write!(report, "\n- {}", child.name).expect("string write");
            }
        }
        report.push_str("\nActive processes:");
        if snapshot.processes.is_empty() {
            report.push_str(" none");
        } else {
            for process in &snapshot.processes {
                write!(
                    report,
                    "\n- {} ({}, shell, running)",
                    process.name, process.id
                )
                .expect("string write");
            }
        }
        report.push_str("\nRecent entries:");
        if activity.recent.is_empty() {
            report.push_str(" none");
            return report;
        }
        let already_reported = activity
            .recent
            .iter()
            .filter(|entry| entry.sequence <= reported_through)
            .count();
        let skipped = already_reported.saturating_sub(1);
        if skipped > 0 {
            write!(
                report,
                "\n- {skipped} earlier entries already reported by a previous agent_status"
            )
            .expect("string write");
        }
        let entries: Vec<String> = activity
            .recent
            .iter()
            .skip(skipped)
            .map(|entry| {
                let kind = match entry.kind {
                    AgentSessionActivityEntryKind::ReasoningSummary => "reasoning summary",
                    AgentSessionActivityEntryKind::AssistantMessage => "assistant message",
                };
                format!(
                    "\n- {kind} ({} ago): {}",
                    format_duration(entry.age),
                    entry.content.replace('\n', "\n  ")
                )
            })
            .collect();
        let mut tokens = 0;
        let mut included = 0;
        for entry in entries.iter().rev() {
            tokens += TokenEstimator::estimate_tokens(entry);
            if tokens > 1000 {
                break;
            }
            included += 1;
        }
        if included < entries.len() {
            write!(report, "\n- {} older entries omitted to stay within 1K tokens; read the live output file for the full output", entries.len() - included).expect("string write");
        }
        for entry in &entries[entries.len() - included..] {
            report.push_str(entry);
        }
        report
    }
}

fn format_duration(duration: Duration) -> String {
    if duration.as_secs_f64() >= 1.0 {
        format!("{:.1}s", duration.as_secs_f64())
    } else {
        format!("{:.0}ms", duration.as_secs_f64() * 1000.0)
    }
}
