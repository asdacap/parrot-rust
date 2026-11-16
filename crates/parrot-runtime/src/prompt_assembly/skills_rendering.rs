use super::{PromptResult, SkillMetadata};
use parrot_foundation::template_catalog::PromptTemplateCatalog;

pub use super::rendering::MAXIMUM_CATALOG_BYTES;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SkillCatalogEntry {
    pub name: String,
    pub description: String,
    pub discovery_path: String,
    pub enabled: bool,
    pub prompt_visible: bool,
}

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
    super::rendering::render_catalog_entries(&entries, templates)
}
