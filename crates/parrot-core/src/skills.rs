use crate::{
    CancellationToken, ConfigKey, Content, Error, PreparedSkillPrompt, PromotedInput,
    PromptTemplate, Skill, SkillContent, SkillProvider, SkillReference, States,
    parse_skill_mentions,
};
use std::{collections::HashSet, sync::Arc};

const MAXIMUM_SKILL_BYTES: usize = 8 * 1024;
const MAXIMUM_TURN_BYTES: usize = 1024 * 1024;

#[derive(Default)]
pub(crate) struct SkillPreparation {
    selected_ids: HashSet<String>,
}

impl SkillPreparation {
    pub async fn prepare(
        &mut self,
        states: &mut dyn States,
        skill_provider: &dyn SkillProvider,
        inputs: &[PromotedInput],
        cancellation: CancellationToken<'_>,
    ) -> Result<PreparedSkillPrompt, Error> {
        let selected = self
            .detect_selected_skills(skill_provider, inputs, cancellation)
            .await?;

        if cancellation.is_cancelled() {
            return Err(Error::cancelled());
        }

        if selected.is_empty() {
            return Ok(PreparedSkillPrompt::default());
        }

        let templates = SkillTemplates {
            unavailable: states
                .get_config(ConfigKey::SkillUnavailableTemplate)
                .await?
                .into_prompt_template()?,
            selected: states
                .get_config(ConfigKey::SkillSelectedTemplate)
                .await?
                .into_prompt_template()?,
            too_large: states
                .get_config(ConfigKey::SkillTooLargeTemplate)
                .await?
                .into_prompt_template()?,
        };
        let mut rendering = SkillRendering::new(&templates)?;
        for (skill, _) in selected {
            if cancellation.is_cancelled() {
                return Err(Error::cancelled());
            }
            match skill_provider.read_skill_content(&skill).await? {
                SkillContent::Text(content) => rendering.selected(&templates, &skill, &content)?,
                SkillContent::Unavailable(message) => {
                    rendering.diagnostic(&templates, &skill.name, &message)?;
                }
            }
        }
        if cancellation.is_cancelled() {
            return Err(Error::cancelled());
        }
        rendering.finish(&templates)
    }

    /// Detects skills newly selected by the promoted inputs and returns them in
    /// input, mention and identity-tiebreak order, skipping skills already
    /// selected on earlier turns.
    async fn detect_selected_skills(
        &mut self,
        skill_provider: &dyn SkillProvider,
        inputs: &[PromotedInput],
        cancellation: CancellationToken<'_>,
    ) -> Result<Vec<(Skill, usize)>, Error> {
        let mut selected = Vec::new();
        for input in inputs {
            let mut mentions = Vec::new();
            let mut offset = 0;
            for content in &input.message.contents {
                if let Content::Text(text) = content {
                    mentions.extend(parse_skill_mentions(text).into_iter().map(|mention| {
                        (
                            SkillReference {
                                name: mention.name,
                                path: mention.path,
                            },
                            offset + mention.position,
                        )
                    }));
                    offset += text.encode_utf16().count();
                }
            }
            if !mentions.is_empty() {
                if cancellation.is_cancelled() {
                    return Err(Error::cancelled());
                }
                let references = mentions
                    .iter()
                    .map(|(reference, _)| reference.clone())
                    .collect::<Vec<_>>();
                let resolved = skill_provider.resolve_skill_references(&references).await?;
                for (record, (_, position)) in resolved.into_iter().zip(mentions) {
                    if let Some(skill) = record
                        && self.selected_ids.insert(skill.id.clone())
                    {
                        selected.push((skill, position));
                    }
                }
            }
        }
        selected.sort_by(|(left, lp), (right, rp)| {
            lp.cmp(rp)
                .then_with(|| left.id.encode_utf16().cmp(right.id.encode_utf16()))
        });
        Ok(selected)
    }
}

struct SkillTemplates {
    unavailable: Arc<PromptTemplate>,
    selected: Arc<PromptTemplate>,
    too_large: Arc<PromptTemplate>,
}

