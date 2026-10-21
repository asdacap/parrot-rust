use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// Resolved workspace values, identified by the exact physical-identity spelling.
///
/// Construction performs no filesystem lookup, normalization, or authorization.
/// The owning resolver must establish these values; writable roots are not grants.
#[derive(Clone, Debug)]
pub struct ProjectWorkspace {
    launch_directory: PathBuf,
    physical_identity: PathBuf,
    repository_root: Option<PathBuf>,
    writable_roots: Vec<PathBuf>,
}

impl ProjectWorkspace {
    pub fn from_resolved(
        launch_directory: PathBuf,
        physical_identity: PathBuf,
        repository_root: Option<PathBuf>,
        repository_writable_root: Option<PathBuf>,
    ) -> Self {
        let mut writable_roots: Vec<PathBuf> = Vec::with_capacity(3);
        for root in repository_writable_root
            .into_iter()
            .chain([launch_directory.clone(), physical_identity.clone()])
        {
            if !writable_roots
                .iter()
                .any(|existing| existing.as_os_str() == root.as_os_str())
            {
                writable_roots.push(root);
            }
        }
        Self {
            launch_directory,
            physical_identity,
            repository_root,
            writable_roots,
        }
    }

    pub fn launch_directory(&self) -> &Path {
        &self.launch_directory
    }

    pub fn physical_identity(&self) -> &Path {
        &self.physical_identity
    }

    pub fn is_git_repository(&self) -> bool {
        self.repository_root.is_some()
    }

    pub fn repository_root(&self) -> Option<&Path> {
        self.repository_root.as_deref()
    }

    pub fn writable_roots(&self) -> &[PathBuf] {
        &self.writable_roots
    }
}

impl PartialEq for ProjectWorkspace {
    fn eq(&self, other: &Self) -> bool {
        self.physical_identity.as_os_str() == other.physical_identity.as_os_str()
    }
}

impl Eq for ProjectWorkspace {}

impl Hash for ProjectWorkspace {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.physical_identity.as_os_str().hash(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::collections::hash_map::DefaultHasher;
    use std::ffi::OsString;

    fn workspace(launch: &str, identity: &str) -> ProjectWorkspace {
        ProjectWorkspace::from_resolved(launch.into(), identity.into(), None, None)
    }

    fn roots(workspace: &ProjectWorkspace) -> Vec<OsString> {
        workspace
            .writable_roots()
            .iter()
            .map(|path| path.as_os_str().to_owned())
            .collect()
    }

    #[test]
    fn resolved_values_preserve_launch_spelling_without_filesystem_resolution() {
        let launch = PathBuf::from("unresolved/./launch//");
        let identity = PathBuf::from("unresolved/physical/");
        let repository = PathBuf::from("repository/./root/");
        let value = ProjectWorkspace::from_resolved(
            launch.clone(),
            identity.clone(),
            Some(repository.clone()),
            None,
        );

        assert_eq!(value.launch_directory().as_os_str(), launch.as_os_str());
        assert_eq!(value.physical_identity().as_os_str(), identity.as_os_str());
        assert_eq!(
            value.repository_root().unwrap().as_os_str(),
            repository.as_os_str()
        );
        assert!(value.is_git_repository());
    }

    #[test]
    fn equality_and_hash_depend_only_on_exact_physical_identity() {
        let first = workspace("launch-a", "physical");
        let second = ProjectWorkspace::from_resolved(
            "launch-b".into(),
            "physical".into(),
            Some("repository".into()),
            Some("writable".into()),
        );
        assert_eq!(first, second);
        let hash = |value: &ProjectWorkspace| {
            let mut hasher = DefaultHasher::new();
            value.hash(&mut hasher);
            hasher.finish()
        };
        assert_eq!(hash(&first), hash(&second));
        assert_eq!(HashSet::from([first, second]).len(), 1);
    }

    #[test]
    fn identity_equality_is_ordinal_not_path_component_equality() {
        let value = workspace("launch", "/workspace/repo");
        for identity in ["/workspace/./repo", "/workspace/repo/", "/Workspace/repo"] {
            assert_ne!(value, workspace("launch", identity), "{identity}");
        }
    }

    #[test]
    fn writable_roots_keep_source_order_and_exact_spelling() {
        let value = ProjectWorkspace::from_resolved(
            "/repo/".into(),
            "/repo/.".into(),
            Some("/repository".into()),
            Some("/repo".into()),
        );
        assert_eq!(
            roots(&value),
            ["/repo", "/repo/", "/repo/."].map(OsString::from).to_vec()
        );
    }

    #[test]
    fn writable_roots_deduplicate_only_exact_values() {
        let duplicate = ProjectWorkspace::from_resolved(
            "/repo".into(),
            "/repo".into(),
            Some("/repository".into()),
            Some("/repo".into()),
        );
        assert_eq!(roots(&duplicate), vec![OsString::from("/repo")]);
        let different_case = ProjectWorkspace::from_resolved(
            "/Repo".into(),
            "/repo".into(),
            Some("/repository".into()),
            Some("/repo".into()),
        );
        assert_eq!(
            roots(&different_case),
            vec![OsString::from("/repo"), OsString::from("/Repo")]
        );
    }

    #[test]
    fn repository_status_does_not_require_a_repository_writable_root() {
        let non_git = workspace("launch", "physical");
        assert!(!non_git.is_git_repository());
        assert_eq!(non_git.repository_root(), None);
        assert_eq!(
            roots(&non_git),
            vec![OsString::from("launch"), OsString::from("physical")]
        );
        let git = ProjectWorkspace::from_resolved(
            "launch".into(),
            "physical".into(),
            Some("repository".into()),
            None,
        );
        assert!(git.is_git_repository());
        assert_eq!(git.repository_root(), Some(Path::new("repository")));
        assert_eq!(roots(&git), roots(&non_git));
    }
}
