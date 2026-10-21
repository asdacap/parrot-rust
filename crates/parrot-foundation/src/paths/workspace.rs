use super::invalid;
use parrot_domain::values::ProjectWorkspace;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

/// Discovers ordinary workspace identity and repository-root candidates.
///
/// Candidate writable roots are data, not grants. This performs no operation
/// authorization, target confinement, or protection against filesystem races.
pub fn discover_workspace(launch_directory: &Path) -> io::Result<ProjectWorkspace> {
    if launch_directory.as_os_str().is_empty()
        || launch_directory
            .to_str()
            .is_some_and(|value| value.trim().is_empty())
        || !launch_directory.is_absolute()
    {
        return Err(invalid(
            "the project launch directory must be absolute and nonblank",
        ));
    }
    if !launch_directory.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "project launch directory does not exist",
        ));
    }

    let physical_identity = physical_identity(launch_directory)?;
    let repository = find_repository(launch_directory).or_else(|| {
        (launch_directory.as_os_str() != physical_identity.as_os_str())
            .then(|| find_repository(&physical_identity))
            .flatten()
    });
    let (repository_root, repository_candidate) = match repository {
        Some((root, candidate)) => (Some(root), candidate),
        None => (None, None),
    };
    Ok(ProjectWorkspace::from_resolved(
        launch_directory.to_path_buf(),
        physical_identity,
        repository_root,
        repository_candidate,
    ))
}

fn full_path(path: &Path, base: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    let mut result = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            component => result.push(component.as_os_str()),
        }
    }
    if has_ending_separator(&absolute) {
        result.push("");
    }
    result
}

fn has_ending_separator(path: &Path) -> bool {
    path.as_os_str()
        .as_encoded_bytes()
        .last()
        .is_some_and(|byte| *byte == b'/' || (cfg!(windows) && *byte == b'\\'))
}

fn normalize_identity(path: &Path) -> io::Result<PathBuf> {
    let full = full_path(path, path);
    #[cfg(target_os = "macos")]
    let full = super::normalize(
        &full,
        &full,
        parrot_domain::values::config::PathPlatform::MacOs,
    )?;
    // Components remove an ending separator without removing a filesystem root.
    Ok(full.components().collect())
}

fn physical_identity(launch: &Path) -> io::Result<PathBuf> {
    let full = normalize_identity(launch)?;
    let mut current = PathBuf::new();
    for component in full.components() {
        current.push(component.as_os_str());
        if matches!(component, Component::Normal(_))
            && fs::symlink_metadata(&current)?.file_type().is_symlink()
        {
            current = canonical_link_target(&current)?;
        }
    }
    normalize_identity(&current)
}

fn canonical_link_target(path: &Path) -> io::Result<PathBuf> {
    let target = fs::canonicalize(path)?;
    #[cfg(windows)]
    {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        let wide: Vec<_> = target.as_os_str().encode_wide().collect();
        if let Some(ordinary) = ordinary_windows_link_target(&wide) {
            return Ok(std::ffi::OsString::from_wide(&ordinary).into());
        }
    }
    Ok(target)
}

// Only canonicalized ordinary drive/UNC paths acquire these prefixes. Preserve
// other namespaces rather than interpreting device paths as ordinary roots.
#[cfg(any(windows, test))]
fn ordinary_windows_link_target(path: &[u16]) -> Option<Vec<u16>> {
    let suffix = path.strip_prefix(&[92, 92, 63, 92])?;
    if suffix.len() >= 3
        && ((65..=90).contains(&suffix[0]) || (97..=122).contains(&suffix[0]))
        && suffix[1..3] == [58, 92]
    {
        return Some(suffix.to_vec());
    }
    let unc = suffix.strip_prefix(&[85, 78, 67, 92])?;
    let server_end = unc.iter().position(|unit| *unit == 92)?;
    if server_end == 0 || unc.get(server_end + 1).is_none_or(|unit| *unit == 92) {
        return None;
    }
    Some([92, 92].into_iter().chain(unc.iter().copied()).collect())
}

