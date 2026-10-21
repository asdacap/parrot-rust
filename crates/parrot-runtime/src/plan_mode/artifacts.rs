use super::{ModeError, ModeResult};
use parrot_foundation::paths::AgentScratchDirectory;
use std::{
    fmt::Write as _,
    fs::{self, File, OpenOptions},
    io::{self, Read},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// A token source cannot supply a path. The store always formats the sixteen
/// bytes as one lowercase GUID-shaped name under its selected plan directory.
pub trait PlanTokenSource: Send + Sync {
    fn next_token(&self) -> io::Result<[u8; 16]>;
}
#[derive(Default)]
pub struct SystemPlanTokens;
impl PlanTokenSource for SystemPlanTokens {
    fn next_token(&self) -> io::Result<[u8; 16]> {
        let mut bytes = [0_u8; 16];
        File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        Ok(bytes)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactPaths {
    plan: PathBuf,
    tasks: PathBuf,
}
impl ArtifactPaths {
    pub fn plan(&self) -> &Path {
        &self.plan
    }
    pub fn tasks(&self) -> &Path {
        &self.tasks
    }
}

pub(crate) struct LoadedArtifacts {
    pub paths: ArtifactPaths,
    pub markdown: String,
    pub task_json: String,
}
pub(crate) struct ReadFailure {
    pub paths: ArtifactPaths,
    pub error: io::Error,
}

/// Concrete ownership of only the source's selected-scratch plan lifecycle.
/// It is not a general filesystem backend, an arbitrary-path writer, a user
/// permission grant, or proof of P05's physical authorization/race boundary.
pub struct PlanArtifactStore {
    scratch: AgentScratchDirectory,
    tokens: Arc<dyn PlanTokenSource>,
    pair: Mutex<Option<ArtifactPaths>>,
}
impl PlanArtifactStore {
    pub fn new(scratch: AgentScratchDirectory) -> Self {
        Self::with_tokens(scratch, Arc::new(SystemPlanTokens))
    }
    pub fn with_tokens(scratch: AgentScratchDirectory, tokens: Arc<dyn PlanTokenSource>) -> Self {
        Self {
            scratch,
            tokens,
            pair: Mutex::new(None),
        }
    }
    pub fn scratch(&self) -> &AgentScratchDirectory {
        &self.scratch
    }
    pub fn plan_directory(&self) -> PathBuf {
        self.scratch.plan_directory()
    }
    pub fn paths(&self) -> Option<ArtifactPaths> {
        self.pair.lock().expect("plan artifacts poisoned").clone()
    }
    pub fn prepare(&self) -> ModeResult<()> {
        let mut pair = self.pair.lock().expect("plan artifacts poisoned");
        if let Some(paths) = pair.as_ref() {
            self.secure_artifact(paths.plan())?;
            self.secure_artifact(paths.tasks())?;
            return Ok(());
        }
        self.scratch.provision_plan_directory().map_err(|error| {
            ModeError::Registry(format!("mode: create plan directory: {error}"))
        })?;
        self.secure_directory().map_err(ModeError::io)?;
        let token = self.tokens.next_token().map_err(ModeError::io)?;
        let token = token
            .iter()
            .fold(String::with_capacity(32), |mut text, byte| {
                write!(text, "{byte:02x}").expect("writing to String cannot fail");
                text
            });
        let paths = ArtifactPaths {
            plan: self.plan_directory().join(format!("plan-{token}.md")),
            tasks: self
                .plan_directory()
                .join(format!("plan-{token}.tasks.json")),
        };
        let mut created_plan = false;
        let mut created_tasks = false;
        let created = (|| {
            let _plan = create_private(paths.plan()).map_err(ModeError::io)?;
            created_plan = true;
            let _tasks = create_private(paths.tasks()).map_err(ModeError::io)?;
            created_tasks = true;
            self.secure_artifact(paths.plan())?;
            self.secure_artifact(paths.tasks())?;
            Ok::<_, ModeError>(())
        })();
        if let Err(failure) = created {
            // Explicit ownership correction: unlike the source's unconditional
            // Delete calls, never delete an earlier occupant after CreateNew
            // reports a collision. Only this operation's created files belong
            // to its cleanup. Keep the source plan-then-tasks cleanup order.
            for (path, created) in [(paths.plan(), created_plan), (paths.tasks(), created_tasks)] {
                if created {
                    if let Err(cleanup) = fs::remove_file(path) {
                        if cleanup.kind() != io::ErrorKind::NotFound {
                            return Err(ModeError::Registry(format!(
                                "mode: create and secure correlated plan files: {failure}; cleanup: {cleanup}"
                            )));
                        }
                    }
                }
            }
            return Err(ModeError::Registry(format!(
                "mode: create and secure correlated plan files: {failure}"
            )));
        }
        *pair = Some(paths);
        Ok(())
    }
    pub(crate) fn read(&self) -> Result<Option<LoadedArtifacts>, ReadFailure> {
        let pair = self.pair.lock().expect("plan artifacts poisoned");
        let Some(paths) = pair.as_ref() else {
            return Ok(None);
        };
        let load = || -> io::Result<(String, String)> {
            let markdown = crate::prompt_context::decode_source_text(&fs::read(paths.plan())?);
            let task_json = crate::prompt_context::decode_source_text(&fs::read(paths.tasks())?);
            Ok((markdown.trim().to_owned(), task_json.trim().to_owned()))
        };
        let (markdown, task_json) = load().map_err(|error| ReadFailure {
            paths: paths.clone(),
            error,
        })?;
        Ok(Some(LoadedArtifacts {
            paths: paths.clone(),
            markdown,
            task_json,
        }))
    }
    fn secure_directory(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(self.plan_directory(), fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
    fn secure_artifact(&self, path: &Path) -> ModeResult<()> {
        self.secure_directory().map_err(ModeError::io)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|error| {
                ModeError::Registry(format!("mode: make plan file writable: {error}"))
            })?;
        }
        Ok(())
    }
}
fn create_private(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}
