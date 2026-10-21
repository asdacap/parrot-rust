use super::{PromptError, PromptResult, StaticSystemPrompt, TemplateSystemPromptProvider, render};
use parrot_domain::values::project_workspace::ProjectWorkspace;
use parrot_foundation::{
    paths::{AgentScratchDirectory, UserSessionResources},
    template_catalog::PromptTemplateCatalog,
};
use serde_json::json;
use std::{collections::BTreeMap, path::Path, sync::Arc};

/// Captures actual selected session resources. These values are addresses, not grants.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentPathEnvironment {
    entries: BTreeMap<String, String>,
}
impl AgentPathEnvironment {
    pub fn from_resources(
        resources: &UserSessionResources<ProjectWorkspace>,
        scratch: &AgentScratchDirectory,
    ) -> PromptResult<Self> {
        let text = |path: &Path| {
            path.to_str().map(str::to_owned).ok_or_else(|| {
                PromptError::Context("agent environment path is not valid Unicode".into())
            })
        };
        Ok(Self {
            entries: BTreeMap::from([
                (
                    "WORKDIR".into(),
                    text(resources.workspace().launch_directory())?,
                ),
                ("SCRATCH_DIR".into(), text(&resources.scratch_directory())?),
                ("AGENT_SCRATCH_DIR".into(), text(&scratch.scratch_path())?),
                ("AGENT_HISTORY_DIR".into(), text(scratch.root())?),
            ]),
        })
    }

    pub fn materialize(&self) -> &BTreeMap<String, String> {
        &self.entries
    }

    pub fn merge(&self, overrides: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        let mut entries = self.entries.clone();
        entries.extend(overrides.clone());
        entries
    }

    pub fn prompt_provider(
        &self,
        templates: Arc<PromptTemplateCatalog>,
    ) -> TemplateSystemPromptProvider {
        let mut entries: Vec<_> = self.entries.iter().collect();
        entries.sort_by(|(a, _), (b, _)| b.encode_utf16().cmp(a.encode_utf16()));
        let mut displayed: Vec<(&String, &String)> = Vec::new();
        let mut rendered = Vec::new();
        for (name, value) in entries {
            let mut path = value.clone();
            // OrderByDescending is stable: equally long earlier paths retain precedence.
            let mut previous = displayed.clone();
            previous.sort_by_key(|(_, path)| std::cmp::Reverse(path.encode_utf16().count()));
            for (previous_name, previous_path) in previous {
                if let Ok(relative) = Path::new(value).strip_prefix(previous_path) {
                    path = if relative.as_os_str().is_empty() {
                        format!("${previous_name}")
                    } else {
                        format!("${previous_name}/{}", relative.display())
                    };
                    break;
                }
            }
            rendered.push(json!({"name": name, "path": path}));
            displayed.push((name, value));
        }
        TemplateSystemPromptProvider::new(
            "runtime:system-context:90-agent-path-environment",
            "context.agent-path-environment",
            vec![("entries".into(), rendered.into())],
            templates,
        )
    }
}

/// The host supplies its real locator; this operation neither launches a command
/// nor treats a utility name or PATH string as filesystem authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CliUtilityAvailability {
    pub available_expected: Vec<String>,
    pub missing_expected: Vec<String>,
    pub available_optional: Vec<String>,
}
impl CliUtilityAvailability {
    pub fn inspect(
        expected: &[String],
        optional: &[String],
        macos: bool,
        mut locate: impl FnMut(&str) -> bool,
    ) -> Self {
        let mut value = Self {
            available_expected: Vec::new(),
            missing_expected: Vec::new(),
            available_optional: Vec::new(),
        };
        for command in expected {
            if macos && command == "bwrap" {
                continue;
            }
            if locate(command) {
                value.available_expected.push(command.clone());
            } else {
                value.missing_expected.push(command.clone());
            }
        }
        for command in optional {
            if locate(command) {
                value.available_optional.push(command.clone());
            }
        }
        for commands in [
            &mut value.available_expected,
            &mut value.missing_expected,
            &mut value.available_optional,
        ] {
            commands.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
        }
        value
    }
}

pub fn native_platform_identifier() -> String {
    let os = match std::env::consts::OS {
        "macos" => "osx",
        "windows" => "win",
        os => os,
    };
    let architecture = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "x86",
        architecture => architecture,
    };
    format!("{os}-{architecture}")
}

pub fn git_repository_prompt(
    workspace: &ProjectWorkspace,
    templates: &PromptTemplateCatalog,
) -> PromptResult<StaticSystemPrompt> {
    Ok(StaticSystemPrompt(render(
        templates,
        "context.git-repository",
        &[(
            "is_repository".into(),
            workspace.is_git_repository().to_string().into(),
        )],
    )?))
}
