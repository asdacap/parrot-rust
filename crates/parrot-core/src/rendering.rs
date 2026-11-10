use crate::{
    AgentIdentityDescription, CancellationToken, ConfigKey, Error, PromptTemplate,
    SkillCatalogEntry, States,
};
use serde_json::Value;

fn render(template: &PromptTemplate, arguments: &[(String, Value)]) -> Result<String, Error> {
    template.render(arguments, CancellationToken::uncancelled())
}

pub(crate) async fn render_template(
    states: &dyn States,
    key: ConfigKey,
    arguments: &[(String, Value)],
) -> Result<String, Error> {
    let template = states.get_config(key).await?.into_prompt_template()?;
    render(&template, arguments)
}

pub(crate) async fn render_instructions(states: &mut dyn States) -> Result<String, Error> {
    let mut sections = states.capture_system_prompt_sections().await?;
    let files = states.read_agents_files().await?;
    if !files.is_empty() {
        let template = states
            .get_config(ConfigKey::AgentsFileTemplate)
            .await?
            .into_prompt_template()?;
        sections.push(render_agents_files(&files, &template)?);
    }
    if let Some(identity) = states.read_agent_identity().await? {
        let template = states
            .get_config(ConfigKey::AgentIdentityTemplate)
            .await?
            .into_prompt_template()?;
        sections.push(render_agent_identity(&identity, &template)?);
    }
    let instruction = states.read_standing_instruction().await?;
    if !instruction.is_empty() {
        let template = states
            .get_config(ConfigKey::StandingInstructionTemplate)
            .await?
            .into_prompt_template()?;
        sections.push(render_standing_instruction(&instruction, &template)?);
    }
    let skills = states.read_skill_catalog().await?;
    if !skills.is_empty() {
        let template = states
            .get_config(ConfigKey::SkillCatalogTemplate)
            .await?
            .into_prompt_template()?;
        sections.push(render_skill_catalog_entries(&skills, &template)?);
    }
    Ok(render_system_prompt_sections(sections))
}

pub fn render_agents_files(
    files: &[(String, String)],
    template: &PromptTemplate,
) -> Result<(String, String), Error> {
    let mut sections = Vec::with_capacity(files.len());
    for (path, content) in files {
        sections.push(render(
            template,
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

pub fn render_agent_identity(
    identity: &AgentIdentityDescription,
    template: &PromptTemplate,
) -> Result<(String, String), Error> {
    let content = if identity.depth == 0 {
        identity.scope_text.clone()
    } else {
        let content = render(
            template,
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
    template: &PromptTemplate,
) -> Result<(String, String), Error> {
    let content = if instruction.is_empty() {
        String::new()
    } else {
        render(template, &[("instruction".into(), instruction.into())])?
    };
    Ok((
        "runtime:system-context:94-standing-instruction".into(),
        content,
    ))
}

pub fn render_skill_catalog_entries(
    skills: &[SkillCatalogEntry],
    template: &PromptTemplate,
) -> Result<(String, String), Error> {
    Ok((
        "runtime:system-context:09b-skills".into(),
        render_catalog_entries(skills, template)?,
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

pub const MAXIMUM_CATALOG_BYTES: usize = 64 * 1024;

pub fn render_catalog_entries(
    skills: &[SkillCatalogEntry],
    template: &PromptTemplate,
) -> Result<String, Error> {
    let available: Vec<_> = skills
        .iter()
        .filter(|skill| skill.enabled && skill.prompt_visible)
        .collect();
    if available.is_empty() {
        return Ok(String::new());
    }
    let wrapper = render(template, &[("skills".into(), "{skills}".into())])?;
    let budget = (MAXIMUM_CATALOG_BYTES + "{skills}".len())
        .checked_sub(wrapper.len())
        .filter(|budget| *budget > 0)
        .ok_or_else(|| {
            Error::prompt_context("The skills prompt template exceeds the 64 KiB catalogue limit.")
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
        template,
        &[("skills".into(), entries.concat().trim_end().into())],
    )
}

fn omitted_catalog(count: usize) -> String {
    format!(
        "- {count} additional skill(s) omitted because the catalogue reached its 64 KiB limit.\n"
    )
}
