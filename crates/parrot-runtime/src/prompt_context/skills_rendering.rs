use super::{PromptError, PromptResult, SkillMetadata, render};
use parrot_foundation::template_catalog::PromptTemplateCatalog;

pub const MAXIMUM_CATALOG_BYTES: usize = 64 * 1024;

pub fn render_skill_catalog(
    skills: &[SkillMetadata],
    templates: &PromptTemplateCatalog,
) -> PromptResult<String> {
    let available: Vec<_> = skills
        .iter()
        .filter(|skill| skill.enabled && skill.prompt_visible)
        .collect();
    if available.is_empty() {
        return Ok(String::new());
    }
    let wrapper = render(
        templates,
        "context.skills",
        &[("skills".into(), "{skills}".into())],
    )?;
    let budget = (MAXIMUM_CATALOG_BYTES + "{skills}".len())
        .checked_sub(wrapper.len())
        .filter(|budget| *budget > 0)
        .ok_or_else(|| {
            PromptError::Context(
                "The skills prompt template exceeds the 64 KiB catalogue limit.".into(),
            )
        })?;
    let mut entries: Vec<String> = Vec::new();
    let mut consumed = 0;
    let mut omitted = 0;
    for skill in available {
        let entry = format!(
            "- ${}: {} ({})\n",
            skill.name, skill.description, skill.discovery_path
        );
        if consumed + entry.len() > budget {
            omitted += 1;
        } else {
            consumed += entry.len();
            entries.push(entry);
        }
    }
    if omitted > 0 {
        let mut diagnostic = omitted_catalog(omitted);
        while !entries.is_empty() && consumed + diagnostic.len() > budget {
            consumed -= entries.pop().unwrap().len();
            omitted += 1;
            diagnostic = omitted_catalog(omitted);
        }
        entries.push(diagnostic);
    }
    render(
        templates,
        "context.skills",
        &[("skills".into(), entries.concat().trim_end().into())],
    )
}

fn omitted_catalog(count: usize) -> String {
    format!(
        "- {count} additional skill(s) omitted because the catalogue reached its 64 KiB limit.\n"
    )
}
