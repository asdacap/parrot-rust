//! Source skill discovery and catalog state over inseparable authorized I/O.
//! Returned paths and cached metadata are observations, never read capabilities.

use super::skills_parsing::{
    SkillDisplayMetadata, SkillMetadata, SkillRoot, SkillScope, normalize_skill_path,
    parse_skill_display_metadata, parse_skill_frontmatter,
};
use parrot_domain::{
    contracts::{
        async_ops::{Cancellation, OperationError, OperationResult},
        filesystem::{
            AuthorizedReadService, EntryKind, FileMetadata, FileReadError, FileReadErrorKind,
            LinkPolicy, ReadRequest,
        },
    },
    values::{
        config::{PathPlatform, PlatformPath, SkillConfiguration},
        project_workspace::ProjectWorkspace,
    },
};
use std::{
    collections::{HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::Mutex;

const MAXIMUM_DEPTH: usize = 6;
const MAXIMUM_DIRECTORIES: usize = 2000;
const MAXIMUM_ENTRIES: usize = 20_000;
pub const MAXIMUM_SKILL_FILE_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillLoadError {
    pub path: String,
    pub message: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SkillSnapshot {
    pub skills: Vec<SkillMetadata>,
    pub errors: Vec<SkillLoadError>,
}

/// An explicit path base and platform, not host discovery or filesystem authority.
#[derive(Clone, Debug)]
pub struct SkillDiscoveryContext {
    pub working_directory: PathBuf,
    pub platform: PathPlatform,
}
impl SkillDiscoveryContext {
    fn lexical(&self, path: &Path) -> Result<String, FileReadError> {
        let value = unicode(path)?;
        let base = unicode(&self.working_directory)?;
        normalize_skill_path(value, Some(base)).map_err(invalid_path)
    }

    fn physical(&self, path: &Path) -> Result<String, FileReadError> {
        let path = PlatformPath::normalize(
            unicode(path)?,
            unicode(&self.working_directory)?,
            self.platform,
        )
        .map_err(|error| invalid_path(error.to_string()))?;
        Ok(trim_separator(path))
    }
}

fn unicode(path: &Path) -> Result<&str, FileReadError> {
    path.to_str()
        .ok_or_else(|| invalid_path("skill path is not valid Unicode"))
}
fn invalid_path(message: impl Into<String>) -> FileReadError {
    FileReadError::new(FileReadErrorKind::InvalidPath, message)
}
fn trim_separator(path: String) -> String {
    if path == "/" {
        path
    } else {
        path.trim_end_matches('/').to_owned()
    }
}
fn boundary_error(error: FileReadError) -> OperationError<FileReadError> {
    OperationError::Failed(error)
}
fn caught_io(error: &FileReadError) -> bool {
    error.is_io() || error.kind == FileReadErrorKind::Denied
}
fn caught_skill(error: &FileReadError) -> bool {
    caught_io(error) || error.kind == FileReadErrorKind::InvalidOperation
}
fn check_cancel(cancellation: &dyn Cancellation) -> OperationResult<(), FileReadError> {
    cancellation.check().map_err(|_| OperationError::Cancelled)
}
fn add_error(errors: &mut Vec<SkillLoadError>, path: String, message: &str) {
    let normalized = message.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut truncated = String::new();
    let mut units = 0;
    for character in normalized.chars() {
        if units + character.len_utf16() > 1024 {
            break;
        }
        truncated.push(character);
        units += character.len_utf16();
    }
    errors.push(SkillLoadError {
        path,
        message: if truncated.is_empty() {
            "Skill discovery failed.".into()
        } else {
            truncated
        },
    });
}

/// Reads through the injected service on every use, binding the opened target to
/// the discovered physical path. This does not reopen metadata with `std::fs`.
pub async fn read_skill_file<R: AuthorizedReadService + ?Sized>(
    service: &R,
    lexical: &Path,
    expected_physical: &Path,
    snapshot: &R::Security,
    cancellation: &dyn Cancellation,
) -> OperationResult<String, FileReadError> {
    check_cancel(cancellation)?;
    let request = ReadRequest {
        requested: lexical.into(),
        expected_physical: Some(expected_physical.into()),
        links: LinkPolicy::Follow,
        max_bytes: Some(MAXIMUM_SKILL_FILE_BYTES),
    };
    let read = service.read(&request, snapshot, cancellation).await?;
    if read.bytes.len() > MAXIMUM_SKILL_FILE_BYTES {
        return Err(boundary_error(FileReadError::new(
            FileReadErrorKind::TooLarge,
            format!(
                "Skill source '{}' exceeds the 1 MiB limit.",
                lexical.display()
            ),
        )));
    }
    String::from_utf8(read.bytes).map_err(|_| {
        boundary_error(FileReadError::new(
            FileReadErrorKind::InvalidData,
            format!("Skill source '{}' is not valid UTF-8.", lexical.display()),
        ))
    })
}

#[derive(Clone)]
struct DirectoryVisit {
    logical: PathBuf,
    physical: PathBuf,
    depth: usize,
}

struct Discovery<'a, R: AuthorizedReadService + ?Sized> {
    service: &'a R,
    configuration: &'a SkillConfiguration,
    context: &'a SkillDiscoveryContext,
    snapshot: &'a R::Security,
    cancellation: &'a dyn Cancellation,
    result: SkillSnapshot,
    canonical_paths: HashSet<String>,
}

pub async fn discover_skills<R: AuthorizedReadService + ?Sized>(
    service: &R,
    roots: &[SkillRoot],
    configuration: &SkillConfiguration,
    context: &SkillDiscoveryContext,
    snapshot: &R::Security,
    cancellation: &dyn Cancellation,
) -> OperationResult<SkillSnapshot, FileReadError> {
    let mut discovery = Discovery {
        service,
        configuration,
        context,
        snapshot,
        cancellation,
        result: SkillSnapshot::default(),
        canonical_paths: HashSet::new(),
    };
    for root in roots {
        check_cancel(cancellation)?;
        discovery.root(root).await?;
    }
    Ok(discovery.result)
}

impl<R: AuthorizedReadService + ?Sized> Discovery<'_, R> {
    async fn root(&mut self, root: &SkillRoot) -> OperationResult<(), FileReadError> {
        let logical = self
            .context
            .lexical(Path::new(&root.path))
            .map_err(boundary_error)?;
        let path = Path::new(&logical);
        let entry = match self
            .service
            .inspect(path, LinkPolicy::NoFollow, self.snapshot, self.cancellation)
            .await
        {
            Ok(entry) => entry,
            Err(OperationError::Failed(error)) if error.kind == FileReadErrorKind::NotFound => {
                return Ok(());
            }
            Err(OperationError::Failed(error)) if caught_io(&error) => {
                add_error(&mut self.result.errors, logical, &error.message);
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        if entry.kind == EntryKind::SymbolicLink && !root.follow_directory_links {
            add_error(
                &mut self.result.errors,
                logical,
                "System skill roots cannot be symbolic links.",
            );
            return Ok(());
        }
        // Source checks the final root link separately before resolving its
        // physical path, including links in ancestor components.
        let resolved = match self
            .service
            .inspect(path, LinkPolicy::Follow, self.snapshot, self.cancellation)
            .await
        {
            Ok(entry) => entry,
            Err(OperationError::Failed(error)) if caught_io(&error) => {
                add_error(&mut self.result.errors, logical, &error.message);
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let physical = self
            .context
            .physical(&resolved.physical_path)
            .map_err(boundary_error)?;
        let mut pending = VecDeque::from([DirectoryVisit {
            logical: logical.clone().into(),
            physical: physical.into(),
            depth: 0,
        }]);
        let mut visited = HashSet::new();
        let mut directories = 0;
        let mut entries_seen = 0;
        while let Some(current) = pending.pop_front() {
            check_cancel(self.cancellation)?;
            if !visited.insert(
                self.context
                    .physical(&current.physical)
                    .map_err(boundary_error)?,
            ) {
                continue;
            }
            directories += 1;
            if directories > MAXIMUM_DIRECTORIES {
                add_error(
                    &mut self.result.errors,
                    logical.clone(),
                    "Skill discovery exceeded 2000 directories.",
                );
                return Ok(());
            }
            let mut entries = match self
                .service
                .list_directory(
                    &current.logical,
                    LinkPolicy::Follow,
                    self.snapshot,
                    self.cancellation,
                )
                .await
            {
                Ok(entries) => entries,
                Err(OperationError::Failed(error)) if caught_io(&error) => {
                    add_error(
                        &mut self.result.errors,
                        self.context
                            .lexical(&current.logical)
                            .map_err(boundary_error)?,
                        &error.message,
                    );
                    continue;
                }
                Err(error) => return Err(error),
            };
            entries.sort_by(|a, b| {
                a.lexical_path
                    .to_string_lossy()
                    .encode_utf16()
                    .cmp(b.lexical_path.to_string_lossy().encode_utf16())
            });
            for listed in entries {
                check_cancel(self.cancellation)?;
                entries_seen += 1;
                if entries_seen > MAXIMUM_ENTRIES {
                    add_error(
                        &mut self.result.errors,
                        logical.clone(),
                        "Skill discovery exceeded 20000 entries.",
                    );
                    return Ok(());
                }
                self.entry(root, &current, &listed.lexical_path, &mut pending)
                    .await?;
            }
        }
        Ok(())
    }

    async fn entry(
        &mut self,
        root: &SkillRoot,
        parent: &DirectoryVisit,
        path: &Path,
        pending: &mut VecDeque<DirectoryVisit>,
    ) -> OperationResult<(), FileReadError> {
        let entry = match self
            .service
            .inspect(path, LinkPolicy::NoFollow, self.snapshot, self.cancellation)
            .await
        {
            Ok(entry) => entry,
            Err(OperationError::Failed(error)) if caught_io(&error) => {
                add_error(
                    &mut self.result.errors,
                    self.context.lexical(path).map_err(boundary_error)?,
                    &error.message,
                );
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        match entry.kind {
            EntryKind::SymbolicLink
                if root.follow_directory_links && parent.depth < MAXIMUM_DEPTH =>
            {
                let target = match self
                    .service
                    .inspect(path, LinkPolicy::Follow, self.snapshot, self.cancellation)
                    .await
                {
                    Ok(target) => target,
                    Err(OperationError::Failed(error)) if caught_io(&error) => {
                        add_error(
                            &mut self.result.errors,
                            self.context.lexical(path).map_err(boundary_error)?,
                            &error.message,
                        );
                        return Ok(());
                    }
                    Err(error) => return Err(error),
                };
                if target.kind == EntryKind::Directory && !hidden(&entry) {
                    pending.push_back(DirectoryVisit {
                        logical: path.into(),
                        physical: self
                            .context
                            .physical(&target.physical_path)
                            .map_err(boundary_error)?
                            .into(),
                        depth: parent.depth + 1,
                    });
                }
            }
            EntryKind::Directory if parent.depth < MAXIMUM_DEPTH && !hidden(&entry) => {
                let Some(name) = path.file_name() else {
                    return Ok(());
                };
                pending.push_back(DirectoryVisit {
                    logical: path.into(),
                    physical: parent.physical.join(name),
                    depth: parent.depth + 1,
                });
            }
            EntryKind::Regular if path.file_name().is_some_and(|name| name == "SKILL.md") => {
                self.load(root, parent, path).await?
            }
            _ => {}
        }
        Ok(())
    }

    async fn display(
        &mut self,
        directory: &DirectoryVisit,
    ) -> OperationResult<SkillDisplayMetadata, FileReadError> {
        let logical = directory.logical.join("agents/openai.yaml");
        let result = self
            .service
            .inspect(
                &logical,
                LinkPolicy::Follow,
                self.snapshot,
                self.cancellation,
            )
            .await;
        let physical = match result {
            Ok(metadata) if metadata.kind == EntryKind::Regular => self
                .context
                .physical(&metadata.physical_path)
                .map_err(boundary_error)?,
            Ok(_) => return Ok(SkillDisplayMetadata::default()),
            Err(OperationError::Failed(error)) if error.kind == FileReadErrorKind::NotFound => {
                return Ok(SkillDisplayMetadata::default());
            }
            Err(OperationError::Failed(error)) if caught_skill(&error) => {
                add_error(
                    &mut self.result.errors,
                    self.context.lexical(&logical).map_err(boundary_error)?,
                    &error.message,
                );
                return Ok(SkillDisplayMetadata::default());
            }
            Err(error) => return Err(error),
        };
        match read_skill_file(
            self.service,
            &logical,
            Path::new(&physical),
            self.snapshot,
            self.cancellation,
        )
        .await
        {
            Ok(content) => Ok(parse_skill_display_metadata(&content)),
            Err(OperationError::Failed(error)) if caught_skill(&error) => {
                add_error(
                    &mut self.result.errors,
                    self.context.lexical(&logical).map_err(boundary_error)?,
                    &error.message,
                );
                Ok(SkillDisplayMetadata::default())
            }
            Err(error) => Err(error),
        }
    }

    async fn load(
        &mut self,
        root: &SkillRoot,
        directory: &DirectoryVisit,
        lexical: &Path,
    ) -> OperationResult<(), FileReadError> {
        let canonical = self
            .context
            .physical(&directory.physical.join("SKILL.md"))
            .map_err(boundary_error)?;
        if self.canonical_paths.contains(&canonical) {
            return Ok(());
        }
        let content = match read_skill_file(
            self.service,
            lexical,
            Path::new(&canonical),
            self.snapshot,
            self.cancellation,
        )
        .await
        {
            Ok(content) => content,
            Err(OperationError::Failed(error)) if caught_skill(&error) => {
                add_error(
                    &mut self.result.errors,
                    self.context.lexical(lexical).map_err(boundary_error)?,
                    &error.message,
                );
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let display = self.display(directory).await?;
        let mut parsed = match parse_skill_frontmatter(
            &canonical,
            &content,
            root.scope,
            display.allow_implicit_invocation.unwrap_or(true),
        ) {
            Ok(parsed) => parsed,
            Err(error) => {
                add_error(
                    &mut self.result.errors,
                    self.context.lexical(lexical).map_err(boundary_error)?,
                    &error.message,
                );
                return Ok(());
            }
        };
        parsed.display_name = display.display_name;
        parsed.short_description = parsed.short_description.or(display.short_description);
        parsed.enabled = self
            .configuration
            .is_enabled(
                &canonical,
                unicode(&self.context.working_directory).map_err(boundary_error)?,
                self.context.platform,
            )
            .map_err(|error| boundary_error(invalid_path(error.to_string())))?;
        parsed.discovery_path = self.context.lexical(lexical).map_err(boundary_error)?;
        self.canonical_paths.insert(canonical);
        self.result.skills.push(parsed);
        Ok(())
    }
}

fn hidden(entry: &FileMetadata) -> bool {
    entry.hidden
        || entry
            .lexical_path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with('.'))
}

/// Source root order is nearest repository ancestor first, then user, then
/// packaged. A non-repository launch never scans launch ancestors.
pub fn resolve_skill_roots(
    workspace: &ProjectWorkspace,
    user_home: &Path,
    packaged_root: &Path,
    platform: PathPlatform,
) -> Result<Vec<SkillRoot>, FileReadError> {
    let context = SkillDiscoveryContext {
        working_directory: workspace.launch_directory().into(),
        platform,
    };
    let mut roots = Vec::new();
    let mut paths = HashSet::new();
    let mut add = |path: PathBuf, scope, follow_directory_links| -> Result<(), FileReadError> {
        let path = context.lexical(&path)?;
        if paths.insert(path.clone()) {
            roots.push(SkillRoot {
                path,
                scope,
                follow_directory_links,
            });
        }
        Ok(())
    };
    if let Some(repository) = workspace.repository_root() {
        let repository_normalized = context.physical(repository)?;
        let launch = context.physical(workspace.launch_directory())?;
        let start = if Path::new(&launch).starts_with(&repository_normalized) {
            workspace.launch_directory()
        } else {
            workspace.physical_identity()
        };
        let mut current = PathBuf::from(context.lexical(start)?);
        loop {
            let normalized = context.physical(&current)?;
            if !Path::new(&normalized).starts_with(&repository_normalized) {
                break;
            }
            add(current.join(".agents/skills"), SkillScope::Repo, true)?;
            if normalized == repository_normalized {
                break;
            }
            let Some(parent) = current.parent() else {
                break;
            };
            current = parent.into();
        }
    }
    if !unicode(user_home)?.trim().is_empty() {
        add(user_home.join(".agents/skills"), SkillScope::User, true)?;
    }
    add(packaged_root.into(), SkillScope::System, false)?;
    Ok(roots)
}

pub type SkillConfigurationSource = Arc<dyn Fn() -> (SkillConfiguration, i64) + Send + Sync>;
pub type SkillConfigurationWriter =
    Arc<dyn Fn(&str, bool) -> Result<(), FileReadError> + Send + Sync>;

#[derive(Default)]
struct CatalogState {
    generation: i32,
    captured_generation: Option<i32>,
    configuration_generation: Option<i64>,
    snapshot: Option<Arc<SkillSnapshot>>,
}

pub struct SkillCatalog<R: AuthorizedReadService + ?Sized> {
    service: Arc<R>,
    roots: Vec<SkillRoot>,
    context: SkillDiscoveryContext,
    configuration: SkillConfigurationSource,
    configure: Option<SkillConfigurationWriter>,
    state: Mutex<CatalogState>,
}
impl<R: AuthorizedReadService + ?Sized> SkillCatalog<R> {
    pub fn new(
        service: Arc<R>,
        roots: Vec<SkillRoot>,
        context: SkillDiscoveryContext,
        configuration: SkillConfigurationSource,
    ) -> Self {
        Self {
            service,
            roots,
            context,
            configuration,
            configure: None,
            state: Mutex::new(CatalogState::default()),
        }
    }
    pub fn with_configuration(mut self, configure: SkillConfigurationWriter) -> Self {
        self.configure = Some(configure);
        self
    }
    pub fn roots(&self) -> &[SkillRoot] {
        &self.roots
    }
    pub fn service(&self) -> &R {
        &self.service
    }
    pub fn context(&self) -> &SkillDiscoveryContext {
        &self.context
    }
    pub async fn capture(
        &self,
        security: &R::Security,
        cancellation: &dyn Cancellation,
    ) -> OperationResult<Arc<SkillSnapshot>, FileReadError> {
        check_cancel(cancellation)?;
        let mut state = self.state.lock().await;
        check_cancel(cancellation)?;
        let (configuration, generation) = (self.configuration)();
        if state.captured_generation == Some(state.generation)
            && state.configuration_generation == Some(generation)
        {
            if let Some(snapshot) = &state.snapshot {
                return Ok(snapshot.clone());
            }
        }
        let snapshot = Arc::new(
            discover_skills(
                self.service.as_ref(),
                &self.roots,
                &configuration,
                &self.context,
                security,
                cancellation,
            )
            .await?,
        );
        state.captured_generation = Some(state.generation);
        state.configuration_generation = Some(generation);
        state.snapshot = Some(snapshot.clone());
        Ok(snapshot)
    }
    pub async fn invalidate(&self) {
        let mut state = self.state.lock().await;
        state.generation = state.generation.wrapping_add(1);
    }
    pub async fn refresh(
        &self,
        security: &R::Security,
        cancellation: &dyn Cancellation,
    ) -> OperationResult<Arc<SkillSnapshot>, FileReadError> {
        self.invalidate().await;
        self.capture(security, cancellation).await
    }
    pub async fn configure(
        &self,
        path: &str,
        enabled: bool,
        security: &R::Security,
        cancellation: &dyn Cancellation,
    ) -> OperationResult<SkillMetadata, FileReadError> {
        if path.trim().is_empty() {
            return Err(boundary_error(invalid_path("skill path cannot be blank")));
        }
        let equal = |left: &str, right: &str| match (
            self.context.lexical(Path::new(left)),
            self.context.lexical(Path::new(right)),
        ) {
            (Ok(left), Ok(right)) => left == right,
            _ => false,
        };
        let first = self.refresh(security, cancellation).await?;
        let skill = first
            .skills
            .iter()
            .find(|skill| equal(&skill.path, path))
            .ok_or_else(|| {
                boundary_error(FileReadError::new(
                    FileReadErrorKind::NotFound,
                    "skill path is not present in this session catalog",
                ))
            })?;
        let configure = self.configure.as_ref().ok_or_else(|| {
            boundary_error(FileReadError::new(
                FileReadErrorKind::InvalidOperation,
                "skill configuration is unavailable",
            ))
        })?;
        configure(&skill.path, enabled).map_err(boundary_error)?;
        let refreshed = self.refresh(security, cancellation).await?;
        refreshed.skills.iter().find(|candidate| equal(&candidate.path, &skill.path)).cloned().ok_or_else(|| boundary_error(FileReadError::new(FileReadErrorKind::NotFound, "skill configuration was persisted, but the skill is no longer present in this session catalog")))
    }
}
