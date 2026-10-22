use parrot_domain::{
    contracts::packaged_files::{InstallMissingError, InstallMissingErrorKind},
    values::identifiers::Identifier,
};
use parrot_runtime::prompt_context::{
    PACKAGED_SKILL_ASSETS, PackagedSkillsError, hash_packaged_skills,
};
use std::{fs, io, io::Write, path::Path};

pub(super) fn install_packaged_skills(destination: &Path) -> Result<(), PackagedSkillsError> {
    if destination.as_os_str().is_empty() {
        return Err(PackagedSkillsError::EmptyDestination);
    }
    let result = (|| {
        let bundle_hash = hash_packaged_skills();
        let version_path = destination.join("VERSION");
        let refresh = match fs::read(&version_path) {
            Ok(version) => version != bundle_hash.as_bytes(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => true,
            Err(error) => return Err(error),
        };
        let publication = if refresh {
            FilePublication::ReplaceExisting
        } else {
            FilePublication::PreserveExisting
        };
        for asset in PACKAGED_SKILL_ASSETS {
            install_file(
                &destination.join(asset.relative_path),
                asset.bytes,
                asset.executable,
                publication,
            )?;
        }
        if refresh {
            install_file(
                &version_path,
                bundle_hash.as_bytes(),
                false,
                FilePublication::ReplaceExisting,
            )?;
        }
        Ok(())
    })();
    result.map_err(|error: io::Error| {
        PackagedSkillsError::Write(InstallMissingError {
            kind: if error.kind() == io::ErrorKind::PermissionDenied {
                InstallMissingErrorKind::Denied
            } else {
                InstallMissingErrorKind::Io
            },
            message: error.to_string(),
        })
    })
}

#[derive(Clone, Copy)]
enum FilePublication {
    PreserveExisting,
    ReplaceExisting,
}

// Only the host-selected application directory and embedded asset requests reach
// this installer; it is not a file-tool capability or an agent permission grant.
fn install_file(
    target: &Path,
    contents: &[u8],
    executable: bool,
    publication: FilePublication,
) -> io::Result<()> {
    if matches!(publication, FilePublication::PreserveExisting) && target.is_file() {
        return Ok(());
    }
    let parent = target
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "skill file has no parent"))?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".packaged-{}", Identifier::event_id()?));
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    let result = (|| {
        file.write_all(contents)?;
        if executable {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(fs::Permissions::from_mode(0o755))?;
            }
        }
        drop(file);
        match publication {
            FilePublication::ReplaceExisting => fs::rename(&temporary, target),
            FilePublication::PreserveExisting => match fs::hard_link(&temporary, target) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists && target.is_file() => {
                    Ok(())
                }
                Err(error) => Err(error),
            },
        }
    })();
    let cleanup = match fs::remove_file(&temporary) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    };
    result.and(cleanup)
}