fn read_git_metadata(path: impl AsRef<Path>) -> io::Result<String> {
    let bytes = fs::read(path)?;
    // File.ReadAllText detects these BOMs before decoding. UTF-32 LE must be
    // checked before UTF-16 LE because their first two bytes overlap.
    Ok(match bytes.as_slice() {
        [0xff, 0xfe, 0x00, 0x00, body @ ..] => decode_utf32(body, true),
        [0x00, 0x00, 0xfe, 0xff, body @ ..] => decode_utf32(body, false),
        [0xff, 0xfe, body @ ..] => decode_utf16(body, true),
        [0xfe, 0xff, body @ ..] => decode_utf16(body, false),
        [0xef, 0xbb, 0xbf, body @ ..] => String::from_utf8_lossy(body).into_owned(),
        body => String::from_utf8_lossy(body).into_owned(),
    })
}

fn decode_utf16(bytes: &[u8], little_endian: bool) -> String {
    let mut chunks = bytes.chunks_exact(2);
    let units: Vec<_> = chunks
        .by_ref()
        .map(|chunk| {
            let pair = [chunk[0], chunk[1]];
            if little_endian {
                u16::from_le_bytes(pair)
            } else {
                u16::from_be_bytes(pair)
            }
        })
        .collect();
    let mut text = String::from_utf16_lossy(&units);
    if !chunks.remainder().is_empty() {
        text.push(char::REPLACEMENT_CHARACTER);
    }
    text
}

fn decode_utf32(bytes: &[u8], little_endian: bool) -> String {
    let mut chunks = bytes.chunks_exact(4);
    let mut text: String = chunks
        .by_ref()
        .map(|chunk| {
            let word = [chunk[0], chunk[1], chunk[2], chunk[3]];
            let scalar = if little_endian {
                u32::from_le_bytes(word)
            } else {
                u32::from_be_bytes(word)
            };
            char::from_u32(scalar).unwrap_or(char::REPLACEMENT_CHARACTER)
        })
        .collect();
    if !chunks.remainder().is_empty() {
        text.push(char::REPLACEMENT_CHARACTER);
    }
    text
}

fn find_repository(launch: &Path) -> Option<(PathBuf, Option<PathBuf>)> {
    let directory = full_path(launch, launch);
    // Like the source discovery, an I/O failure means no repository was found.
    find_repository_inner(&directory).ok().flatten()
}

fn find_repository_inner(directory: &Path) -> io::Result<Option<(PathBuf, Option<PathBuf>)>> {
    for directory in directory.ancestors() {
        let git_path = directory.join(".git");
        if git_path.is_dir() {
            return Ok(Some((
                directory.to_path_buf(),
                Some(directory.to_path_buf()),
            )));
        }
        if git_path.is_file() {
            let text = read_git_metadata(&git_path)?;
            let Some(pointer) = text.trim().strip_prefix("gitdir: ") else {
                return Ok(None);
            };
            if pointer.is_empty() {
                return Ok(None);
            }
            let git_directory = full_path(Path::new(pointer), directory);
            return if git_directory.is_dir() {
                Ok(Some((
                    directory.to_path_buf(),
                    linked_repository_root(&git_path, &git_directory)?,
                )))
            } else {
                Ok(None)
            };
        }
    }
    Ok(None)
}

