use parrot_domain::{
    contracts::packaged_files::{
        InstallMissingError, InstallMissingErrorKind, InstallMissingOutcome, InstallMissingRequest,
    },
    values::identifiers::Identifier,
};
use parrot_runtime::prompt_context::{PackagedSkillsError, extract_packaged_skills_sync};
use std::{fs, io, io::Write, path::Path};

pub(super) fn install_packaged_skills(destination: &Path) -> Result<(), PackagedSkillsError> {
    extract_packaged_skills_sync(destination, |request| {
        install_missing(request).map_err(|error| InstallMissingError {
            kind: if error.kind() == io::ErrorKind::PermissionDenied {
                InstallMissingErrorKind::Denied
            } else {
                InstallMissingErrorKind::Io
            },
            message: error.to_string(),
        })
    })
}

// Only the host-selected application directory and embedded asset requests reach
// this installer; it is not a file-tool capability or an agent permission grant.
fn install_missing(request: &InstallMissingRequest) -> io::Result<InstallMissingOutcome> {
    if request.target.is_file() {
        return Ok(InstallMissingOutcome::AlreadyPresent);
    }
    let parent = request
        .target
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "skill file has no parent"))?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".packaged-{}", Identifier::event_id()?));
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    let result = (|| {
        file.write_all(&request.contents)?;
        #[cfg(unix)]
        if request.executable {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o755))?;
        }
        drop(file);
        match fs::hard_link(&temporary, &request.target) {
            Ok(()) => Ok(InstallMissingOutcome::Installed),
            Err(error)
                if error.kind() == io::ErrorKind::AlreadyExists && request.target.is_file() =>
            {
                Ok(InstallMissingOutcome::AlreadyPresent)
            }
            Err(error) => Err(error),
        }
    })();
    let cleanup = fs::remove_file(&temporary);
    match result {
        Ok(outcome) => cleanup.map(|()| outcome),
        Err(error) => Err(error),
    }
}
