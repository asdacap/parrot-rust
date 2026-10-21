use super::{PromptError, PromptResult, SkillMetadata, render};
use parrot_foundation::template_catalog::PromptTemplateCatalog;

pub const MAXIMUM_CATALOG_BYTES: usize = 64 * 1024;
pub const MAXIMUM_SKILL_BYTES: usize = 8 * 1024;
pub const MAXIMUM_TURN_BYTES: usize = 1024 * 1024;

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

pub(crate) fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub(crate) fn unavailable(
    templates: &PromptTemplateCatalog,
    name: &str,
    message: &str,
) -> PromptResult<String> {
    let normalized = message.split_whitespace().collect::<Vec<_>>().join(" ");
    // Source length is UTF-16; do not truncate in the middle of a Rust UTF-8 character.
    let mut message = String::new();
    let mut units = 0;
    for character in normalized.chars() {
        if units + character.len_utf16() > 1024 {
            break;
        }
        message.push(character);
        units += character.len_utf16();
    }
    render(
        templates,
        "context.skill-unavailable",
        &[
            ("name".into(), escape(name).into()),
            ("message".into(), escape(&message).into()),
        ],
    )
}

pub(crate) struct SelectionRendering {
    pub content: String,
    pub loaded_paths: Vec<String>,
    content_budget: usize,
    omitted: i32,
}
impl SelectionRendering {
    pub fn new(templates: &PromptTemplateCatalog) -> PromptResult<Self> {
        let summary = unavailable(
            templates,
            "selected skills",
            &format!(
                "{} diagnostics omitted by the active-turn prompt budget",
                i32::MAX
            ),
        )?;
        let budget = MAXIMUM_TURN_BYTES
            .checked_sub(summary.len() + 2)
            .filter(|budget| *budget > 0)
            .ok_or_else(|| {
                PromptError::Context(
                    "The unavailable-skill prompt template exceeds the active-turn prompt budget."
                        .into(),
                )
            })?;
        Ok(Self {
            content: String::new(),
            loaded_paths: Vec::new(),
            content_budget: budget,
            omitted: 0,
        })
    }
    fn fits(&self, value: &str) -> bool {
        self.content.len() + if self.content.is_empty() { 0 } else { 2 } + value.len()
            <= self.content_budget
    }
    fn append(&mut self, value: &str) {
        if !self.content.is_empty() {
            self.content.push_str("\n\n");
        }
        self.content.push_str(value);
    }
    pub fn diagnostic(
        &mut self,
        templates: &PromptTemplateCatalog,
        name: &str,
        message: &str,
    ) -> PromptResult<()> {
        let value = unavailable(templates, name, message)?;
        self.notice(&value);
        Ok(())
    }
    fn notice(&mut self, value: &str) {
        if self.fits(value) {
            self.append(value);
        } else {
            self.omitted += 1;
        }
    }
    pub fn selected(
        &mut self,
        templates: &PromptTemplateCatalog,
        skill: &SkillMetadata,
        content: &str,
    ) -> PromptResult<()> {
        if content.len() > MAXIMUM_SKILL_BYTES {
            let notice = render(
                templates,
                "context.skill-too-large",
                &[
                    ("name".into(), escape(&skill.name).into()),
                    ("path".into(), escape(&skill.discovery_path).into()),
                    ("limit_bytes".into(), MAXIMUM_SKILL_BYTES.to_string().into()),
                ],
            )?;
            self.notice(&notice);
            return Ok(());
        }
        let selected = render(
            templates,
            "context.skill-selected",
            &[
                ("name".into(), escape(&skill.name).into()),
                ("path".into(), escape(&skill.discovery_path).into()),
                ("content".into(), content.into()),
            ],
        )?;
        if self.fits(&selected) {
            self.append(&selected);
            self.loaded_paths.push(skill.discovery_path.clone());
        } else {
            self.diagnostic(
                templates,
                &skill.name,
                "selected skill instructions exceed the active-turn prompt budget",
            )?;
        }
        Ok(())
    }
    pub fn finish(mut self, templates: &PromptTemplateCatalog) -> PromptResult<Self> {
        if self.omitted > 0 {
            let summary = unavailable(
                templates,
                "selected skills",
                &format!(
                    "{} diagnostics omitted by the active-turn prompt budget",
                    self.omitted
                ),
            )?;
            self.append(&summary);
            if self.content.len() > MAXIMUM_TURN_BYTES {
                return Err(PromptError::Context(
                    "The selected-skill context exceeds the active-turn prompt budget.".into(),
                ));
            }
        }
        Ok(self)
    }
}
