use std::{io::Cursor, path::Path, sync::Arc};

use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation, OperationError, OperationResult},
        filesystem::{
            AuthorizedReadService, EntryKind, FileReadErrorKind, LinkPolicy, ReadRequest,
        },
        security::InvocationContext,
        tools::{ParallelSafety, Tool},
    },
    values::tools::ConfiguredToolDefinition,
};
use parrot_storage::artifacts::{ErrorKind, ImageArtifactRepository};

use super::{
    FileToolError, FileToolInvocation, FileToolOutcome, FileToolResult, Formatter, file_error,
    input,
};

pub struct ReadImageTool<S> {
    service: S,
    artifacts: Arc<ImageArtifactRepository>,
    maximum_bytes: i32,
    definition: ConfiguredToolDefinition,
}

impl<S: AuthorizedReadService> ReadImageTool<S> {
    pub fn new(
        service: S,
        artifacts: Arc<ImageArtifactRepository>,
        maximum_bytes: i32,
        definition: ConfiguredToolDefinition,
    ) -> Self {
        Self {
            service,
            artifacts,
            maximum_bytes,
            definition,
        }
    }

    pub const fn name(&self) -> &'static str {
        "read_image"
    }

    pub async fn execute(
        &self,
        invocation: &FileToolInvocation,
        snapshot: &S::Security,
        cancellation: &dyn Cancellation,
    ) -> OperationResult<FileToolResult, FileToolError> {
        if let Some(budget) = &invocation.image_budget {
            if budget.budget.is_exceeded() {
                return exceeded(budget);
            }
        }
        let formatter = Formatter::new(invocation);
        let path = match input::image_path(&invocation.arguments_json) {
            Ok(path) => path,
            Err(error) => return formatter.error(&error),
        };
        let path = Path::new(&path);
        let metadata = match self
            .service
            .inspect(path, LinkPolicy::Follow, snapshot, cancellation)
            .await
        {
            Ok(metadata) => metadata,
            Err(OperationError::Failed(error)) if error.kind == FileReadErrorKind::NotFound => {
                return formatter.error("no such file or directory");
            }
            Err(OperationError::Failed(error))
                if error.is_io() || error.kind == FileReadErrorKind::InvalidOperation =>
            {
                return formatter.error(&error.message);
            }
            Err(error) => return Err(file_error(error)),
        };
        if metadata.kind != EntryKind::Regular {
            return formatter.error("no such file or directory");
        }
        let request = ReadRequest {
            requested: path.to_owned(),
            expected_physical: Some(metadata.physical_path),
            links: LinkPolicy::Follow,
            max_bytes: Some(self.maximum_bytes.max(0) as usize),
        };
        let read = match self.service.read(&request, snapshot, cancellation).await {
            Ok(read) => read,
            Err(OperationError::Failed(error)) if error.kind == FileReadErrorKind::TooLarge => {
                return formatter.error(&format!(
                    "An image cannot exceed {} bytes.",
                    self.maximum_bytes
                ));
            }
            Err(OperationError::Failed(error))
                if error.is_io() || error.kind == FileReadErrorKind::Denied =>
            {
                return formatter.error(&error.message);
            }
            Err(error) => return Err(file_error(error)),
        };
        if read.bytes.len() as i64 > i64::from(self.maximum_bytes) {
            return formatter.error(&format!(
                "An image cannot exceed {} bytes.",
                self.maximum_bytes
            ));
        }
        let display_name = read
            .physical_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let artifact = match self
            .artifacts
            .persist(
                Cursor::new(read.bytes),
                &invocation.call_id,
                &display_name,
                "read_image",
                None,
                None,
                cancellation,
            )
            .await
        {
            Ok(artifact) => artifact,
            Err(error) if error.kind == ErrorKind::Cancelled => {
                return Err(OperationError::Cancelled);
            }
            Err(error)
                if matches!(
                    error.kind,
                    ErrorKind::InvalidData | ErrorKind::Io | ErrorKind::NotFound
                ) =>
            {
                return formatter.error(&error.message);
            }
            Err(error) => {
                return Err(OperationError::Failed(FileToolError::Artifact(
                    error.to_string(),
                )));
            }
        };
        if let Some(budget) = &invocation.image_budget {
            if !budget
                .budget
                .try_accept(artifact.byte_length)
                .map_err(|error| {
                    OperationError::Failed(FileToolError::Artifact(error.to_string()))
                })?
            {
                return exceeded(budget);
            }
        }
        Ok(FileToolResult {
            text: "image read".into(),
            image_artifacts: vec![artifact],
            outcome: FileToolOutcome::Finished,
        })
    }
}

fn exceeded(budget: &super::ImageBudget) -> OperationResult<FileToolResult, FileToolError> {
    Ok(FileToolResult {
        text: budget.describe_failure()?,
        image_artifacts: Vec::new(),
        outcome: FileToolOutcome::ImageBudgetExceeded,
    })
}

impl<S: AuthorizedReadService + 'static> Tool for ReadImageTool<S> {
    type Definition = ConfiguredToolDefinition;
    type Invocation = FileToolInvocation;
    type Selection = ();
    type Security = S::Security;
    type Output = FileToolResult;
    type Error = FileToolError;

    fn definition(&self) -> &Self::Definition {
        &self.definition
    }
    fn parallel_safety(&self, _: &Self::Invocation) -> ParallelSafety {
        ParallelSafety::ParallelSafe
    }
    fn enabled_after_interruption(&self) -> bool {
        false
    }
    fn invoke<'a>(
        &'a self,
        invocation: Self::Invocation,
        context: InvocationContext<'a, Self::Security, Self::Selection>,
    ) -> BoxFuture<'a, OperationResult<Self::Output, Self::Error>> {
        Box::pin(async move {
            self.execute(&invocation, context.security(), context.cancellation())
                .await
        })
    }
}
