use super::{AgentIdentityDescription, PromptResult, SystemPrompt, SystemPromptProvider, render};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use std::sync::{Arc, RwLock};

#[derive(Clone, Debug, Eq, PartialEq)]
struct ScopeChange {
    name: String,
    depth: i32,
    content: String,
}

/// Ordered prompt scope history, not a policy lineage or resource lookup scope.
#[derive(Clone)]
pub struct PromptScope {
    changes: Vec<ScopeChange>,
    templates: Arc<PromptTemplateCatalog>,
}
impl PromptScope {
    pub fn empty(templates: Arc<PromptTemplateCatalog>) -> Self {
        Self {
            changes: Vec::new(),
            templates,
        }
    }
    pub fn derive_child(&self, name: &str, depth: i32, requested_scope: &str) -> Self {
        if requested_scope.is_empty()
            || self
                .changes
                .last()
                .is_some_and(|change| change.content == requested_scope)
        {
            return self.clone();
        }
        let mut child = self.clone();
        child.changes.push(ScopeChange {
            name: name.into(),
            depth,
            content: requested_scope.into(),
        });
        child
    }
    pub fn format(&self, current_depth: i32) -> PromptResult<String> {
        let mut text = String::new();
        for (index, change) in self.changes.iter().enumerate() {
            let distance = current_depth.wrapping_sub(change.depth);
            let label = if distance == 0 {
                "Self".into()
            } else {
                format!("{} Ancestor ({})", ordinal(distance), change.name)
            };
            text.push_str(&render(
                &self.templates,
                "system.agent-scope",
                &[
                    (
                        "heading".into(),
                        if index == 0 { "## Scope" } else { "" }.into(),
                    ),
                    ("label".into(), label.into()),
                    ("content".into(), change.content.clone().into()),
                ],
            )?);
        }
        Ok(text)
    }
    pub fn apply(
        &self,
        mut identity: AgentIdentityDescription,
    ) -> PromptResult<AgentIdentityDescription> {
        identity.scope_text = self.format(identity.depth)?;
        Ok(identity)
    }
}
fn ordinal(value: i32) -> String {
    let suffix = match value % 100 {
        11..=13 => "th",
        _ => match value % 10 {
            1 => "st",
            2 => "nd",
            3 => "rd",
            _ => "th",
        },
    };
    format!("{value}{suffix}")
}

/// A session's live instruction is shared by all materializations, matching
/// source Volatile.Read/Write rather than capturing a stale instruction string.
#[derive(Clone)]
pub struct StandingInstruction {
    instruction: Arc<RwLock<String>>,
    templates: Arc<PromptTemplateCatalog>,
}
impl StandingInstruction {
    pub fn new(templates: Arc<PromptTemplateCatalog>) -> Self {
        Self {
            instruction: Arc::new(RwLock::new(String::new())),
            templates,
        }
    }
    pub fn update(&self, instruction: impl Into<String>) {
        *self
            .instruction
            .write()
            .expect("standing instruction lock poisoned") = instruction.into();
    }
}
impl SystemPrompt for StandingInstruction {
    fn renew_epoch(&mut self) {}
    fn read_standing_instruction(&self) -> String {
        self.instruction
            .read()
            .expect("standing instruction lock poisoned")
            .clone()
    }
    fn get_template_catalog(&self, source_key: &str) -> Option<&PromptTemplateCatalog> {
        (source_key == "runtime:system-context:94-standing-instruction")
            .then_some(self.templates.as_ref())
    }
}
impl SystemPromptProvider for StandingInstruction {
    fn key(&self) -> &str {
        "runtime:system-context:94-standing-instruction"
    }
    fn materialize(&self, _: &AgentIdentityDescription) -> PromptResult<Box<dyn SystemPrompt>> {
        Ok(Box::new(self.clone()))
    }
}
