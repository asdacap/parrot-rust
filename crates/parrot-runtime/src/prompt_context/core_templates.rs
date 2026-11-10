use super::{PromptError, PromptResult};
use parrot_core::ConfigKey;

const PROMPT_TEMPLATE_CONFIG_KEYS: &[(ConfigKey, &str)] = &[
    (ConfigKey::AgentsFileTemplate, "context.agents-file"),
    (ConfigKey::ModelAliasesTemplate, "context.model-aliases"),
    (ConfigKey::AgentIdentityTemplate, "system.agent-identity"),
    (
        ConfigKey::StandingInstructionTemplate,
        "context.standing-instruction",
    ),
    (ConfigKey::SkillCatalogTemplate, "context.skills"),
    (
        ConfigKey::SkillUnavailableTemplate,
        "context.skill-unavailable",
    ),
    (ConfigKey::SkillSelectedTemplate, "context.skill-selected"),
    (ConfigKey::SkillTooLargeTemplate, "context.skill-too-large"),
    (
        ConfigKey::ContextReminderTemplate,
        "agent-session.context-reminder",
    ),
    (
        ConfigKey::ContextLimitReminderTemplate,
        "agent-session.context-limit-reminder",
    ),
    (
        ConfigKey::FinalRequestTemplate,
        "agent-session.final-request",
    ),
    (
        ConfigKey::TruncatedToolCallTemplate,
        "agent-session.truncated-tool-call",
    ),
    (
        ConfigKey::InvalidToolCallTemplate,
        "agent-session.invalid-tool-call",
    ),
    (
        ConfigKey::OptionalCliUtilitiesTemplate,
        "context.optional-cli-utilities",
    ),
    (
        ConfigKey::ExpectedCliUtilitiesTemplate,
        "context.expected-cli-utilities",
    ),
    (ConfigKey::PlatformTemplate, "context.platform"),
    (
        ConfigKey::WorkingDirectoryTemplate,
        "system.working-directory",
    ),
    (ConfigKey::GitRepositoryTemplate, "context.git-repository"),
    (ConfigKey::SubagentsTemplate, "context.subagents"),
    (
        ConfigKey::SecurityProfileTemplate,
        "context.security-profile",
    ),
    (
        ConfigKey::AgentPathEnvironmentTemplate,
        "context.agent-path-environment",
    ),
    (ConfigKey::AgentScratchTemplate, "context.agent-scratch"),
    (ConfigKey::AgentHistoryTemplate, "context.agent-history"),
];

pub(crate) fn get_prompt_template_id(key: ConfigKey) -> Option<&'static str> {
    PROMPT_TEMPLATE_CONFIG_KEYS
        .iter()
        .find_map(|(candidate, id)| (*candidate == key).then_some(*id))
}

pub(crate) fn resolve_prompt_template_key(id: &str) -> PromptResult<ConfigKey> {
    PROMPT_TEMPLATE_CONFIG_KEYS
        .iter()
        .find_map(|(key, candidate)| (*candidate == id).then_some(*key))
        .ok_or_else(|| {
            PromptError::Template(format!(
                "prompt_templates.{id} is not an enumerated core prompt template"
            ))
        })
}
