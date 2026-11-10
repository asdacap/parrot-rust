use super::{PromptError, PromptResult};
use parrot_core::{AgentIdentityDescription, ModelAliasDescription, SkillCatalogEntry};
use parrot_foundation::template_catalog::PromptTemplateCatalog;

pub fn render_agents_files(
    files: &[(String, String)],
    templates: &PromptTemplateCatalog,
) -> PromptResult<(String, String)> {
    let template = templates
        .get_template("context.agents-file")
        .map_err(|error| PromptError::Template(error.to_string()))?;
    parrot_core::rendering::render_agents_files(files, &template).map_err(PromptError::from)
}

pub fn render_model_prompt(
    aliases: &[ModelAliasDescription],
    guidance: &(Option<String>, Option<String>),
    templates: &PromptTemplateCatalog,
) -> PromptResult<(String, String)> {
    let template = templates
        .get_template("context.model-aliases")
        .map_err(|error| PromptError::Template(error.to_string()))?;
    parrot_core::rendering::render_model_prompt(aliases, guidance, &template)
        .map_err(PromptError::from)
}

pub fn render_agent_identity(
    identity: &AgentIdentityDescription,
    templates: &PromptTemplateCatalog,
) -> PromptResult<(String, String)> {
    let template = templates
        .get_template("system.agent-identity")
        .map_err(|error| PromptError::Template(error.to_string()))?;
    parrot_core::rendering::render_agent_identity(identity, &template).map_err(PromptError::from)
}

pub fn render_standing_instruction(
    instruction: &str,
    templates: &PromptTemplateCatalog,
) -> PromptResult<(String, String)> {
    let template = templates
        .get_template("context.standing-instruction")
        .map_err(|error| PromptError::Template(error.to_string()))?;
    parrot_core::rendering::render_standing_instruction(instruction, &template)
        .map_err(PromptError::from)
}

pub fn render_skill_catalog_entries(
    skills: &[SkillCatalogEntry],
    templates: &PromptTemplateCatalog,
) -> PromptResult<(String, String)> {
    let template = templates
        .get_template("context.skills")
        .map_err(|error| PromptError::Template(error.to_string()))?;
    parrot_core::rendering::render_skill_catalog_entries(skills, &template)
        .map_err(PromptError::from)
}

pub use parrot_core::rendering::render_system_prompt_sections;
