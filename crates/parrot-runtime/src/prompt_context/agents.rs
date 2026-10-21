use super::{
    AgentIdentityDescription, PromptError, PromptResult, SystemPrompt, SystemPromptProvider,
    normalize_skill_path, render,
};
use parrot_domain::contracts::{
    async_ops::{BoxFuture, Cancellation, OperationError},
    filesystem::{
        AuthorizedReadService, EntryKind, FileReadError, FileReadErrorKind, LinkPolicy, ReadRequest,
    },
    security::SecurityProvider,
};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use parrot_provider::routing::aliases::ResolvedModelSelection;
use std::{path::PathBuf, sync::Arc};

pub struct AgentsPromptProvider<
    R: AuthorizedReadService,
    P: SecurityProvider<Snapshot = R::Security>,
> {
    working_directory: PathBuf,
    configuration_directory: PathBuf,
    reader: Arc<R>,
    security: Arc<P>,
    templates: Arc<PromptTemplateCatalog>,
}
impl<R: AuthorizedReadService, P: SecurityProvider<Snapshot = R::Security>>
    AgentsPromptProvider<R, P>
{
    pub fn new(
        working_directory: &str,
        configuration_directory: &str,
        reader: Arc<R>,
        security: Arc<P>,
        templates: Arc<PromptTemplateCatalog>,
    ) -> PromptResult<Self> {
        let working_directory =
            normalize_skill_path(working_directory, None).map_err(PromptError::Context)?;
        let configuration_directory =
            normalize_skill_path(configuration_directory, None).map_err(PromptError::Context)?;
        Ok(Self {
            working_directory: working_directory.into(),
            configuration_directory: configuration_directory.into(),
            reader,
            security,
            templates,
        })
    }
}
impl<R, P> SystemPromptProvider for AgentsPromptProvider<R, P>
where
    R: AuthorizedReadService + 'static,
    P: SecurityProvider<Snapshot = R::Security> + 'static,
    R::Security: 'static,
{
    fn key(&self) -> &str {
        "runtime:system-context:07a-agents"
    }
    fn materialize(&self, _: &AgentIdentityDescription) -> PromptResult<Box<dyn SystemPrompt>> {
        Ok(Box::new(AgentsPrompt {
            working_directory: self.working_directory.clone(),
            configuration_directory: self.configuration_directory.clone(),
            reader: self.reader.clone(),
            security: self.security.clone(),
            templates: self.templates.clone(),
            sampled: None,
        }))
    }
}

