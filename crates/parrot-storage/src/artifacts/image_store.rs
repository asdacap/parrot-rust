use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use parrot_domain::contracts::async_ops::Cancellation;
use parrot_domain::contracts::paths::SelectedSessionRoot;
use sha2::{Digest, Sha256};

use super::{Error, ErrorKind, ImageArtifactMetadata, ImageInspection, Result, internal_resource};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImageLimits {
    pub maximum_images: usize,
    pub maximum_image_bytes: usize,
    pub maximum_batch_bytes: usize,
    pub maximum_dimension: u32,
    pub maximum_frame_pixels: u64,
    pub maximum_frames: u32,
    pub maximum_aggregate_pixels: u64,
}

impl ImageLimits {
    pub const SOURCE: Self = Self {
        maximum_images: 16,
        maximum_image_bytes: 5 * 1024 * 1024,
        maximum_batch_bytes: 20 * 1024 * 1024,
        maximum_dimension: 8192,
        maximum_frame_pixels: 40_000_000,
        maximum_frames: 100,
        maximum_aggregate_pixels: 100_000_000,
    };
}

static NEXT_STAGE: AtomicU64 = AtomicU64::new(0);

/// Content-addressed image bytes confined to one selected session resource.
#[derive(Clone, Debug)]
pub struct ImageArtifactStore {
    selected_root: SelectedSessionRoot,
    directory: PathBuf,
    limits: ImageLimits,
}

impl ImageArtifactStore {
    pub fn for_session(root: &SelectedSessionRoot, relative: impl AsRef<Path>) -> Result<Self> {
        let resource = internal_resource(root, relative)?;
        Ok(Self {
            selected_root: root.clone(),
            directory: resource.path().to_owned(),
            limits: ImageLimits::SOURCE,
        })
    }

    pub fn selected_root(&self) -> &SelectedSessionRoot {
        &self.selected_root
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn persist<R: Read>(
        &self,
        source: &mut R,
        display_name: &str,
        origin: &str,
        declared_media_type: Option<&str>,
        declared_byte_length: Option<i64>,
        cancellation: &dyn Cancellation,
    ) -> Result<ImageArtifactMetadata> {
        validate_label(display_name, "displayName")?;
        validate_label(origin, "origin")?;
        self.ensure_directory()?;
        cancellation.check().map_err(|_| Error::task_cancelled())?;
        let (staging, mut staged) = self.create_staging()?;
        let outcome = (|| {
            let mut length: usize = 0;
            let mut buffer = [0u8; 81920];
            loop {
                cancellation.check().map_err(|_| Error::task_cancelled())?;
                let read = source
                    .read(&mut buffer)
                    .map_err(|e| Error::io("could not read image source", e))?;
                if read == 0 {
                    break;
                }
                if length
                    .checked_add(read)
                    .is_none_or(|next| next > self.limits.maximum_image_bytes)
                {
                    return Err(Error::invalid_data("An image cannot exceed 5 MiB."));
                }
                staged
                    .write_all(&buffer[..read])
                    .map_err(|e| Error::io("could not write image staging file", e))?;
                length += read;
            }
            staged
                .flush()
                .map_err(|e| Error::io("could not flush image staging file", e))?;
            drop(staged);
            cancellation.check().map_err(|_| Error::task_cancelled())?;
            let bytes = fs::read(&staging)
                .map_err(|e| Error::io("could not read image staging file", e))?;
            let metadata = self.inspect_bytes(&bytes, display_name, origin)?;
            if declared_byte_length
                .is_some_and(|declared| declared >= 0 && declared != metadata.byte_length)
            {
                return Err(Error::invalid_data(
                    "The image byte length does not match its declaration.",
                ));
            }
            if declared_media_type.is_some_and(|declared| {
                !declared.is_empty() && !declared.eq_ignore_ascii_case(&metadata.media_type)
            }) {
                return Err(Error::invalid_data(
                    "The image media type does not match its declaration.",
                ));
            }
            cancellation.check().map_err(|_| Error::task_cancelled())?;
            let destination = self.directory.join(&metadata.artifact_id);
            match fs::hard_link(&staging, &destination) {
                Ok(()) => {
                    fs::remove_file(&staging)
                        .map_err(|e| Error::io("could not remove image staging file", e))?;
                }
                Err(error) => {
                    if destination.is_file() {
                        let _ = fs::remove_file(&staging);
                    } else {
                        return Err(Error::io("could not publish image artifact", error));
                    }
                }
            }
            Ok(metadata)
        })();
        if let Err(error) = outcome {
            let _ = fs::remove_file(&staging);
            return Err(error);
        }
        outcome
    }

    pub fn delete(&self, artifact_id: &str) -> Result<()> {
        let path = self.hash_path(artifact_id)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(Error::io("could not delete image artifact", error)),
        }
    }

    pub fn open(&self, artifact_id: &str) -> Result<File> {
        File::open(self.path_of(artifact_id)?)
            .map_err(|e| Error::io("could not open image artifact", e))
    }

