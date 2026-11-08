use crate::presenters::*;
use crate::terminal_layout::ToolYamlFormatter;
use parrot_domain::protocol::messages::parrot::{ListedSkillScope, SkillRootLocation};
use parrot_runtime::prompt_context::normalize_skill_path;
use serde_json::Value;
use std::{
    path::Path,
    sync::{Arc, RwLock},
};

#[derive(Default)]
pub struct SkillReadPathFormatter {
    working_directory: String,
    root_locations: RwLock<Vec<SkillRootLocation>>,
}
impl SkillReadPathFormatter {
    pub fn new(working_directory: String, root_locations: Vec<SkillRootLocation>) -> Self {
        Self {
            working_directory,
            root_locations: RwLock::new(root_locations),
        }
    }
    pub fn replace_root_locations(&self, root_locations: Vec<SkillRootLocation>) {
        *self
            .root_locations
            .write()
            .unwrap_or_else(|error| error.into_inner()) = root_locations;
    }
    fn shorten(&self, path: &str) -> Option<String> {
        let path = normalize_skill_path(path, Some(&self.working_directory)).ok()?;
        let locations = self
            .root_locations
            .read()
            .unwrap_or_else(|error| error.into_inner());
        let mut matched: Option<(usize, String)> = None;
        for location in locations.iter() {
            let alias = match location.scope.enum_value() {
                Ok(ListedSkillScope::SKILL_SCOPE_REPO) => "$REPO_SKILLS",
                Ok(ListedSkillScope::SKILL_SCOPE_USER) => "$USER_SKILLS",
                Ok(ListedSkillScope::SKILL_SCOPE_SYSTEM) => "$SYSTEM_SKILLS",
                _ => continue,
            };
            let Ok(root) = normalize_skill_path(&location.path, None) else {
                continue;
            };
            let Ok(suffix) = Path::new(&path).strip_prefix(&root) else {
                continue;
            };
            if matched
                .as_ref()
                .is_none_or(|(length, _)| root.len() > *length)
            {
                let suffix = suffix.to_string_lossy();
                let display = if suffix.is_empty() {
                    alias.to_owned()
                } else {
                    format!("{alias}/{suffix}")
                };
                matched = Some((root.len(), display));
            }
        }
        matched.map(|(_, display)| display)
    }
}

#[derive(Default)]
pub struct ReadToolPresenter {
    skill_paths: Arc<SkillReadPathFormatter>,
}
impl ReadToolPresenter {
    pub fn new(skill_paths: Arc<SkillReadPathFormatter>) -> Self {
        Self { skill_paths }
    }
    fn describe_call(
        &self,
        call: &ToolCallPresentation,
    ) -> PresenterResult<(String, ToolPresentationMetadata)> {
        let value: Value = serde_json::from_str(&call.arguments_json)?;
        let path = value
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let skill_path = self.skill_paths.shorten(path);
        let display_path = skill_path
            .as_deref()
            .map_or_else(|| call.shorten_path(path), str::to_owned);
        let offset = value.get("offset").and_then(Value::as_i64);
        let limit = value.get("limit").and_then(Value::as_i64);
        let label = if offset.is_some() || limit.is_some() {
            let offset = offset.unwrap_or(1);
            let limit = limit.unwrap_or(2000);
            let end = offset.saturating_add(limit.saturating_sub(1));
            format!("read L{offset}-L{end} {display_path}")
        } else {
            format!("read {display_path}")
        };
        Ok((
            label,
            ToolPresentationMetadata {
                style: if skill_path.is_some() {
                    ToolPresentationStyle::Default
                } else {
                    ToolPresentationStyle::Muted
                },
                ..self.metadata()
            },
        ))
    }
}
impl ToolPresenter for ReadToolPresenter {
    fn tool_name(&self) -> &str {
        "read"
    }
    fn metadata(&self) -> ToolPresentationMetadata {
        ToolPresentationMetadata {
            style: ToolPresentationStyle::Muted,
            context_dependent_live: true,
            ..Default::default()
        }
    }
    fn present_live(
        &self,
        call: &ToolCallPresentation,
        frame: usize,
    ) -> PresenterResult<Presentation> {
        let (label, metadata) = self.describe_call(call)?;
        Ok(Presentation::Tool(ToolReport::live(
            label,
            ToolBlock::empty(),
            metadata,
            frame,
        )))
    }
    fn present_terminal(
        &self,
        call: &ToolCallPresentation,
        terminal: &ToolTerminalPresentation,
    ) -> PresenterResult<Option<Presentation>> {
        let (mut label, metadata) = self.describe_call(call)?;
        let status = terminal.resolve_status();
        let block = if matches!(
            status,
            ToolTerminalStatus::Errored | ToolTerminalStatus::ReportedFailure
        ) {
            let error = terminal.describe_block(ToolBlockKind::None).text;
            if error == "error: no such file or directory" {
                label.push_str(" · not found");
                ToolBlock::empty()
            } else {
                ToolBlock::from_error(format!(
                    "{}\n---\n{}",
                    ToolYamlFormatter::format(&call.arguments_json)?,
                    error
                ))
            }
        } else {
            ToolBlock::empty()
        };
        Ok(Some(Presentation::Tool(ToolReport::terminal(
            label, block, status, metadata,
        ))))
    }
}
