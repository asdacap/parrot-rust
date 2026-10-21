use super::skills_rendering::SelectionRendering;
use super::{
    AgentIdentityDescription, PromptError, PromptResult, SkillCatalog, SkillMetadata,
    SkillSnapshot, SystemPrompt, SystemPromptProvider, normalize_skill_match_path,
    normalize_skill_path, parse_skill_mentions, render_skill_catalog,
};
use parrot_domain::contracts::{
    async_ops::{BoxFuture, Cancellation, OperationError},
    filesystem::{AuthorizedReadService, FileReadErrorKind, LinkPolicy, ReadRequest},
    security::SecurityProvider,
};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use parrot_provider::routing::aliases::ResolvedModelSelection;
use parrot_storage::events::ConversationPart;
use std::{
    collections::HashSet,
    sync::{Arc, RwLock},
};
use tokio::sync::Mutex;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillRendering {
    pub content: String,
    pub loaded_paths: Vec<String>,
}

/// Owns turn-local selection, not authorization. Every I/O invocation receives
/// a fresh immutable snapshot from the injected session security provider.
pub struct AgentSkills<
    R: AuthorizedReadService + ?Sized,
    P: SecurityProvider<Snapshot = R::Security>,
> {
    catalog: Arc<SkillCatalog<R>>,
    reader: Arc<R>,
    security: Arc<P>,
    templates: Arc<PromptTemplateCatalog>,
    working_directory: String,
    snapshot: Arc<SkillSnapshot>,
    catalog_rendering: Option<(Arc<SkillSnapshot>, String)>,
    catalog_view: Arc<RwLock<Option<PromptResult<String>>>>,
    selected: Vec<(SkillMetadata, usize)>,
    selected_paths: HashSet<String>,
    turn_active: bool,
}
impl<R, P> AgentSkills<R, P>
where
    R: AuthorizedReadService + ?Sized,
    P: SecurityProvider<Snapshot = R::Security>,
{
    pub fn new(
        catalog: Arc<SkillCatalog<R>>,
        reader: Arc<R>,
        security: Arc<P>,
        templates: Arc<PromptTemplateCatalog>,
        working_directory: String,
    ) -> PromptResult<Self> {
        let working_directory =
            normalize_skill_path(&working_directory, None).map_err(PromptError::Context)?;
        Ok(Self {
            catalog,
            reader,
            security,
            templates,
            working_directory,
            snapshot: Arc::new(SkillSnapshot {
                skills: Vec::new(),
                errors: Vec::new(),
            }),
            catalog_rendering: None,
            catalog_view: Arc::new(RwLock::new(None)),
            selected: Vec::new(),
            selected_paths: HashSet::new(),
            turn_active: false,
        })
    }
    pub async fn begin_turn(&mut self, cancellation: &dyn Cancellation) -> PromptResult<()> {
        let security = self.security.capture();
        let snapshot = self
            .catalog
            .capture(&security, cancellation)
            .await
            .map_err(super::agents::read_error)?;
        self.turn_active = true;
        self.snapshot = snapshot;
        self.selected.clear();
        self.selected_paths.clear();
        let _ = self.publish_catalog(self.snapshot.clone());
        Ok(())
    }
    async fn ensure_turn(&mut self, cancellation: &dyn Cancellation) -> PromptResult<()> {
        if !self.turn_active {
            self.begin_turn(cancellation).await?;
        }
        Ok(())
    }
    pub async fn build_catalog(&mut self, cancellation: &dyn Cancellation) -> PromptResult<String> {
        let snapshot = if self.turn_active {
            self.snapshot.clone()
        } else {
            let security = self.security.capture();
            self.catalog
                .capture(&security, cancellation)
                .await
                .map_err(super::agents::read_error)?
        };
        self.publish_catalog(snapshot)
    }
    fn publish_catalog(&mut self, snapshot: Arc<SkillSnapshot>) -> PromptResult<String> {
        let rendered = match &self.catalog_rendering {
            Some((previous, text)) if Arc::ptr_eq(previous, &snapshot) => Ok(text.clone()),
            _ => render_skill_catalog(&snapshot.skills, &self.templates),
        };
        if let Ok(text) = &rendered {
            self.catalog_rendering = Some((snapshot, text.clone()));
        }
        *self
            .catalog_view
            .write()
            .expect("skill catalog view lock poisoned") = Some(rendered.clone());
        rendered
    }
    pub async fn select(
        &mut self,
        parts: &[ConversationPart],
        cancellation: &dyn Cancellation,
    ) -> PromptResult<()> {
        self.ensure_turn(cancellation).await?;
        let mut mentions = Vec::new();
        let mut offset = 0;
        for part in parts {
            if let ConversationPart::Text(text) = part {
                mentions.extend(parse_skill_mentions(text).into_iter().map(|mut mention| {
                    mention.position += offset;
                    mention
                }));
                offset += text.encode_utf16().count();
            }
        }
        let working_directory = self.working_directory.clone();
        let platform = self.catalog.context().platform;
        let matches = |candidate: &SkillMetadata, name: &str, path: &Option<String>| {
            candidate.name == name
                && path.as_ref().is_none_or(|path| {
                    let Ok(path) =
                        normalize_skill_match_path(path, Some(&working_directory), platform)
                    else {
                        return false;
                    };
                    [&candidate.path, &candidate.discovery_path]
                        .iter()
                        .any(|candidate| {
                            normalize_skill_match_path(
                                candidate,
                                Some(&working_directory),
                                platform,
                            )
                            .as_ref()
                                == Ok(&path)
                        })
                })
        };
        if mentions.iter().any(|mention| {
            !self
                .snapshot
                .skills
                .iter()
                .any(|skill| matches(skill, &mention.name, &mention.path))
        }) {
            let security = self.security.capture();
            self.snapshot = self
                .catalog
                .refresh(&security, cancellation)
                .await
                .map_err(super::agents::read_error)?;
            let _ = self.publish_catalog(self.snapshot.clone());
        }
        mentions.sort_by_key(|mention| mention.position);
        for mention in mentions {
            if let Some(skill) = self
                .snapshot
                .skills
                .iter()
                .find(|skill| skill.enabled && matches(skill, &mention.name, &mention.path))
            {
                if self.selected_paths.insert(skill.path.clone()) {
                    self.selected.push((skill.clone(), mention.position));
                }
            }
        }
        Ok(())
    }
    pub async fn render(
        &mut self,
        cancellation: &dyn Cancellation,
    ) -> PromptResult<SkillRendering> {
        self.ensure_turn(cancellation).await?;
        if self.selected.is_empty() {
            return Ok(SkillRendering {
                content: String::new(),
                loaded_paths: Vec::new(),
            });
        }
        let security = self.security.capture();
        let mut rendering = SelectionRendering::new(&self.templates)?;
        let mut selected = self.selected.clone();
        selected.sort_by(|(a, ap), (b, bp)| {
            ap.cmp(bp)
                .then_with(|| a.path.encode_utf16().cmp(b.path.encode_utf16()))
        });
        for (skill, _) in selected {
            let request = ReadRequest {
                requested: skill.discovery_path.clone().into(),
                expected_physical: Some(skill.path.clone().into()),
                links: LinkPolicy::Follow,
                max_bytes: Some(1024 * 1024),
            };
            match self.reader.read(&request, &security, cancellation).await {
                Ok(read) => match std::str::from_utf8(&read.bytes) {
                    Ok(content) => rendering.selected(&self.templates, &skill, content)?,
                    Err(_) => rendering.diagnostic(
                        &self.templates,
                        &skill.name,
                        &format!(
                            "Skill source '{}' is not valid UTF-8.",
                            skill.discovery_path
                        ),
                    )?,
                },
                Err(OperationError::Cancelled) => return Err(PromptError::Cancelled),
                Err(OperationError::Failed(error))
                    if error.is_io()
                        || matches!(
                            error.kind,
                            FileReadErrorKind::Denied | FileReadErrorKind::InvalidOperation
                        ) =>
                {
                    rendering.diagnostic(&self.templates, &skill.name, &error.message)?;
                }
                Err(error) => return Err(super::agents::read_error(error)),
            }
        }
        let rendering = rendering.finish(&self.templates)?;
        self.selected.clear();
        Ok(SkillRendering {
            content: rendering.content,
            loaded_paths: rendering.loaded_paths,
        })
    }
    pub fn end_turn(&mut self) {
        self.selected.clear();
        self.selected_paths.clear();
        self.turn_active = false;
        self.snapshot = Arc::new(SkillSnapshot {
            skills: Vec::new(),
            errors: Vec::new(),
        });
        *self
            .catalog_view
            .write()
            .expect("skill catalog view lock poisoned") = None;
    }
}