    pub fn path_of(&self, artifact_id: &str) -> Result<PathBuf> {
        let path = self.hash_path(artifact_id)?;
        match fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => Ok(path),
            Ok(_) => Err(Error::new(
                ErrorKind::NotFound,
                "The image artifact does not exist.",
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(Error::new(
                ErrorKind::NotFound,
                "The image artifact does not exist.",
            )
            .with_source(error)),
            Err(error) => Err(Error::io("could not inspect image artifact path", error)),
        }
    }

    pub(super) fn inspect(
        &self,
        artifact_id: &str,
        display_name: &str,
        origin: &str,
        cancellation: &dyn Cancellation,
    ) -> Result<Option<ImageArtifactMetadata>> {
        cancellation.check().map_err(|_| Error::task_cancelled())?;
        let file = match self.open(artifact_id) {
            Ok(file) => file,
            Err(error) if error.kind == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let mut bytes = Vec::new();
        file.take(self.limits.maximum_image_bytes as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| Error::io("could not read image artifact", error))?;
        cancellation.check().map_err(|_| Error::task_cancelled())?;
        let metadata = self.inspect_bytes(&bytes, display_name, origin)?;
        if metadata.artifact_id != artifact_id {
            return Err(Error::invalid_data(
                "The image bytes do not match their artifact id.",
            ));
        }
        Ok(Some(metadata))
    }

    fn hash_path(&self, artifact_id: &str) -> Result<PathBuf> {
        if artifact_id.len() != 64
            || !artifact_id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(Error::invalid_input(
                "An artifact id must be a lowercase SHA-256 hexadecimal value.",
            ));
        }
        Ok(self.directory.join(artifact_id))
    }

    fn inspect_bytes(
        &self,
        bytes: &[u8],
        display_name: &str,
        origin: &str,
    ) -> Result<ImageArtifactMetadata> {
        if bytes.len() > self.limits.maximum_image_bytes {
            return Err(Error::invalid_data("An image cannot exceed 5 MiB."));
        }
        let inspection = ImageInspection::inspect(bytes)
            .map_err(|error| Error::invalid_data(error.message()))?;
        let frame_pixels = (inspection.width as u64)
            .checked_mul(inspection.height as u64)
            .ok_or_else(|| {
                Error::invalid_data(
                    "The image dimensions or frame count exceed the supported limits.",
                )
            })?;
        let aggregate_pixels = frame_pixels
            .checked_mul(inspection.frame_count as u64)
            .ok_or_else(|| {
                Error::invalid_data(
                    "The image dimensions or frame count exceed the supported limits.",
                )
            })?;
        if inspection.width > self.limits.maximum_dimension
            || inspection.height > self.limits.maximum_dimension
            || frame_pixels > self.limits.maximum_frame_pixels
            || inspection.frame_count > self.limits.maximum_frames
            || aggregate_pixels > self.limits.maximum_aggregate_pixels
        {
            return Err(Error::invalid_data(
                "The image dimensions or frame count exceed the supported limits.",
            ));
        }
        let hash = format!("{:x}", Sha256::digest(bytes));
        let metadata = ImageArtifactMetadata {
            artifact_id: hash.clone(),
            sha256: hash,
            media_type: inspection.media_type.to_owned(),
            byte_length: bytes.len() as i64,
            width: inspection.width,
            height: inspection.height,
            frame_count: inspection.frame_count,
            aggregate_pixels: aggregate_pixels as i64,
            display_name: display_name.to_owned(),
            origin: origin.to_owned(),
        };
        Ok(metadata)
    }

    fn ensure_directory(&self) -> Result<()> {
        fs::create_dir_all(&self.directory)
            .map_err(|e| Error::io("image artifact path is not a directory", e))?;
        if !self.directory.is_dir() {
            return Err(Error::invalid_data(
                "image artifact path is not a directory",
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.directory, fs::Permissions::from_mode(0o700))
                .map_err(|e| Error::io("image artifact path is not a directory", e))?;
        }
        Ok(())
    }

    fn create_staging(&self) -> Result<(PathBuf, File)> {
        for _ in 0..128 {
            let count = NEXT_STAGE.fetch_add(1, Ordering::Relaxed);
            let path = self.directory.join(format!(
                ".staging-{:016x}-{:016x}",
                std::process::id(),
                count
            ));
            let mut options = OpenOptions::new();
            options.write(true).read(false).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(file) => return Ok((path, file)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(Error::io("could not create image staging file", error)),
            }
        }
        Err(Error::new(
            ErrorKind::Conflict,
            "could not allocate image staging file",
        ))
    }
}

fn validate_label(label: &str, parameter_name: &str) -> Result<()> {
    if label.trim().is_empty() {
        return Err(Error::invalid_input(format!(
            "The value cannot be an empty string or composed entirely of whitespace. (Parameter '{parameter_name}')"
        )));
    }
    if label.chars().any(char::is_control) {
        return Err(Error::invalid_input(format!(
            "An image label cannot contain control characters. (Parameter '{parameter_name}')"
        )));
    }
    Ok(())
}