struct AgentsPrompt<R: AuthorizedReadService, P: SecurityProvider<Snapshot = R::Security>> {
    working_directory: PathBuf,
    configuration_directory: PathBuf,
    reader: Arc<R>,
    security: Arc<P>,
    templates: Arc<PromptTemplateCatalog>,
    sampled: Option<String>,
}
impl<R: AuthorizedReadService, P: SecurityProvider<Snapshot = R::Security>> AgentsPrompt<R, P> {
    async fn file(
        &self,
        path: PathBuf,
        snapshot: &R::Security,
        cancellation: &dyn Cancellation,
    ) -> PromptResult<Option<(PathBuf, String)>> {
        let metadata = match self
            .reader
            .inspect(&path, LinkPolicy::Follow, snapshot, cancellation)
            .await
        {
            Ok(metadata) => metadata,
            Err(OperationError::Failed(error)) if error.kind == FileReadErrorKind::NotFound => {
                return Ok(None);
            }
            Err(error) => return Err(read_error(error)),
        };
        // File.Exists is false for a directory/non-file. Authorization failures
        // are not reclassified as missing and cannot trigger a fallback read.
        if metadata.kind != EntryKind::Regular {
            return Ok(None);
        }
        let request = ReadRequest {
            requested: path.clone(),
            expected_physical: Some(metadata.physical_path),
            links: LinkPolicy::Follow,
            max_bytes: None,
        };
        let read = self
            .reader
            .read(&request, snapshot, cancellation)
            .await
            .map_err(read_error)?;
        // File.ReadAllText recognizes UTF BOMs and replaces invalid code units.
        Ok(Some((path, decode_agents_text(&read.bytes))))
    }
}
impl<R, P> SystemPrompt for AgentsPrompt<R, P>
where
    R: AuthorizedReadService,
    P: SecurityProvider<Snapshot = R::Security>,
{
    fn renew_epoch(&mut self) {
        self.sampled = None;
    }
    fn sample_epoch<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, PromptResult<()>> {
        Box::pin(async move {
            cancellation.check().map_err(|_| PromptError::Cancelled)?;
            let snapshot = self.security.capture();
            let mut found = Vec::new();
            for directory in self.working_directory.ancestors() {
                if let Some(file) = self
                    .file(directory.join("AGENTS.md"), &snapshot, cancellation)
                    .await?
                {
                    found.push(file);
                }
            }
            found.reverse();
            let global = self.configuration_directory.join("AGENTS.md");
            if !found.iter().any(|(path, _)| path == &global) {
                if let Some(file) = self.file(global, &snapshot, cancellation).await? {
                    found.insert(0, file);
                }
            }
            let mut sections = Vec::new();
            for (path, content) in found {
                sections.push(render(
                    &self.templates,
                    "context.agents-file",
                    &[
                        ("path".into(), path.to_string_lossy().into_owned().into()),
                        ("content".into(), content.into()),
                    ],
                )?);
            }
            self.sampled = Some(sections.join("\n\n"));
            Ok(())
        })
    }
    fn build(&mut self, _: &ResolvedModelSelection) -> PromptResult<String> {
        self.sampled.clone().ok_or_else(|| {
            PromptError::Context("The system context has not been sampled for this epoch.".into())
        })
    }
}

pub(crate) fn read_error(error: OperationError<FileReadError>) -> PromptError {
    match error {
        OperationError::Cancelled => PromptError::Cancelled,
        OperationError::Failed(error) => PromptError::Read(error),
    }
}

pub(crate) fn decode_agents_text(bytes: &[u8]) -> String {
    if let Some(bytes) = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]) {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    if let Some(bytes) = bytes.strip_prefix(&[0xff, 0xfe, 0x00, 0x00]) {
        return bytes
            .chunks(4)
            .map(|chunk| {
                if let Ok(chunk) = <[u8; 4]>::try_from(chunk) {
                    char::from_u32(u32::from_le_bytes(chunk)).unwrap_or(char::REPLACEMENT_CHARACTER)
                } else {
                    char::REPLACEMENT_CHARACTER
                }
            })
            .collect();
    }
    if let Some(bytes) = bytes.strip_prefix(&[0x00, 0x00, 0xfe, 0xff]) {
        return bytes
            .chunks(4)
            .map(|chunk| {
                if let Ok(chunk) = <[u8; 4]>::try_from(chunk) {
                    char::from_u32(u32::from_be_bytes(chunk)).unwrap_or(char::REPLACEMENT_CHARACTER)
                } else {
                    char::REPLACEMENT_CHARACTER
                }
            })
            .collect();
    }
    for (prefix, little_endian) in [([0xff, 0xfe], true), ([0xfe, 0xff], false)] {
        if let Some(bytes) = bytes.strip_prefix(&prefix) {
            let mut text = String::from_utf16_lossy(
                &bytes
                    .chunks_exact(2)
                    .map(|chunk| {
                        if little_endian {
                            u16::from_le_bytes([chunk[0], chunk[1]])
                        } else {
                            u16::from_be_bytes([chunk[0], chunk[1]])
                        }
                    })
                    .collect::<Vec<_>>(),
            );
            if bytes.len() % 2 != 0 {
                text.push(char::REPLACEMENT_CHARACTER);
            }
            return text;
        }
    }
    String::from_utf8_lossy(bytes).into_owned()
}
