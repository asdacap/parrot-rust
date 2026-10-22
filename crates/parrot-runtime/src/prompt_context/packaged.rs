//! Unmodified packaged source assets; publishing belongs to an injected installer.
#[derive(Clone, Copy, Debug)]
pub struct PackagedSkillAsset {
    pub relative_path: &'static str,
    pub bytes: &'static [u8],
    pub executable: bool,
}
pub static PACKAGED_SKILL_ASSETS: &[PackagedSkillAsset] = &[
    PackagedSkillAsset {
        relative_path: "agent-task-graph/SKILL.md",
        bytes: include_bytes!("../../../../assets/skills/agent-task-graph/SKILL.md"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "domain-modeling/ADR-FORMAT.md",
        bytes: include_bytes!("../../../../assets/skills/domain-modeling/ADR-FORMAT.md"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "domain-modeling/agents/openai.yaml",
        bytes: include_bytes!("../../../../assets/skills/domain-modeling/agents/openai.yaml"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "domain-modeling/GLOSSARY-FORMAT.md",
        bytes: include_bytes!("../../../../assets/skills/domain-modeling/GLOSSARY-FORMAT.md"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "domain-modeling/LICENSE.txt",
        bytes: include_bytes!("../../../../assets/skills/domain-modeling/LICENSE.txt"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "domain-modeling/SKILL.md",
        bytes: include_bytes!("../../../../assets/skills/domain-modeling/SKILL.md"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "grilling/agents/openai.yaml",
        bytes: include_bytes!("../../../../assets/skills/grilling/agents/openai.yaml"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "grilling/LICENSE.txt",
        bytes: include_bytes!("../../../../assets/skills/grilling/LICENSE.txt"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "grilling/SKILL.md",
        bytes: include_bytes!("../../../../assets/skills/grilling/SKILL.md"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "grill-with-docs/agents/openai.yaml",
        bytes: include_bytes!("../../../../assets/skills/grill-with-docs/agents/openai.yaml"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "grill-with-docs/LICENSE.txt",
        bytes: include_bytes!("../../../../assets/skills/grill-with-docs/LICENSE.txt"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "grill-with-docs/SKILL.md",
        bytes: include_bytes!("../../../../assets/skills/grill-with-docs/SKILL.md"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "imagegen/agents/openai.yaml",
        bytes: include_bytes!("../../../../assets/skills/imagegen/agents/openai.yaml"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "imagegen/assets/imagegen.png",
        bytes: include_bytes!("../../../../assets/skills/imagegen/assets/imagegen.png"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "imagegen/assets/imagegen-small.svg",
        bytes: include_bytes!("../../../../assets/skills/imagegen/assets/imagegen-small.svg"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "imagegen/LICENSE.txt",
        bytes: include_bytes!("../../../../assets/skills/imagegen/LICENSE.txt"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "imagegen/references/cli.md",
        bytes: include_bytes!("../../../../assets/skills/imagegen/references/cli.md"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "imagegen/references/codex-network.md",
        bytes: include_bytes!("../../../../assets/skills/imagegen/references/codex-network.md"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "imagegen/references/image-api.md",
        bytes: include_bytes!("../../../../assets/skills/imagegen/references/image-api.md"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "imagegen/references/prompting.md",
        bytes: include_bytes!("../../../../assets/skills/imagegen/references/prompting.md"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "imagegen/references/sample-prompts.md",
        bytes: include_bytes!("../../../../assets/skills/imagegen/references/sample-prompts.md"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "imagegen/scripts/image_gen.py",
        bytes: include_bytes!("../../../../assets/skills/imagegen/scripts/image_gen.py"),
        executable: true,
    },
    PackagedSkillAsset {
        relative_path: "imagegen/scripts/remove_chroma_key.py",
        bytes: include_bytes!("../../../../assets/skills/imagegen/scripts/remove_chroma_key.py"),
        executable: true,
    },
    PackagedSkillAsset {
        relative_path: "imagegen/SKILL.md",
        bytes: include_bytes!("../../../../assets/skills/imagegen/SKILL.md"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "openai-docs/agents/openai.yaml",
        bytes: include_bytes!("../../../../assets/skills/openai-docs/agents/openai.yaml"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "openai-docs/assets/openai.png",
        bytes: include_bytes!("../../../../assets/skills/openai-docs/assets/openai.png"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "openai-docs/assets/openai-small.svg",
        bytes: include_bytes!("../../../../assets/skills/openai-docs/assets/openai-small.svg"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "openai-docs/LICENSE.txt",
        bytes: include_bytes!("../../../../assets/skills/openai-docs/LICENSE.txt"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "openai-docs/references/codex-self-knowledge.md",
        bytes: include_bytes!(
            "../../../../assets/skills/openai-docs/references/codex-self-knowledge.md"
        ),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "openai-docs/references/latest-model.md",
        bytes: include_bytes!("../../../../assets/skills/openai-docs/references/latest-model.md"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "openai-docs/references/mcp-diagnostics.md",
        bytes: include_bytes!(
            "../../../../assets/skills/openai-docs/references/mcp-diagnostics.md"
        ),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "openai-docs/references/model-migration.md",
        bytes: include_bytes!(
            "../../../../assets/skills/openai-docs/references/model-migration.md"
        ),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "openai-docs/references/model-selection.md",
        bytes: include_bytes!(
            "../../../../assets/skills/openai-docs/references/model-selection.md"
        ),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "openai-docs/references/official-docs.md",
        bytes: include_bytes!("../../../../assets/skills/openai-docs/references/official-docs.md"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "openai-docs/references/prompting-guide.md",
        bytes: include_bytes!(
            "../../../../assets/skills/openai-docs/references/prompting-guide.md"
        ),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "openai-docs/references/upgrade-guide.md",
        bytes: include_bytes!("../../../../assets/skills/openai-docs/references/upgrade-guide.md"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "openai-docs/references/upgrading-to-gpt-5p6-sol.md",
        bytes: include_bytes!(
            "../../../../assets/skills/openai-docs/references/upgrading-to-gpt-5p6-sol.md"
        ),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "openai-docs/scripts/fetch-codex-manual.mjs",
        bytes: include_bytes!(
            "../../../../assets/skills/openai-docs/scripts/fetch-codex-manual.mjs"
        ),
        executable: true,
    },
    PackagedSkillAsset {
        relative_path: "openai-docs/scripts/resolve-latest-model-info",
        bytes: include_bytes!(
            "../../../../assets/skills/openai-docs/scripts/resolve-latest-model-info"
        ),
        executable: true,
    },
    PackagedSkillAsset {
        relative_path: "openai-docs/scripts/resolve-latest-model-info.cjs",
        bytes: include_bytes!(
            "../../../../assets/skills/openai-docs/scripts/resolve-latest-model-info.cjs"
        ),
        executable: true,
    },
    PackagedSkillAsset {
        relative_path: "openai-docs/SKILL.md",
        bytes: include_bytes!("../../../../assets/skills/openai-docs/SKILL.md"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "plugin-creator/agents/openai.yaml",
        bytes: include_bytes!("../../../../assets/skills/plugin-creator/agents/openai.yaml"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "plugin-creator/assets/plugin-creator.png",
        bytes: include_bytes!("../../../../assets/skills/plugin-creator/assets/plugin-creator.png"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "plugin-creator/assets/plugin-creator-small.svg",
        bytes: include_bytes!(
            "../../../../assets/skills/plugin-creator/assets/plugin-creator-small.svg"
        ),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "plugin-creator/references/installing-and-updating.md",
        bytes: include_bytes!(
            "../../../../assets/skills/plugin-creator/references/installing-and-updating.md"
        ),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "plugin-creator/references/plugin-json-spec.md",
        bytes: include_bytes!(
            "../../../../assets/skills/plugin-creator/references/plugin-json-spec.md"
        ),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "plugin-creator/scripts/create_basic_plugin.py",
        bytes: include_bytes!(
            "../../../../assets/skills/plugin-creator/scripts/create_basic_plugin.py"
        ),
        executable: true,
    },
    PackagedSkillAsset {
        relative_path: "plugin-creator/scripts/identifier_validation.py",
        bytes: include_bytes!(
            "../../../../assets/skills/plugin-creator/scripts/identifier_validation.py"
        ),
        executable: true,
    },
    PackagedSkillAsset {
        relative_path: "plugin-creator/scripts/read_marketplace_name.py",
        bytes: include_bytes!(
            "../../../../assets/skills/plugin-creator/scripts/read_marketplace_name.py"
        ),
        executable: true,
    },
    PackagedSkillAsset {
        relative_path: "plugin-creator/scripts/update_plugin_cachebuster.py",
        bytes: include_bytes!(
            "../../../../assets/skills/plugin-creator/scripts/update_plugin_cachebuster.py"
        ),
        executable: true,
    },
    PackagedSkillAsset {
        relative_path: "plugin-creator/scripts/validate_plugin.py",
        bytes: include_bytes!(
            "../../../../assets/skills/plugin-creator/scripts/validate_plugin.py"
        ),
        executable: true,
    },
    PackagedSkillAsset {
        relative_path: "plugin-creator/SKILL.md",
        bytes: include_bytes!("../../../../assets/skills/plugin-creator/SKILL.md"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "review-agent/agents/openai.yaml",
        bytes: include_bytes!("../../../../assets/skills/review-agent/agents/openai.yaml"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "review-agent/SKILL.md",
        bytes: include_bytes!("../../../../assets/skills/review-agent/SKILL.md"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "skill-creator/agents/openai.yaml",
        bytes: include_bytes!("../../../../assets/skills/skill-creator/agents/openai.yaml"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "skill-creator/assets/skill-creator.png",
        bytes: include_bytes!("../../../../assets/skills/skill-creator/assets/skill-creator.png"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "skill-creator/assets/skill-creator-small.svg",
        bytes: include_bytes!(
            "../../../../assets/skills/skill-creator/assets/skill-creator-small.svg"
        ),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "skill-creator/license.txt",
        bytes: include_bytes!("../../../../assets/skills/skill-creator/license.txt"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "skill-creator/references/openai_yaml.md",
        bytes: include_bytes!("../../../../assets/skills/skill-creator/references/openai_yaml.md"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "skill-creator/scripts/generate_openai_yaml.py",
        bytes: include_bytes!(
            "../../../../assets/skills/skill-creator/scripts/generate_openai_yaml.py"
        ),
        executable: true,
    },
    PackagedSkillAsset {
        relative_path: "skill-creator/scripts/init_skill.py",
        bytes: include_bytes!("../../../../assets/skills/skill-creator/scripts/init_skill.py"),
        executable: true,
    },
    PackagedSkillAsset {
        relative_path: "skill-creator/scripts/quick_validate.py",
        bytes: include_bytes!("../../../../assets/skills/skill-creator/scripts/quick_validate.py"),
        executable: true,
    },
    PackagedSkillAsset {
        relative_path: "skill-creator/SKILL.md",
        bytes: include_bytes!("../../../../assets/skills/skill-creator/SKILL.md"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "skill-installer/agents/openai.yaml",
        bytes: include_bytes!("../../../../assets/skills/skill-installer/agents/openai.yaml"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "skill-installer/assets/skill-installer.png",
        bytes: include_bytes!(
            "../../../../assets/skills/skill-installer/assets/skill-installer.png"
        ),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "skill-installer/assets/skill-installer-small.svg",
        bytes: include_bytes!(
            "../../../../assets/skills/skill-installer/assets/skill-installer-small.svg"
        ),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "skill-installer/LICENSE.txt",
        bytes: include_bytes!("../../../../assets/skills/skill-installer/LICENSE.txt"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "skill-installer/scripts/github_utils.py",
        bytes: include_bytes!("../../../../assets/skills/skill-installer/scripts/github_utils.py"),
        executable: true,
    },
    PackagedSkillAsset {
        relative_path: "skill-installer/scripts/install-skill-from-github.py",
        bytes: include_bytes!(
            "../../../../assets/skills/skill-installer/scripts/install-skill-from-github.py"
        ),
        executable: true,
    },
    PackagedSkillAsset {
        relative_path: "skill-installer/scripts/list-skills.py",
        bytes: include_bytes!("../../../../assets/skills/skill-installer/scripts/list-skills.py"),
        executable: true,
    },
    PackagedSkillAsset {
        relative_path: "skill-installer/SKILL.md",
        bytes: include_bytes!("../../../../assets/skills/skill-installer/SKILL.md"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "unslop/agents/openai.yaml",
        bytes: include_bytes!("../../../../assets/skills/unslop/agents/openai.yaml"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "unslop/LICENSE.txt",
        bytes: include_bytes!("../../../../assets/skills/unslop/LICENSE.txt"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "unslop/SKILL.md",
        bytes: include_bytes!("../../../../assets/skills/unslop/SKILL.md"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "wayfinder/agents/openai.yaml",
        bytes: include_bytes!("../../../../assets/skills/wayfinder/agents/openai.yaml"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "wayfinder/LICENSE.txt",
        bytes: include_bytes!("../../../../assets/skills/wayfinder/LICENSE.txt"),
        executable: false,
    },
    PackagedSkillAsset {
        relative_path: "wayfinder/SKILL.md",
        bytes: include_bytes!("../../../../assets/skills/wayfinder/SKILL.md"),
        executable: false,
    },
];

use parrot_domain::contracts::packaged_files::{
    InstallMissingError, InstallMissingOutcome, InstallMissingRequest, PackagedFileInstaller,
};
use std::{fmt, path::Path};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PackagedSkillsError {
    EmptyDestination,
    Write(InstallMissingError),
    InvalidOperation(InstallMissingError),
}
impl fmt::Display for PackagedSkillsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyDestination => formatter.write_str("destination must not be empty"),
            Self::Write(_) => formatter.write_str("unable to write packaged skills"),
            Self::InvalidOperation(error) => error.fmt(formatter),
        }
    }
}
impl std::error::Error for PackagedSkillsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Write(error) | Self::InvalidOperation(error) => Some(error),
            Self::EmptyDestination => None,
        }
    }
}

/// Await and join this entire operation before releasing the installer scope.
/// It supplies resources to the injected publisher; it never opens files itself.
pub async fn extract_packaged_skills<I: PackagedFileInstaller + ?Sized>(
    destination: &Path,
    installer: &I,
    security: &I::Security,
) -> Result<(), PackagedSkillsError> {
    for request in requests(destination)? {
        installer
            .install_missing(&request, security)
            .await
            .map_err(install_error)?;
    }
    Ok(())
}

/// Installs the same embedded resources during synchronous host construction.
pub fn extract_packaged_skills_sync(
    destination: &Path,
    mut install: impl FnMut(
        &InstallMissingRequest,
    ) -> Result<InstallMissingOutcome, InstallMissingError>,
) -> Result<(), PackagedSkillsError> {
    for request in requests(destination)? {
        install(&request).map_err(install_error)?;
    }
    Ok(())
}

fn requests(
    destination: &Path,
) -> Result<impl Iterator<Item = InstallMissingRequest> + '_, PackagedSkillsError> {
    if destination.as_os_str().is_empty() {
        return Err(PackagedSkillsError::EmptyDestination);
    }
    Ok(PACKAGED_SKILL_ASSETS
        .iter()
        .map(move |asset| InstallMissingRequest {
            target: destination.join(asset.relative_path),
            contents: asset.bytes.to_vec(),
            executable: asset.executable,
        }))
}

fn install_error(error: InstallMissingError) -> PackagedSkillsError {
    if error.kind.is_io_or_denied() {
        PackagedSkillsError::Write(error)
    } else {
        PackagedSkillsError::InvalidOperation(error)
    }
}
