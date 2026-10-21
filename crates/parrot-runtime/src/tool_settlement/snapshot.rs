use super::{ToolHandle, ToolInventory};
use crate::agent_turn::services::{TurnError, TurnProfile};
use parrot_domain::contracts::security::AccessSnapshot;
use parrot_provider::sessions;
use std::collections::HashSet;

/// A documented inventory retaining the original tool Arcs through every filter.
pub struct ToolSnapshot<S: AccessSnapshot + 'static> {
    entries: Vec<ToolHandle<S>>,
    definitions: Vec<sessions::Tool>,
}
impl<S: AccessSnapshot + 'static> Clone for ToolSnapshot<S> {
    fn clone(&self) -> Self {
        Self {
            entries: self.entries.clone(),
            definitions: self.definitions.clone(),
        }
    }
}
impl<S: AccessSnapshot + 'static> ToolSnapshot<S> {
    pub fn document(inventory: ToolInventory<S>) -> Result<Self, TurnError> {
        if inventory.tools.len() != inventory.supported.len()
            || inventory.tools.len() != inventory.definitions.len()
        {
            return Err(TurnError::Service(
                "Tool support flags and definitions must match the tool inventory.".into(),
            ));
        }
        let mut names = HashSet::new();
        // Unsupported entries still participate in global name/alias validation.
        for tool in &inventory.tools {
            let metadata = tool.definition();
            for name in std::iter::once(&metadata.name).chain(&metadata.aliases) {
                if !names.insert(name.clone()) {
                    return Err(TurnError::Service(format!(
                        "tool '{name}' is registered more than once"
                    )));
                }
            }
        }
        let mut entries = Vec::new();
        let mut definitions = Vec::new();
        for ((tool, supported), definition) in inventory
            .tools
            .into_iter()
            .zip(inventory.supported)
            .zip(inventory.definitions)
        {
            if supported {
                definitions.push(sessions::Tool {
                    name: tool.definition().name.clone(),
                    description: definition.description,
                    parameters_json: definition.parameters_json,
                });
                entries.push(tool);
            }
        }
        Ok(Self {
            entries,
            definitions,
        })
    }
    pub fn find(&self, name: &str) -> Option<&ToolHandle<S>> {
        self.entries.iter().find(|tool| {
            let metadata = tool.definition();
            metadata.name == name || metadata.aliases.iter().any(|alias| alias == name)
        })
    }
    pub fn definitions(&self) -> &[sessions::Tool] {
        &self.definitions
    }
    pub fn entries(&self) -> &[ToolHandle<S>] {
        &self.entries
    }
    pub fn permitted_by(&self, profile: &TurnProfile) -> Self {
        self.filter(|tool| profile.is_tool_permitted(&tool.definition().name))
    }
    pub fn enabled_after_interruption(&self) -> Self {
        self.filter(ToolHandle::enabled_after_interruption)
    }
    fn filter(&self, predicate: impl Fn(&ToolHandle<S>) -> bool) -> Self {
        let mut entries = Vec::new();
        let mut definitions = Vec::new();
        for (tool, definition) in self.entries.iter().zip(&self.definitions) {
            if predicate(tool) {
                entries.push(tool.clone());
                definitions.push(definition.clone());
            }
        }
        Self {
            entries,
            definitions,
        }
    }
}