fn render_template(
    template: &PromptTemplate,
    arguments: &[(String, String)],
) -> Result<String, Error> {
    let arguments = arguments
        .iter()
        .map(|(name, value)| (name.clone(), serde_json::Value::String(value.clone())))
        .collect::<Vec<_>>();
    template.render(&arguments, CancellationToken::uncancelled())
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn unavailable(templates: &SkillTemplates, name: &str, message: &str) -> Result<String, Error> {
    let normalized = message.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut message = String::new();
    let mut units = 0;
    for character in normalized.chars() {
        if units + character.len_utf16() > 1024 {
            break;
        }
        message.push(character);
        units += character.len_utf16();
    }
    render_template(
        &templates.unavailable,
        &[
            ("name".into(), escape(name)),
            ("message".into(), escape(&message)),
        ],
    )
}

struct SkillRendering {
    prompt: PreparedSkillPrompt,
    content_budget: usize,
    omitted: i32,
}

impl SkillRendering {
    fn new(templates: &SkillTemplates) -> Result<Self, Error> {
        let summary = unavailable(
            templates,
            "selected skills",
            &format!(
                "{} diagnostics omitted by the active-turn prompt budget",
                i32::MAX
            ),
        )?;
        let content_budget = MAXIMUM_TURN_BYTES
            .checked_sub(summary.len() + 2)
            .filter(|budget| *budget > 0)
            .ok_or_else(|| {
                Error::failed(
                    "The unavailable-skill prompt template exceeds the active-turn prompt budget.",
                )
            })?;
        Ok(Self {
            prompt: PreparedSkillPrompt::default(),
            content_budget,
            omitted: 0,
        })
    }

    fn fits(&self, value: &str) -> bool {
        self.prompt.content.len() + if self.prompt.content.is_empty() { 0 } else { 2 } + value.len()
            <= self.content_budget
    }

    fn append(&mut self, value: &str) {
        if !self.prompt.content.is_empty() {
            self.prompt.content.push_str("\n\n");
        }
        self.prompt.content.push_str(value);
    }

    fn notice(&mut self, value: &str) {
        if self.fits(value) {
            self.append(value);
        } else {
            self.omitted += 1;
        }
    }

    fn diagnostic(
        &mut self,
        templates: &SkillTemplates,
        name: &str,
        message: &str,
    ) -> Result<(), Error> {
        let rendered = unavailable(templates, name, message)?;
        self.notice(&rendered);
        Ok(())
    }

    fn selected(
        &mut self,
        templates: &SkillTemplates,
        skill: &Skill,
        content: &str,
    ) -> Result<(), Error> {
        if content.len() > MAXIMUM_SKILL_BYTES {
            let rendered = render_template(
                &templates.too_large,
                &[
                    ("name".into(), escape(&skill.name)),
                    ("path".into(), escape(&skill.path)),
                    ("limit_bytes".into(), MAXIMUM_SKILL_BYTES.to_string()),
                ],
            )?;
            self.notice(&rendered);
            return Ok(());
        }
        let rendered = render_template(
            &templates.selected,
            &[
                ("name".into(), escape(&skill.name)),
                ("path".into(), escape(&skill.path)),
                ("content".into(), content.into()),
            ],
        )?;
        if self.fits(&rendered) {
            self.append(&rendered);
            self.prompt.loaded_paths.push(skill.path.clone());
        } else {
            self.diagnostic(
                templates,
                &skill.name,
                "selected skill instructions exceed the active-turn prompt budget",
            )?;
        }
        Ok(())
    }

    fn finish(mut self, templates: &SkillTemplates) -> Result<PreparedSkillPrompt, Error> {
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
            if self.prompt.content.len() > MAXIMUM_TURN_BYTES {
                return Err(Error::failed(
                    "The selected-skill context exceeds the active-turn prompt budget.",
                ));
            }
        }
        Ok(self.prompt)
    }
}
