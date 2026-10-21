use super::*;
use parrot_domain::contracts::{packaged_files::*, paths::ResolvedRead, security::AccessSnapshot};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Mutex,
};
struct Snapshot;
impl AccessSnapshot for Snapshot {
    type Error = &'static str;
    fn check_read(&self, _: &ResolvedRead) -> Result<(), Self::Error> {
        panic!("installer orchestrator must not make read grants")
    }
    fn check_write(&self, _: &Path) -> Result<(), Self::Error> {
        Err("not a production capability")
    }
}
#[derive(Default)]
struct Installer {
    files: Mutex<BTreeMap<PathBuf, (Vec<u8>, bool)>>,
    fail: Option<InstallMissingErrorKind>,
}
impl PackagedFileInstaller for Installer {
    type Security = Snapshot;
    fn install_missing<'a>(
        &'a self,
        request: &'a InstallMissingRequest,
        _: &'a Snapshot,
    ) -> BoxFuture<'a, Result<InstallMissingOutcome, InstallMissingError>> {
        Box::pin(async move {
            if let Some(kind) = self.fail {
                return Err(InstallMissingError {
                    kind,
                    message: "original backend failure".into(),
                });
            }
            use std::collections::btree_map::Entry;
            match self.files.lock().unwrap().entry(request.target.clone()) {
                Entry::Occupied(_) => Ok(InstallMissingOutcome::AlreadyPresent),
                Entry::Vacant(entry) => {
                    entry.insert((request.contents.clone(), request.executable));
                    Ok(InstallMissingOutcome::Installed)
                }
            }
        })
    }
}
#[tokio::test]
async fn installer_receives_every_original_asset_once_and_preserves_existing_targets() {
    let installer = Installer::default();
    let path = Path::new("/private/packaged");
    let first = &PACKAGED_SKILL_ASSETS[0];
    installer.files.lock().unwrap().insert(
        path.join(first.relative_path),
        (b"local edits".to_vec(), false),
    );
    assert_eq!(PACKAGED_SKILL_ASSETS.len(), 76);
    extract_packaged_skills(path, &installer, &Snapshot)
        .await
        .unwrap();
    let files = installer.files.lock().unwrap();
    assert_eq!(files.len(), PACKAGED_SKILL_ASSETS.len());
    assert_eq!(files[&path.join(first.relative_path)].0, b"local edits");
    for asset in PACKAGED_SKILL_ASSETS.iter().skip(1) {
        assert_eq!(files[&path.join(asset.relative_path)].0, asset.bytes);
        assert_eq!(
            files[&path.join(asset.relative_path)].1,
            asset
                .relative_path
                .split('/')
                .take(asset.relative_path.split('/').count() - 1)
                .any(|part| part == "scripts")
        );
    }
}
#[tokio::test]
async fn packaged_outer_error_wraps_only_actual_source_io_and_denial_families() {
    for kind in [
        InstallMissingErrorKind::Io,
        InstallMissingErrorKind::Denied,
        InstallMissingErrorKind::InvalidOperation,
    ] {
        let installer = Installer {
            fail: Some(kind),
            ..Installer::default()
        };
        let error = extract_packaged_skills(Path::new("/private/packaged"), &installer, &Snapshot)
            .await
            .unwrap_err();
        assert!(installer.files.lock().unwrap().is_empty());
        match kind {
            InstallMissingErrorKind::Io | InstallMissingErrorKind::Denied => {
                assert_eq!(error.to_string(), "unable to write packaged skills");
                assert!(
                    matches!(error, PackagedSkillsError::Write(InstallMissingError { kind: actual, .. }) if actual == kind)
                );
            }
            InstallMissingErrorKind::InvalidOperation => {
                assert_eq!(error.to_string(), "original backend failure")
            }
        }
    }
    assert_eq!(
        extract_packaged_skills(Path::new(""), &Installer::default(), &Snapshot)
            .await
            .unwrap_err(),
        PackagedSkillsError::EmptyDestination
    );
}
