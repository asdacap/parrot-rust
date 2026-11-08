use std::collections::BTreeSet;

use parrot_domain::{
    protocol::messages::parrot::ToolSnapshot,
    values::progress::{Progress, ProgressSnapshot},
};

use super::{PresenterError, PresenterResult};
use crate::terminal_layout::TerminalText;

#[derive(Default)]
pub(crate) struct ProgressInventory {
    instance: Option<String>,
    retired: BTreeSet<String>,
    revision: Option<u64>,
    removed: bool,
    pub(crate) progress: Option<Progress>,
}
impl ProgressInventory {
    pub(crate) fn observe(&mut self, owner: &str, inventory: &ToolSnapshot) -> PresenterResult<()> {
        if owner.is_empty()
            || inventory.owner_agent_session_id != owner
            || inventory.inventory_instance_id.is_empty()
        {
            return Err(PresenterError(
                "Invalid progress snapshot ownership.".into(),
            ));
        }
        if inventory.chunk_index != 0 || !inventory.final_chunk {
            return Ok(());
        }
        let snapshot: ProgressSnapshot = serde_json::from_slice(&inventory.data)?;
        snapshot
            .validate()
            .map_err(|error| PresenterError(error.to_string()))?;
        if snapshot.owner_session_id != owner || snapshot.revision != inventory.revision {
            return Err(PresenterError(
                "Progress snapshot envelope does not match its state.".into(),
            ));
        }
        if self.retired.contains(&inventory.inventory_instance_id) {
            return Ok(());
        }
        if self.instance.as_deref() == Some(&inventory.inventory_instance_id) {
            if self.removed
                || self.revision.is_some_and(|revision| {
                    inventory.revision < revision
                        || (inventory.revision == revision && !inventory.removed)
                })
            {
                return Ok(());
            }
        } else if let Some(previous) = self
            .instance
            .replace(inventory.inventory_instance_id.clone())
        {
            self.retired.insert(previous);
        }
        self.revision = Some(inventory.revision);
        self.removed = inventory.removed;
        self.progress = if inventory.removed {
            None
        } else {
            snapshot.progress
        };
        Ok(())
    }
}

pub(crate) fn format_progress(progress: &Progress, columns: usize) -> String {
    let count = format!("{}/{}", progress.steps, progress.total_steps);
    let percentage = u128::from(progress.steps) * 100 / u128::from(progress.total_steps);
    let summary = format!("{count} {percentage}%");
    let summary_width = TerminalText::width(&summary);
    if summary_width > columns {
        return TerminalText::clip(&count, columns);
    }
    let available = columns - summary_width;
    let mut text = if available >= 7 {
        let width = (available - 3).min(24);
        let filled = (u128::from(progress.steps) * width as u128 / u128::from(progress.total_steps))
            as usize;
        format!(
            "{count} [{}{}] {percentage}%",
            "█".repeat(filled),
            "░".repeat(width - filled)
        )
    } else {
        summary
    };
    if let Some(label) = &progress.current_step {
        let label = TerminalText::sanitize_line(label);
        let remaining = columns.saturating_sub(TerminalText::width(&text));
        if !label.trim().is_empty() && remaining > 1 {
            text.push(' ');
            text.push_str(&TerminalText::clip(label.trim(), remaining - 1));
        }
    }
    text
}
