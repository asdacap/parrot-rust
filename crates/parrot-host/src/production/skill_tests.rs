use super::*;
use parrot_runtime::prompt_context::PACKAGED_SKILL_ASSETS;

fn skill(root: &Path, name: &str, description: &str) -> PathBuf {
    let path = root.join(name).join("SKILL.md");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        format!("---\nname: {name}\ndescription: {description}\n---\nPrivate test instructions."),
    )
    .unwrap();
    path
}

async fn list(host: &ProductionHost) -> wire::ListSkillsResponse {
    let session = bounded(host.service().create_session(
        &wire::CreateSessionRequest {
            model: "local/test".into(),
            mode: "build".into(),
            ..Default::default()
        },
        Arc::new(NeverCancelled),
    ))
    .await
    .unwrap();
    bounded(host.service().list_skills(
        &wire::ListSkillsRequest {
            user_session_id: session.id,
            ..Default::default()
        },
        &NeverCancelled,
    ))
    .await
    .unwrap()
}

#[tokio::test]
async fn production_skill_catalog_uses_nearest_repo_then_ancestors_user_and_system() {
    let mut fixture = Fixture::new();
    fixture.configuration.runtime.skills.enabled = true;
    let repository = fixture.root.join("workspace");
    let launch = repository.join("src/nested");
    std::fs::create_dir_all(repository.join(".git")).unwrap();
    std::fs::create_dir_all(&launch).unwrap();
    let roots = [
        launch.join(".agents/skills"),
        repository.join(".agents/skills"),
        fixture.roots.user_home().join(".agents/skills"),
        fixture.roots.current().skills_directory(),
    ];
    let descriptions = [
        "nearest repository",
        "repository ancestor",
        "user home",
        "system",
    ];
    let paths: Vec<_> = roots
        .iter()
        .zip(descriptions)
        .map(|(root, description)| skill(root, "parity-duplicate", description))
        .collect();
    let host = fixture.host_at(launch, Arc::new(NoCallProvider::default()));
    let listed = list(&host).await;
    assert!(listed.errors.is_empty(), "{:?}", listed.errors);
    let duplicate: Vec<_> = listed
        .skills
        .iter()
        .filter(|skill| skill.name == "parity-duplicate")
        .collect();
    assert_eq!(duplicate.len(), 4);
    for (index, entry) in duplicate.iter().enumerate() {
        assert_eq!(entry.description, descriptions[index]);
        assert_eq!(
            entry.path,
            std::fs::canonicalize(&paths[index])
                .unwrap()
                .to_string_lossy()
        );
        assert_eq!(
            entry.scope.enum_value().unwrap(),
            [
                wire::ListedSkillScope::SKILL_SCOPE_REPO,
                wire::ListedSkillScope::SKILL_SCOPE_REPO,
                wire::ListedSkillScope::SKILL_SCOPE_USER,
                wire::ListedSkillScope::SKILL_SCOPE_SYSTEM,
            ][index]
        );
        assert!(entry.enabled);
    }
    assert!(
        listed
            .skills
            .iter()
            .any(|skill| skill.name == "domain-modeling"
                && skill.scope.enum_value() == Ok(wire::ListedSkillScope::SKILL_SCOPE_SYSTEM))
    );
    assert!(!format!("{listed:?}").contains("Private test instructions."));
    bounded(host.close()).await.unwrap();
    std::fs::remove_dir_all(&fixture.root).unwrap();
}

#[tokio::test]
async fn nonrepository_launch_skips_local_skill_roots_but_keeps_user_skills() {
    let mut fixture = Fixture::new();
    fixture.configuration.runtime.skills.enabled = true;
    skill(
        &fixture.root.join("workspace/.agents/skills"),
        "local-only",
        "not in repository",
    );
    let user_root = fixture.root.join("linked-user-skills");
    let user = skill(&user_root, "user-only", "user skill");
    std::fs::create_dir_all(fixture.roots.user_home().join(".agents")).unwrap();
    std::os::unix::fs::symlink(&user_root, fixture.roots.user_home().join(".agents/skills"))
        .unwrap();
    let host = fixture.host();
    let listed = list(&host).await;
    assert!(listed.errors.is_empty(), "{:?}", listed.errors);
    assert!(!listed.skills.iter().any(|skill| skill.name == "local-only"));
    let entry = listed
        .skills
        .iter()
        .find(|skill| skill.name == "user-only")
        .unwrap();
    assert_eq!(
        entry.path,
        std::fs::canonicalize(user).unwrap().to_string_lossy()
    );
    assert_eq!(
        entry.scope.enum_value(),
        Ok(wire::ListedSkillScope::SKILL_SCOPE_USER)
    );
    bounded(host.close()).await.unwrap();
    std::fs::remove_dir_all(&fixture.root).unwrap();
}

#[tokio::test]
async fn production_installs_packaged_assets_and_preserves_local_edits_and_modes() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let root = fixture.roots.current().skills_directory();
    assert!(!root.exists());
    let host = fixture.host();
    for asset in PACKAGED_SKILL_ASSETS {
        let target = root.join(asset.relative_path);
        assert_eq!(std::fs::read(&target).unwrap(), asset.bytes);
        if asset.executable {
            assert_eq!(
                std::fs::metadata(target).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
    }
    bounded(host.close()).await.unwrap();
    let edited = PACKAGED_SKILL_ASSETS
        .iter()
        .find(|asset| asset.executable)
        .unwrap();
    let target = root.join(edited.relative_path);
    std::fs::write(&target, "local edited script").unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
    let missing = &PACKAGED_SKILL_ASSETS[0];
    std::fs::remove_file(root.join(missing.relative_path)).unwrap();
    let next = fixture.host();
    assert_eq!(std::fs::read(&target).unwrap(), b"local edited script");
    assert_eq!(
        std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::read(root.join(missing.relative_path)).unwrap(),
        missing.bytes
    );
    bounded(next.close()).await.unwrap();
    std::fs::remove_dir_all(&fixture.root).unwrap();
}

#[test]
fn concurrent_packaged_installers_preserve_winners_and_leave_no_temporary_files() {
    let fixture = Fixture::new();
    let root = fixture.roots.current().skills_directory();
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        for _ in 0..2 {
            let root = &root;
            let barrier = &barrier;
            scope.spawn(move || {
                barrier.wait();
                crate::production::skills::install_packaged_skills(root).unwrap();
            });
        }
    });
    let mut directories = std::collections::BTreeSet::new();
    for asset in PACKAGED_SKILL_ASSETS {
        let path = root.join(asset.relative_path);
        assert_eq!(std::fs::read(&path).unwrap(), asset.bytes);
        directories.insert(path.parent().unwrap().to_owned());
    }
    for directory in directories {
        assert!(std::fs::read_dir(directory).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".packaged-")
        }));
    }
    std::fs::remove_dir_all(&fixture.root).unwrap();
}

#[test]
fn packaged_install_failure_keeps_the_source_write_error_boundary() {
    let fixture = Fixture::new();
    let root = fixture.root.join("not-a-directory");
    std::fs::write(&root, "preserve this file").unwrap();
    let error = crate::production::skills::install_packaged_skills(&root).unwrap_err();
    assert!(matches!(
        error,
        parrot_runtime::prompt_context::PackagedSkillsError::Write(_)
    ));
    assert_eq!(error.to_string(), "unable to write packaged skills");
    assert_eq!(std::fs::read(&root).unwrap(), b"preserve this file");
    std::fs::remove_dir_all(&fixture.root).unwrap();
}
