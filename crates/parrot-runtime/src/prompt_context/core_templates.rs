use parrot_core::ConfigKey;

const PROMPT_TEMPLATE_CONFIG_KEYS: &[(ConfigKey, &str)] = &[
    (ConfigKey::AgentsFileTemplate, "context.agents-file"),
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
        ConfigKey::RestoredToolsTemplate,
        "agent-session.tools-restored",
    ),
    (
        ConfigKey::InterruptedTemplate,
        "agent-session.interrupted-note",
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
        ConfigKey::CrashedToolTemplate,
        "agent-session.crashed-tool-result",
    ),
    (ConfigKey::ToolErrorTemplate, "tool-result.error"),
];

pub(crate) fn get_prompt_template_id(key: ConfigKey) -> Option<&'static str> {
    PROMPT_TEMPLATE_CONFIG_KEYS
        .iter()
        .find_map(|(candidate, id)| (*candidate == key).then_some(*id))
}
