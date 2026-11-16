use super::{PromptResult, SystemPrompt, SystemPromptProvider};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use parrot_provider::routing::aliases::ResolvedModelSelection;
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Clone, Debug, Default)]
pub struct AgentIdentityDescription {
    pub session_id: String,
    pub parent_session_id: String,
    pub parent_session_name: String,
    pub agent_name: String,
    pub depth: i32,
    pub scope_text: String,
}

/// This enum describes source rule spelling only. It grants no access.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuleActionDescription {
    AllowWrite,
    DenyRead,
    AllowRead,
    DenyWrite,
}
impl RuleActionDescription {
    fn as_str(self) -> &'static str {
        match self {
            Self::AllowWrite => "AllowWrite",
            Self::DenyRead => "DenyRead",
            Self::AllowRead => "AllowRead",
            Self::DenyWrite => "DenyWrite",
        }
    }
}
#[derive(Clone, Debug)]
pub struct SecurityRuleDescription {
    pub path: String,
    pub action: RuleActionDescription,
}
/// Captures literal template arguments without expanding the template.
pub struct TemplateSystemPromptProvider {
    key: String,
    template: String,
    arguments: Vec<(String, Value)>,
    templates: Arc<PromptTemplateCatalog>,
}
impl TemplateSystemPromptProvider {
    pub fn new(
        key: impl Into<String>,
        template: impl Into<String>,
        arguments: Vec<(String, Value)>,
        templates: Arc<PromptTemplateCatalog>,
    ) -> Self {
        Self {
            key: key.into(),
            template: template.into(),
            arguments,
            templates,
        }
    }
}
impl SystemPromptProvider for TemplateSystemPromptProvider {
    fn key(&self) -> &str {
        &self.key
    }
    fn materialize(&self, _: &AgentIdentityDescription) -> PromptResult<Box<dyn SystemPrompt>> {
        Ok(Box::new(TemplateSystemPrompt {
            key: self.key.clone(),
            template: self.template.clone(),
            arguments: self.arguments.clone(),
            templates: self.templates.clone(),
        }))
    }
}
struct TemplateSystemPrompt {
    key: String,
    template: String,
    arguments: Vec<(String, Value)>,
    templates: Arc<PromptTemplateCatalog>,
}
impl SystemPrompt for TemplateSystemPrompt {
    fn renew_epoch(&mut self) {}
    fn read_system_prompts(
        &mut self,
        _selection: &ResolvedModelSelection,
    ) -> PromptResult<Vec<(String, String)>> {
        Ok(vec![(
            self.key.clone(),
            super::render(&self.templates, &self.template, &self.arguments)?,
        )])
    }
}

pub struct SecurityProfilePromptProvider {
    rules: Vec<SecurityRuleDescription>,
    sandbox_enabled: Arc<dyn Fn() -> bool + Send + Sync>,
    templates: Arc<PromptTemplateCatalog>,
}
impl SecurityProfilePromptProvider {
    pub fn new(
        rules: Vec<SecurityRuleDescription>,
        sandbox_enabled: Arc<dyn Fn() -> bool + Send + Sync>,
        templates: Arc<PromptTemplateCatalog>,
    ) -> Self {
        Self {
            rules,
            sandbox_enabled,
            templates,
        }
    }
}
impl SystemPromptProvider for SecurityProfilePromptProvider {
    fn key(&self) -> &str {
        "runtime:system-context:16-security-profile"
    }
    fn materialize(&self, _: &AgentIdentityDescription) -> PromptResult<Box<dyn SystemPrompt>> {
        let rules = self
            .rules
            .iter()
            .map(|rule| json!({"path": escape_rule_path(&rule.path), "action": rule.action.as_str()}))
            .collect::<Vec<_>>();
        Ok(Box::new(TemplateSystemPrompt {
            key: self.key().into(),
            template: "context.security-profile".into(),
            templates: self.templates.clone(),
            arguments: vec![
                ("rules".into(), rules.into()),
                ("sandbox_enabled".into(), (self.sandbox_enabled)().into()),
            ],
        }))
    }
}
pub struct SessionIdentityPromptProvider(Arc<PromptTemplateCatalog>);
impl SessionIdentityPromptProvider {
    pub fn new(templates: Arc<PromptTemplateCatalog>) -> Self {
        Self(templates)
    }
}
impl SystemPromptProvider for SessionIdentityPromptProvider {
    fn key(&self) -> &str {
        "runtime:system-context:14-session-identity"
    }
    fn materialize(
        &self,
        identity: &AgentIdentityDescription,
    ) -> PromptResult<Box<dyn SystemPrompt>> {
        Ok(Box::new(SessionIdentityPrompt {
            identity: identity.clone(),
            templates: self.0.clone(),
        }))
    }
}

struct SessionIdentityPrompt {
    identity: AgentIdentityDescription,
    templates: Arc<PromptTemplateCatalog>,
}
impl SystemPrompt for SessionIdentityPrompt {
    fn renew_epoch(&mut self) {}
    fn read_system_prompts(
        &mut self,
        _selection: &ResolvedModelSelection,
    ) -> PromptResult<Vec<(String, String)>> {
        Ok(vec![super::render_agent_identity(
            &self.identity,
            &self.templates,
        )?])
    }
}

fn escape_rule_path(value: &str) -> String {
    let mut escaped = String::new();
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\u{0008}' => escaped.push_str("\\b"),
            '\u{000c}' => escaped.push_str("\\f"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            character if character.is_control() || matches!(character, '\u{2028}' | '\u{2029}') => {
                use std::fmt::Write;
                // Source char iteration emits each control as lower-case UTF-16 hex.
                for unit in character.encode_utf16(&mut [0; 2]) {
                    let _ = write!(escaped, "\\u{unit:04x}");
                }
            }
            character => escaped.push(character),
        }
    }
    escaped
}
