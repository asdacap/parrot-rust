use super::{PromptResult, render, skills_rendering::render_catalog_entries};
use parrot_core::{AgentIdentityDescription, ModelAliasDescription, SkillCatalogEntry};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use serde_json::json;

pub fn render_agents_files(
    files: &[(String, String)],
    templates: &PromptTemplateCatalog,
) -> PromptResult<(String, String)> {
    let mut sections = Vec::with_capacity(files.len());
    for (path, content) in files {
        sections.push(render(
            templates,
            "context.agents-file",
            &[
                ("path".into(), path.clone().into()),
                ("content".into(), content.clone().into()),
            ],
        )?);
    }
    Ok((
        "runtime:system-context:07a-agents".into(),
        sections.join("\n\n"),
    ))
}

pub fn render_model_prompt(
    aliases: &[ModelAliasDescription],
    guidance: &(Option<String>, Option<String>),
    templates: &PromptTemplateCatalog,
) -> PromptResult<(String, String)> {
    let aliases = aliases
        .iter()
        .filter(|alias| !alias.model.is_empty())
        .map(|alias| {
            let usage = match alias
                .model_usage
                .as_deref()
                .filter(|usage| !usage.is_empty())
            {
                None => alias.usage.clone(),
                Some(usage) if alias.usage.is_empty() => usage.to_owned(),
                Some(usage) => format!("{}\n{usage}", alias.usage),
            };
            json!({"name": alias.name, "model": alias.model, "usage": usage})
        })
        .collect::<Vec<_>>();
    let alias_section = if aliases.is_empty() {
        String::new()
    } else {
        render(
            templates,
            "context.model-aliases",
            &[("aliases".into(), aliases.into())],
        )?
    };
    let sections = [
        Some(alias_section.as_str()),
        guidance.0.as_deref(),
        guidance.1.as_deref(),
    ];
    Ok((
        "runtime:system-context:10-model-prompt".into(),
        sections
            .into_iter()
            .flatten()
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
    ))
}

pub fn render_agent_identity(
    identity: &AgentIdentityDescription,
    templates: &PromptTemplateCatalog,
) -> PromptResult<(String, String)> {
    let content = if identity.depth == 0 {
        identity.scope_text.clone()
    } else {
        let content = render(
            templates,
            "system.agent-identity",
            &[
                ("session_id".into(), identity.session_id.clone().into()),
                (
                    "parent_session_id".into(),
                    identity.parent_session_id.clone().into(),
                ),
                (
                    "parent_session_name".into(),
                    identity.parent_session_name.clone().into(),
                ),
                ("agent_name".into(), identity.agent_name.clone().into()),
                ("depth".into(), identity.depth.to_string().into()),
            ],
        )?;
        if identity.scope_text.is_empty() {
            content
        } else {
            format!("{content}\n\n{}", identity.scope_text)
        }
    };
    Ok(("runtime:system-context:14-session-identity".into(), content))
}

pub fn render_standing_instruction(
    instruction: &str,
    templates: &PromptTemplateCatalog,
) -> PromptResult<(String, String)> {
    let content = if instruction.is_empty() {
        String::new()
    } else {
        render(
            templates,
            "context.standing-instruction",
            &[("instruction".into(), instruction.into())],
        )?
    };
    Ok((
        "runtime:system-context:94-standing-instruction".into(),
        content,
    ))
}

pub fn render_skill_catalog_entries(
    skills: &[SkillCatalogEntry],
    templates: &PromptTemplateCatalog,
) -> PromptResult<(String, String)> {
    Ok((
        "runtime:system-context:09b-skills".into(),
        render_catalog_entries(skills, templates)?,
    ))
}

pub fn render_system_prompt_sections(mut sections: Vec<(String, String)>) -> String {
    sections.sort_by(|(left, _), (right, _)| left.encode_utf16().cmp(right.encode_utf16()));
    sections
        .into_iter()
        .map(|(_, content)| content)
        .filter(|content| !content.chars().all(char::is_whitespace))
        .collect::<Vec<_>>()
        .join("\n\n")
}