pub struct AgentSkillPromptProvider<
    R: AuthorizedReadService + ?Sized,
    P: SecurityProvider<Snapshot = R::Security>,
> {
    skills: Arc<Mutex<AgentSkills<R, P>>>,
    catalog_view: Arc<RwLock<Option<PromptResult<String>>>>,
}
impl<R: AuthorizedReadService + ?Sized, P: SecurityProvider<Snapshot = R::Security>>
    AgentSkillPromptProvider<R, P>
{
    pub async fn new(skills: Arc<Mutex<AgentSkills<R, P>>>) -> Self {
        let catalog_view = skills.lock().await.catalog_view.clone();
        Self {
            skills,
            catalog_view,
        }
    }
}
impl<R, P> SystemPromptProvider for AgentSkillPromptProvider<R, P>
where
    R: AuthorizedReadService + ?Sized + 'static,
    P: SecurityProvider<Snapshot = R::Security> + 'static,
    R::Security: 'static,
{
    fn key(&self) -> &str {
        "runtime:system-context:09b-skills"
    }
    fn materialize(&self, _: &AgentIdentityDescription) -> PromptResult<Box<dyn SystemPrompt>> {
        Ok(Box::new(SkillPrompt {
            skills: self.skills.clone(),
            catalog_view: self.catalog_view.clone(),
        }))
    }
}
struct SkillPrompt<R: AuthorizedReadService + ?Sized, P: SecurityProvider<Snapshot = R::Security>> {
    skills: Arc<Mutex<AgentSkills<R, P>>>,
    catalog_view: Arc<RwLock<Option<PromptResult<String>>>>,
}
impl<R: AuthorizedReadService + ?Sized, P: SecurityProvider<Snapshot = R::Security>> SystemPrompt
    for SkillPrompt<R, P>
{
    fn renew_epoch(&mut self) {}
    fn sample_epoch<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, PromptResult<()>> {
        Box::pin(async move {
            self.skills.lock().await.build_catalog(cancellation).await?;
            Ok(())
        })
    }
    fn build_live<'a>(
        &'a mut self,
        _: &'a ResolvedModelSelection,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, PromptResult<String>> {
        Box::pin(async move {
            cancellation.check().map_err(|_| PromptError::Cancelled)?;
            let mut skills = tokio::select! {
                biased;
                () = cancellation.cancelled() => return Err(PromptError::Cancelled),
                skills = self.skills.lock() => skills,
            };
            cancellation.check().map_err(|_| PromptError::Cancelled)?;
            skills.build_catalog(cancellation).await
        })
    }
    fn build(&mut self, _: &ResolvedModelSelection) -> PromptResult<String> {
        self.catalog_view
            .read()
            .expect("skill catalog view lock poisoned")
            .clone()
            .unwrap_or_else(|| {
                Err(PromptError::Context(
                    "The skill catalogue has not been sampled for this epoch.".into(),
                ))
            })
    }
}
