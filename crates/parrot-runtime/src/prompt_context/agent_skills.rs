use super::{
    AgentIdentityDescription, PromptError, PromptResult, SkillCatalog, SkillMetadata,
    SkillSnapshot, SystemPrompt, SystemPromptProvider, normalize_skill_match_path,
    normalize_skill_path, render_skill_catalog_entries,
};
use parrot_core::SkillCatalogEntry;
use parrot_domain::contracts::{
    async_ops::{BoxFuture, Cancellation, OperationError},
    filesystem::{AuthorizedReadService, FileReadErrorKind, LinkPolicy, ReadRequest},
    security::SecurityProvider,
};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use std::sync::{Arc, RwLock};
use tokio::sync::Mutex;

/// Exposes agent-local catalog records without preparing selected-skill context.
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
    catalog_view: Arc<RwLock<Option<Vec<SkillCatalogEntry>>>>,
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
                root_locations: Vec::new(),
            }),
            catalog_view: Arc::new(RwLock::new(None)),
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
        self.publish_catalog(self.snapshot.clone());
        Ok(())
    }
    async fn ensure_turn(&mut self, cancellation: &dyn Cancellation) -> PromptResult<()> {
        if !self.turn_active {
            self.begin_turn(cancellation).await?;
        }
        Ok(())
    }
    pub async fn build_catalog(&mut self, cancellation: &dyn Cancellation) -> PromptResult<String> {
        let entries = self.read_catalog(cancellation).await?;
        render_skill_catalog_entries(&entries, &self.templates).map(|(_, text)| text)
    }
    pub async fn read_catalog(
        &mut self,
        cancellation: &dyn Cancellation,
    ) -> PromptResult<Vec<SkillCatalogEntry>> {
        let snapshot = if self.turn_active {
            self.snapshot.clone()
        } else {
            let security = self.security.capture();
            self.catalog
                .capture(&security, cancellation)
                .await
                .map_err(super::agents::read_error)?
        };
        Ok(self.publish_catalog(snapshot))
    }
    fn publish_catalog(&mut self, snapshot: Arc<SkillSnapshot>) -> Vec<SkillCatalogEntry> {
        let entries = snapshot
            .skills
            .iter()
            .map(|skill| SkillCatalogEntry {
                name: skill.name.clone(),
                description: skill.description.clone(),
                discovery_path: skill.discovery_path.clone(),
                enabled: skill.enabled,
                prompt_visible: skill.prompt_visible,
            })
            .collect::<Vec<_>>();
        *self
            .catalog_view
            .write()
            .expect("skill catalog view lock poisoned") = Some(entries.clone());
        entries
    }

    pub async fn get_skills(
        &mut self,
        references: &[parrot_core::SkillReference],
        cancellation: &dyn Cancellation,
    ) -> PromptResult<Vec<Option<parrot_core::Skill>>> {
        cancellation.check().map_err(|_| PromptError::Cancelled)?;
        self.ensure_turn(cancellation).await?;
        let working_directory = self.working_directory.clone();
        let platform = self.catalog.context().platform;
        let matches = |candidate: &SkillMetadata, reference: &parrot_core::SkillReference| {
            candidate.name == reference.name
                && reference.path.as_ref().is_none_or(|path| {
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
        if references.iter().any(|reference| {
            !self
                .snapshot
                .skills
                .iter()
                .any(|skill| matches(skill, reference))
        }) {
            let security = self.security.capture();
            self.snapshot = self
                .catalog
                .refresh(&security, cancellation)
                .await
                .map_err(super::agents::read_error)?;
            self.publish_catalog(self.snapshot.clone());
        }
        Ok(references
            .iter()
            .map(|reference| {
                self.snapshot
                    .skills
                    .iter()
                    .find(|skill| skill.enabled && matches(skill, reference))
                    .map(|skill| parrot_core::Skill {
                        id: skill.path.clone(),
                        name: skill.name.clone(),
                        path: skill.discovery_path.clone(),
                    })
            })
            .collect())
    }
    pub async fn read_skill_content(
        &mut self,
        skill: &parrot_core::Skill,
        cancellation: &dyn Cancellation,
    ) -> PromptResult<parrot_core::SkillContent> {
        cancellation.check().map_err(|_| PromptError::Cancelled)?;
        let security = self.security.capture();
        let request = ReadRequest {
            requested: skill.path.clone().into(),
            expected_physical: Some(skill.id.clone().into()),
            links: LinkPolicy::Follow,
            max_bytes: Some(1024 * 1024),
        };
        match self.reader.read(&request, &security, cancellation).await {
            Ok(read) => match std::str::from_utf8(&read.bytes) {
                Ok(content) => Ok(parrot_core::SkillContent::Text(content.to_owned())),
                Err(_) => Ok(parrot_core::SkillContent::Unavailable(format!(
                    "Skill source '{}' is not valid UTF-8.",
                    skill.path
                ))),
            },
            Err(OperationError::Cancelled) => Err(PromptError::Cancelled),
            Err(OperationError::Failed(error))
                if error.is_io()
                    || matches!(
                        error.kind,
                        FileReadErrorKind::Denied | FileReadErrorKind::InvalidOperation
                    ) =>
            {
                Ok(parrot_core::SkillContent::Unavailable(error.message))
            }
            Err(error) => Err(super::agents::read_error(error)),
        }
    }
    pub fn end_turn(&mut self) {
        self.turn_active = false;
        self.snapshot = Arc::new(SkillSnapshot {
            skills: Vec::new(),
            errors: Vec::new(),
            root_locations: Vec::new(),
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
    catalog_view: Arc<RwLock<Option<Vec<SkillCatalogEntry>>>>,
    templates: Arc<PromptTemplateCatalog>,
}
impl<R: AuthorizedReadService + ?Sized, P: SecurityProvider<Snapshot = R::Security>>
    AgentSkillPromptProvider<R, P>
{
    pub async fn new(skills: Arc<Mutex<AgentSkills<R, P>>>) -> Self {
        let guard = skills.lock().await;
        let catalog_view = guard.catalog_view.clone();
        let templates = guard.templates.clone();
        drop(guard);
        Self {
            skills,
            catalog_view,
            templates,
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
            templates: self.templates.clone(),
        }))
    }
}
struct SkillPrompt<R: AuthorizedReadService + ?Sized, P: SecurityProvider<Snapshot = R::Security>> {
    skills: Arc<Mutex<AgentSkills<R, P>>>,
    catalog_view: Arc<RwLock<Option<Vec<SkillCatalogEntry>>>>,
    templates: Arc<PromptTemplateCatalog>,
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
            self.skills.lock().await.read_catalog(cancellation).await?;
            Ok(())
        })
    }
    fn read_skill_catalog_live<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, PromptResult<Vec<SkillCatalogEntry>>> {
        Box::pin(async move {
            cancellation.check().map_err(|_| PromptError::Cancelled)?;
            let mut skills = tokio::select! {
                biased;
                () = cancellation.cancelled() => return Err(PromptError::Cancelled),
                skills = self.skills.lock() => skills,
            };
            cancellation.check().map_err(|_| PromptError::Cancelled)?;
            skills.read_catalog(cancellation).await
        })
    }
    fn read_skill_catalog(&self) -> PromptResult<Vec<SkillCatalogEntry>> {
        self.catalog_view
            .read()
            .expect("skill catalog view lock poisoned")
            .clone()
            .ok_or_else(|| {
                PromptError::Context(
                    "The skill catalogue has not been sampled for this epoch.".into(),
                )
            })
    }
    fn get_template_catalog(&self, source_key: &str) -> Option<&PromptTemplateCatalog> {
        (source_key == "runtime:system-context:09b-skills").then_some(self.templates.as_ref())
    }
}
