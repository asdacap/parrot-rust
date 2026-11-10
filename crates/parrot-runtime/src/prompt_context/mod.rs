//! Runtime prompt, AGENTS and skills orchestration through injected I/O services.
//! Service reports and prompt descriptions are not filesystem authorization.
mod agents;
pub use agents::AgentsPromptProvider;
pub(crate) use agents::decode_agents_text as decode_source_text;
mod composition;
mod core_templates;
pub(crate) use core_templates::get_prompt_template_id;
mod rendering;
mod session;
pub(crate) use rendering::render_skill_catalog_entries;
pub use rendering::{
    render_agent_identity, render_agents_files, render_standing_instruction,
    render_system_prompt_sections,
};
pub use session::SessionPrompt;
mod environment;

pub use environment::{
    AgentPathEnvironment, CliUtilityAvailability, git_repository_prompt, native_platform_identifier,
};
mod identity_scope;
pub use identity_scope::{PromptScope, StandingInstruction};
mod model;
mod packaged;
pub use packaged::{
    PACKAGED_SKILL_ASSETS, PackagedSkillAsset, PackagedSkillsError, extract_packaged_skills,
    extract_packaged_skills_sync, hash_packaged_skills,
};
mod sections;
mod skills_discovery;
pub use skills_discovery::{
    MAXIMUM_SKILL_FILE_BYTES, SkillCatalog, SkillConfigurationSource, SkillConfigurationWriter,
    SkillDiscoveryContext, SkillLoadError, SkillSnapshot, discover_skills, read_skill_file,
    resolve_skill_roots,
};
mod agent_skills;
pub use agent_skills::{AgentSkillPromptProvider, AgentSkills};
mod skills_rendering;
pub(crate) use skills_rendering::SkillCatalogEntry;
pub use skills_rendering::{MAXIMUM_CATALOG_BYTES, render_skill_catalog};
mod skills_parsing;
pub use parrot_core::{SkillMention, parse_skill_mentions};
pub use skills_parsing::{
    SkillDisplayMetadata, SkillMetadata, SkillParseError, SkillRoot, SkillRootLocation, SkillScope,
    normalize_skill_match_path, normalize_skill_path, parse_skill_display_metadata,
    parse_skill_frontmatter,
};

pub use composition::{
    CompositeSystemPrompt, CompositeSystemPromptProvider, PromptError, PromptResult,
    StaticSystemPrompt, StaticSystemPromptProvider, SystemPrompt, SystemPromptProvider,
    is_valid_provider_key,
};
pub use model::ModelPromptProvider;
pub use sections::{
    AgentIdentityDescription, MaterializedContext, RuleActionDescription, SecurityDescription,
    SecurityProfilePromptProvider, SecurityRuleDescription, SessionIdentityPromptProvider,
    SubagentDescription, TemplateSystemPromptProvider, materialized_context_providers,
};

use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use serde_json::Value;

struct Uncancelled;
impl Cancellation for Uncancelled {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}

fn render(
    catalog: &PromptTemplateCatalog,
    id: &str,
    arguments: &[(String, Value)],
) -> PromptResult<String> {
    // These source providers render with CancellationToken.None. This token is
    // not a filesystem/security capability and no loader is called here.
    catalog
        .render(id, arguments, &Uncancelled)
        .map_err(|error| PromptError::Template(error.to_string()))
}
