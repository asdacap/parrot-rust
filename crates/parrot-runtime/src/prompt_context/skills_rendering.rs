use super::{PromptError, PromptResult, SkillMetadata};
use parrot_core::SkillCatalogEntry;
use parrot_foundation::template_catalog::PromptTemplateCatalog;

pub use parrot_core::rendering::MAXIMUM_CATALOG_BYTES;

pub fn render_skill_catalog(
    skills: &[SkillMetadata],
    templates: &PromptTemplateCatalog,
) -> PromptResult<String> {
    let entries = skills
        .iter()
        .map(|skill| SkillCatalogEntry {
            name: skill.name.clone(),
            description: skill.description.clone(),
            discovery_path: skill.discovery_path.clone(),
            enabled: skill.enabled,
            prompt_visible: skill.prompt_visible,
        })
        .collect::<Vec<_>>();
    let template = templates
        .get_template("context.skills")
        .map_err(|error| PromptError::Template(error.to_string()))?;
    parrot_core::rendering::render_catalog_entries(&entries, &template).map_err(PromptError::from)
}