fn linked_repository_root(git_path: &Path, git_directory: &Path) -> io::Result<Option<PathBuf>> {
    let common_path = git_directory.join("commondir");
    let backlink_path = git_directory.join("gitdir");
    if !common_path.is_file() || !backlink_path.is_file() {
        return Ok(None);
    }
    let backlink = full_path(
        Path::new(read_git_metadata(backlink_path)?.trim()),
        git_directory,
    );
    if backlink.as_os_str() != git_path.as_os_str() {
        return Ok(None);
    }
    let common = full_path(
        Path::new(read_git_metadata(common_path)?.trim()),
        git_directory,
    );
    let common: PathBuf = common.components().collect();
    let worktrees = common.join("worktrees");
    if common.is_dir()
        && common.file_name().is_some_and(|name| name == ".git")
        && !has_ending_separator(git_directory)
        && git_directory
            .parent()
            .is_some_and(|parent| parent.as_os_str() == worktrees.as_os_str())
    {
        Ok(common.parent().map(Path::to_path_buf))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "parrot-workspace-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            // Keep an enclosing checkout outside this local discovery fixture.
            fs::write(path.join(".git"), "gitdir: missing").unwrap();
            let workspace = path.join("workspace");
            fs::create_dir(&workspace).unwrap();
            Self(fs::canonicalize(workspace).unwrap())
        }

        fn directory(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::create_dir_all(&path).unwrap();
            path
        }

        fn worktree(&self) -> (PathBuf, PathBuf, PathBuf) {
            let common = self.directory("main/.git");
            let admin = self.directory("main/.git/worktrees/linked");
            let linked = self.directory("linked");
            fs::write(
                linked.join(".git"),
                "gitdir: ../main/.git/worktrees/linked\n",
            )
            .unwrap();
            fs::write(admin.join("commondir"), "../..\n").unwrap();
            fs::write(admin.join("gitdir"), "../../../../linked/.git\n").unwrap();
            (common, admin, linked)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(self.0.parent().unwrap()).unwrap();
        }
    }

    #[test]
    fn workspace_windows_link_target_preserves_ordinary_drive_and_unc_identity() {
        for (extended, ordinary) in [
            (r"\\?\C:\repo", r"C:\repo"),
            (r"\\?\c:\repo\nested", r"c:\repo\nested"),
            (r"\\?\UNC\server\share\repo", r"\\server\share\repo"),
        ] {
            assert_eq!(
                ordinary_windows_link_target(&extended.encode_utf16().collect::<Vec<_>>()),
                Some(ordinary.encode_utf16().collect())
            );
        }
        for special in [
            r"\\?\Volume{example}\repo",
            r"\\?\GLOBALROOT\Device\repo",
            r"\\.\C:\repo",
            r"\\?\C:relative",
            r"\\?\UNC\server",
        ] {
            assert!(
                ordinary_windows_link_target(&special.encode_utf16().collect::<Vec<_>>()).is_none()
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn workspace_windows_symlink_and_direct_launch_have_the_same_identity() {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};

        let fixture = Fixture::new();
        let target = fixture.directory("target");
        let wide: Vec<_> = target.as_os_str().encode_wide().collect();
        let ordinary = ordinary_windows_link_target(&wide).unwrap_or(wide);
        let target = PathBuf::from(std::ffi::OsString::from_wide(&ordinary));
        assert!(!target.as_os_str().to_string_lossy().starts_with(r"\\?\"));
        let alias = fixture.0.join("alias");
        std::os::windows::fs::symlink_dir(&target, &alias).unwrap();
        let direct = discover_workspace(&target).unwrap();
        let linked = discover_workspace(&alias).unwrap();
        assert_eq!(direct, linked);
        assert_eq!(linked.physical_identity().as_os_str(), target.as_os_str());
        assert_eq!(direct.writable_roots(), [target.clone()]);
        assert_eq!(linked.writable_roots(), [alias, target]);
    }

    fn assert_bom_metadata(bom: &[u8], width: usize, little_endian: bool) {
        fn encoded(text: &str, bom: &[u8], width: usize, little_endian: bool) -> Vec<u8> {
            let mut bytes = bom.to_vec();
            match width {
                1 => bytes.extend_from_slice(text.as_bytes()),
                2 => {
                    for unit in text.encode_utf16() {
                        bytes.extend_from_slice(&if little_endian {
                            unit.to_le_bytes()
                        } else {
                            unit.to_be_bytes()
                        });
                    }
                }
                4 => {
                    for character in text.chars() {
                        bytes.extend_from_slice(&if little_endian {
                            (character as u32).to_le_bytes()
                        } else {
                            (character as u32).to_be_bytes()
                        });
                    }
                }
                _ => unreachable!(),
            }
            bytes
        }

        let fixture = Fixture::new();
        let (common, admin, linked) = fixture.worktree();
        for (path, text) in [
            (
                linked.join(".git"),
                "gitdir: ../main/.git/worktrees/linked\n",
            ),
            (admin.join("commondir"), "../..\n"),
            (admin.join("gitdir"), "../../../../linked/.git\n"),
        ] {
            fs::write(path, encoded(text, bom, width, little_endian)).unwrap();
            let workspace = discover_workspace(&linked).unwrap();
            assert_eq!(workspace.repository_root(), Some(linked.as_path()));
            assert_eq!(
                workspace.writable_roots(),
                [common.parent().unwrap().to_path_buf(), linked.clone()]
            );
        }
    }

    #[test]
    fn workspace_bom_utf8_metadata_retains_linked_candidate() {
        assert_bom_metadata(&[0xef, 0xbb, 0xbf], 1, false);
    }

    #[test]
    fn workspace_bom_utf16_le_metadata_retains_linked_candidate() {
        assert_bom_metadata(&[0xff, 0xfe], 2, true);
    }

    #[test]
    fn workspace_bom_utf16_be_metadata_retains_linked_candidate() {
        assert_bom_metadata(&[0xfe, 0xff], 2, false);
    }

    #[test]
    fn workspace_bom_utf32_le_metadata_retains_linked_candidate() {
        assert_bom_metadata(&[0xff, 0xfe, 0x00, 0x00], 4, true);
    }

    #[test]
    fn workspace_bom_utf32_be_metadata_retains_linked_candidate() {
        assert_bom_metadata(&[0x00, 0x00, 0xfe, 0xff], 4, false);
    }

    #[test]
    fn workspace_export_rejects_blank_relative_missing_and_file_launches() {
        let discover: fn(&Path) -> io::Result<ProjectWorkspace> = crate::paths::discover_workspace;
        for path in ["", "  \t", "relative"] {
            assert_eq!(
                discover(Path::new(path)).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
        let fixture = Fixture::new();
        let file = fixture.0.join("file");
        fs::write(&file, "not a directory").unwrap();
        for path in [file, fixture.0.join("missing")] {
            assert_eq!(discover(&path).unwrap_err().kind(), io::ErrorKind::NotFound);
        }
    }

    #[test]
    fn workspace_preserves_launch_spelling_and_deduplicates_exact_roots() {
        let fixture = Fixture::new();
        let directory = fixture.directory("plain");
        let launch = PathBuf::from(format!("{}/./", directory.display()));
        let workspace = discover_workspace(&launch).unwrap();
        assert_eq!(workspace.launch_directory().as_os_str(), launch.as_os_str());
        assert_eq!(workspace.physical_identity(), directory);
        assert!(!workspace.is_git_repository());
        assert_eq!(workspace.writable_roots().len(), 2);
        assert_eq!(
            workspace.writable_roots()[0].as_os_str(),
            launch.as_os_str()
        );
        assert_eq!(workspace.writable_roots()[1], directory);
        assert_eq!(
            discover_workspace(&directory).unwrap().writable_roots(),
            [directory]
        );
    }

    #[test]
    fn workspace_discovers_repository_ancestor_and_candidate_order() {
        let fixture = Fixture::new();
        let repository = fixture.directory("repo");
        fixture.directory("repo/.git");
        let launch = fixture.directory("repo/nested/deep");
        let workspace = discover_workspace(&launch).unwrap();
        assert_eq!(workspace.repository_root(), Some(repository.as_path()));
        assert_eq!(workspace.writable_roots(), [repository.clone(), launch]);
        assert_eq!(
            discover_workspace(&repository).unwrap().writable_roots(),
            [repository]
        );
    }

    #[test]
    fn workspace_invalid_git_pointer_stops_ancestor_search() {
        let fixture = Fixture::new();
        fixture.directory(".git");
        let launch = fixture.directory("nested");
        for pointer in ["gitdir:", "GITDIR: ../.git", "gitdir: ../missing"] {
            fs::write(launch.join(".git"), pointer).unwrap();
            assert!(
                !discover_workspace(&launch).unwrap().is_git_repository(),
                "{pointer}"
            );
        }
    }

    #[test]
    fn workspace_git_pointer_without_link_metadata_has_no_extra_candidate() {
        let fixture = Fixture::new();
        fixture.directory("admin");
        let launch = fixture.directory("checkout");
        for pointer in [
            "gitdir: ../admin",
            &format!("gitdir: {}", fixture.0.join("admin").display()),
        ] {
            fs::write(launch.join(".git"), pointer).unwrap();
            let workspace = discover_workspace(&launch).unwrap();
            assert_eq!(workspace.repository_root(), Some(launch.as_path()));
            assert_eq!(workspace.writable_roots(), [launch.clone()]);
        }
    }

    #[test]
    fn workspace_linked_worktree_validates_backlinks_and_common_structure() {
        let fixture = Fixture::new();
        let (common, admin, linked) = fixture.worktree();
        let launch = fixture.directory("linked/nested");
        let workspace = discover_workspace(&launch).unwrap();
        assert_eq!(workspace.repository_root(), Some(linked.as_path()));
        assert_eq!(
            workspace.writable_roots(),
            [common.parent().unwrap().to_path_buf(), launch.clone()]
        );
        for (file, value) in [
            ("gitdir", "../../../../wrong/.git"),
            ("commondir", "../../missing"),
        ] {
            fs::write(admin.join(file), value).unwrap();
            let workspace = discover_workspace(&launch).unwrap();
            assert_eq!(workspace.repository_root(), Some(linked.as_path()));
            assert_eq!(workspace.writable_roots(), [launch.clone()]);
            fs::write(admin.join("gitdir"), "../../../../linked/.git").unwrap();
        }
    }

    #[test]
    fn workspace_non_git_common_directory_is_not_a_candidate() {
        let fixture = Fixture::new();
        let common = fixture.directory("main/admin");
        let admin = fixture.directory("main/admin/worktrees/linked");
        let linked = fixture.directory("linked");
        fs::write(linked.join(".git"), format!("gitdir: {}", admin.display())).unwrap();
        fs::write(admin.join("commondir"), "../..").unwrap();
        fs::write(admin.join("gitdir"), linked.join(".git").to_str().unwrap()).unwrap();
        let workspace = discover_workspace(&linked).unwrap();
        assert_ne!(common.file_name().unwrap(), ".git");
        assert_eq!(workspace.repository_root(), Some(linked.as_path()));
        assert_eq!(workspace.writable_roots(), [linked]);
    }

    #[cfg(unix)]
    #[test]
    fn workspace_symlink_identity_falls_back_to_physical_repository() {
        let fixture = Fixture::new();
        let repository = fixture.directory("repo");
        fixture.directory("repo/.git");
        let directory = fixture.directory("repo/nested");
        std::os::unix::fs::symlink(&directory, fixture.0.join("alias")).unwrap();
        let launch = fixture.0.join("alias");
        let workspace = discover_workspace(&launch).unwrap();
        assert_eq!(workspace.launch_directory(), launch);
        assert_eq!(workspace.physical_identity(), directory);
        assert_eq!(workspace.repository_root(), Some(repository.as_path()));
        assert_eq!(
            workspace.writable_roots(),
            [repository, launch, directory.clone()]
        );
        assert_eq!(workspace, discover_workspace(&directory).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn workspace_normalizes_dot_segments_before_resolving_links() {
        let fixture = Fixture::new();
        let physical = fixture.directory("physical/nested");
        fixture.directory("physical/other");
        let lexical = fixture.directory("other");
        std::os::unix::fs::symlink(&physical, fixture.0.join("alias")).unwrap();
        let launch = fixture.0.join("alias/../other");
        let workspace = discover_workspace(&launch).unwrap();
        assert_eq!(workspace.launch_directory().as_os_str(), launch.as_os_str());
        assert_eq!(workspace.physical_identity(), lexical);
    }

    #[test]
    fn workspace_linked_candidate_requires_direct_worktrees_parent() {
        let fixture = Fixture::new();
        let (common, admin, linked) = fixture.worktree();
        let other_admin = fixture.directory("main/.git/other/linked");
        fs::write(other_admin.join("commondir"), "../..").unwrap();
        fs::write(
            other_admin.join("gitdir"),
            linked.join(".git").to_str().unwrap(),
        )
        .unwrap();
        for pointer in [
            format!("gitdir: {}/", admin.display()),
            format!("gitdir: {}", other_admin.display()),
        ] {
            fs::write(linked.join(".git"), pointer).unwrap();
            let workspace = discover_workspace(&linked).unwrap();
            assert_eq!(workspace.repository_root(), Some(linked.as_path()));
            assert_eq!(workspace.writable_roots(), [linked.clone()]);
        }
        assert!(common.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn workspace_launch_repository_takes_precedence_over_physical_repository() {
        let fixture = Fixture::new();
        let launch_root = fixture.directory("launch");
        fixture.directory("launch/.git");
        fixture.directory("physical/.git");
        let physical = fixture.directory("physical/nested");
        let launch = launch_root.join("alias");
        std::os::unix::fs::symlink(&physical, &launch).unwrap();
        let workspace = discover_workspace(&launch).unwrap();
        assert_eq!(workspace.repository_root(), Some(launch_root.as_path()));
        assert_eq!(workspace.physical_identity(), physical);
        assert_eq!(workspace.writable_roots(), [launch_root, launch, physical]);
    }
}
