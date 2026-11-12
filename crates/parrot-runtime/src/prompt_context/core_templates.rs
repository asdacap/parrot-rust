use parrot_core::ConfigKey;

const PROMPT_TEMPLATE_CONFIG_KEYS: &[(ConfigKey, &str)] = &[
    (
        ConfigKey::OversizedToolGroupTemplate,
        "compaction.oversized-tool-group-notice",
    ),
    (
        ConfigKey::SkillUnavailableTemplate,
        "context.skill-unavailable",
    ),
    (ConfigKey::SkillSelectedTemplate, "context.skill-selected"),
    (ConfigKey::SkillTooLargeTemplate, "context.skill-too-large"),
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
    (ConfigKey::RunawayTemplate, "agent-session.runaway"),
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
