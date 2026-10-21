use super::{invalid, native_platform, normalize};
use parrot_domain::contracts::paths::SelectedSessionRoot;
use parrot_domain::values::identifiers::UserSessionId;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug)]
pub struct UserSessionResources<W> {
    selected_root: SelectedSessionRoot,
    id: UserSessionId,
    workspace: W,
    metadata_gate: Mutex<()>,
}

impl<W> UserSessionResources<W> {
    pub fn new(
        selected_root: SelectedSessionRoot,
        id: UserSessionId,
        workspace: W,
    ) -> io::Result<Self> {
        if selected_root
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            != Some(id.value())
        {
            return Err(invalid("session root and user session id must agree"));
        }
        Ok(Self {
            selected_root,
            id,
            workspace,
            metadata_gate: Mutex::new(()),
        })
    }

    pub fn id(&self) -> &UserSessionId {
        &self.id
    }
    pub fn workspace(&self) -> &W {
        &self.workspace
    }
    pub fn selected_root(&self) -> &SelectedSessionRoot {
        &self.selected_root
    }
    pub fn root(&self) -> &Path {
        self.selected_root.path()
    }
    pub fn sessions_directory(&self) -> &Path {
        self.root()
            .parent()
            .expect("a selected session root has a session id component")
    }
    pub fn metadata_gate(&self) -> &Mutex<()> {
        &self.metadata_gate
    }
    pub fn socket_path(&self) -> PathBuf {
        self.root().join("parrot.sock")
    }
    pub fn metadata_path(&self) -> PathBuf {
        self.root().join("meta.json")
    }
    pub fn database_path(&self) -> PathBuf {
        self.root().join("session.db")
    }
    pub fn log_path(&self) -> PathBuf {
        self.root().join("session.log")
    }
    pub fn artifact_directory(&self) -> PathBuf {
        self.root().join("artifacts")
    }
    pub fn queue_directory(&self) -> PathBuf {
        self.root().join("queues")
    }
    pub fn agent_queue_root_directory(&self) -> PathBuf {
        self.queue_directory().join("agents")
    }
    pub fn scratch_directory(&self) -> PathBuf {
        self.root().join("scratch")
    }
    pub fn agents_directory(&self) -> PathBuf {
        self.root().join("root-agents")
    }

    pub fn owns(&self, path: &Path, current_directory: &Path) -> io::Result<bool> {
        Ok(normalize(path, current_directory, native_platform())?.starts_with(self.root()))
    }

    pub fn agent_queue_directory(&self, session_id: &str) -> io::Result<PathBuf> {
        validate_segment(session_id)?;
        Ok(self.agent_queue_root_directory().join(session_id))
    }

    pub fn agent_scratch(&self, name_path: &[&str]) -> io::Result<AgentScratchDirectory> {
        if name_path.is_empty() {
            return Err(invalid(
                "an agent name path must have at least the root agent name",
            ));
        }
        let mut root = self.agents_directory();
        for (index, name) in name_path.iter().enumerate() {
            validate_segment(name)?;
            if index > 0 && AgentScratchDirectory::RESERVED_NAMES.contains(name) {
                return Err(invalid("an agent name is reserved"));
            }
            root.push(name);
        }
        AgentScratchDirectory::new(&root, self.root())
    }
}

fn validate_segment(segment: &str) -> io::Result<()> {
    if segment.is_empty()
        || segment == "."
        || segment == ".."
        || segment.chars().any(char::is_whitespace)
        || segment.contains(['/', '\\', '\0'])
    {
        return Err(invalid("an agent path segment must be one path segment"));
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentScratchDirectory {
    root: PathBuf,
}

impl AgentScratchDirectory {
    pub const RESERVED_NAMES: &'static [&'static str] = &[
        "scratch",
        "history.jsonl",
        "blobs",
        "plan",
        "last_request.json",
    ];

    pub fn new(root: &Path, current_directory: &Path) -> io::Result<Self> {
        let directory = Self {
            root: normalize(root, current_directory, native_platform())?,
        };
        directory.provision()?;
        Ok(directory)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn scratch_path(&self) -> PathBuf {
        self.root.join("scratch")
    }
    pub fn history_path(&self) -> PathBuf {
        self.root.join("history.jsonl")
    }
    pub fn blob_directory(&self) -> PathBuf {
        self.root.join("blobs")
    }
    pub fn plan_directory(&self) -> PathBuf {
        self.root.join("plan")
    }

    pub fn contains(&self, path: &Path, current_directory: &Path) -> io::Result<bool> {
        Ok(normalize(path, current_directory, native_platform())?.starts_with(&self.root))
    }

    pub fn provision(&self) -> io::Result<()> {
        provision_directory(&self.root)?;
        provision_directory(&self.scratch_path())?;
        provision_directory(&self.blob_directory())
    }

    pub fn provision_plan_directory(&self) -> io::Result<()> {
        provision_directory(&self.plan_directory())
    }
}

fn provision_directory(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "agent scratch path must be a directory, not a symbolic link or file",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    fs::create_dir_all(path)?;
    // Check after creation too: an existing/replaced final directory link is not
    // a scratch directory. Operation-level race confinement remains T36-owned.
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "agent scratch directory cannot be a symbolic link",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use parrot_domain::contracts::paths::RootOrigin;

    #[test]
    fn listed_session_main_agent_blob_directory_matches_storage_invariant_line329() {
        // Current proof for pinned 5f27a8a9 StorageInvariantTests.cs:329, not historical authority.
        // Parrot.Infrastructure.Tests::Parrot.Core.Tests.StorageInvariantTests.1.1.Listing_reads_meta_json_and_never_a_database.1.1.0::assertion.line329
        let fixture = std::env::temp_dir().join(format!("parrot-line329-{}", std::process::id()));
        let workspace = fixture.join("work");
        fs::create_dir_all(&workspace).unwrap();
        let fixture = fs::canonicalize(&fixture).unwrap();
        let selected_root =
            SelectedSessionRoot::new(fixture.join("sessions").join("listed"), RootOrigin::Current)
                .unwrap();
        let resources = UserSessionResources::new(
            selected_root,
            UserSessionId::parse("listed").unwrap(),
            fs::canonicalize(workspace).unwrap(),
        )
        .unwrap();

        assert_eq!(
            resources.agent_scratch(&["main"]).unwrap().blob_directory(),
            resources
                .root()
                .join("root-agents")
                .join("main")
                .join("blobs")
        );

        fs::remove_dir_all(fixture).unwrap();
    }
}
