use super::{
    Failure, FailureKind, Invocation, Output, input,
    tools::{error_output, text_output},
};
use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation, OperationError, OperationResult},
        filesystem::{
            AuthorizedMutationService, AuthorizedReadService, FileReadError, FileReadErrorKind,
            LinkPolicy, MutationRequest, ReadRequest,
        },
        security::{AccessSnapshot, InvocationContext},
        tools::{ParallelSafety, Tool},
    },
    values::tools::ConfiguredToolDefinition,
};
use parrot_provider::image_generation::{
    ImageGenerationReference, ImageGenerationRequest, ImageGenerationResult,
    client::{
        MAXIMUM_OUTPUT_BYTES, MAXIMUM_REFERENCE_BATCH_BYTES, MAXIMUM_REFERENCE_BYTES,
        MAXIMUM_REFERENCES, validate_image,
    },
};
use serde_json::Value;
use std::{path::PathBuf, sync::Arc, time::Duration};

/// The selected turn's actual provider, including its wrappers. Unsupported
/// generation must fail, not fall back to another provider or credentials.
/// Implementations observe cancellation and join their admitted request work.
pub trait ImageProvider: Send + Sync {
    type Selection: Send + Sync + 'static;
    type Security: AccessSnapshot + 'static;

    fn generate<'a>(
        &'a self,
        selection: &'a Self::Selection,
        request: ImageGenerationRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<ImageGenerationResult, Failure>>;
}

pub struct ImageGenerationTool<P: ImageProvider, F> {
    definition: ConfiguredToolDefinition,
    provider: Arc<P>,
    filesystem: Arc<F>,
}

impl<P, F> ImageGenerationTool<P, F>
where
    P: ImageProvider,
    F: AuthorizedReadService<Security = P::Security>
        + AuthorizedMutationService<Security = P::Security>,
{
    pub fn new(definition: ConfiguredToolDefinition, provider: Arc<P>, filesystem: Arc<F>) -> Self {
        Self {
            definition,
            provider,
            filesystem,
        }
    }

    pub fn name(&self) -> &'static str {
        "imagegen"
    }

    async fn execute(
        &self,
        invocation: &Invocation,
        context: &InvocationContext<'_, P::Security, P::Selection>,
    ) -> OperationResult<Output, Failure> {
        let cancellation = ImageCancellation {
            parent: context.cancellation(),
            deadline: tokio::time::Instant::now() + Duration::from_secs(600),
        };
        cancellation
            .check()
            .map_err(|_| OperationError::Cancelled)?;
        let object = input::object(
            &invocation.arguments_json,
            &["prompt", "output_path", "referenced_image_paths"],
        )
        .map_err(OperationError::Failed)?;
        let prompt = input::optional_string(&object, "prompt")
            .map_err(OperationError::Failed)?
            .unwrap_or("");
        let output = input::optional_string(&object, "output_path")
            .map_err(OperationError::Failed)?
            .unwrap_or("");
        if prompt.trim().is_empty() || output.trim().is_empty() {
            return Err(OperationError::Failed(input::format(
                "Tool arguments require nonblank 'prompt' and 'output_path' strings.",
            )));
        }
        let final_component = output
            .rsplit(std::path::MAIN_SEPARATOR)
            .next()
            .unwrap_or("");
        let extension = final_component
            .rfind('.')
            .map(|index| &final_component[index..]);
        if !extension.is_some_and(|extension| extension.eq_ignore_ascii_case(".png")) {
            return Err(OperationError::Failed(input::format(
                "Image output_path must have a .png extension.",
            )));
        }
        let paths = match object.get("referenced_image_paths") {
            None | Some(Value::Null) => &[][..],
            Some(Value::Array(paths)) => paths.as_slice(),
            Some(_) => {
                return Err(OperationError::Failed(Failure::new(
                    FailureKind::Json,
                    "referenced_image_paths must be an array.",
                )));
            }
        };
        if paths.len() > MAXIMUM_REFERENCES {
            return Err(OperationError::Failed(input::format(
                "At most five referenced_image_paths are supported.",
            )));
        }
        let mutation = MutationRequest {
            requested: PathBuf::from(output),
            create: true,
        };
        self.filesystem
            .inspect_for_mutation(&mutation, context.security(), &cancellation)
            .await
            .map_err(file_error)?;
        let mut references = Vec::new();
        let mut total = 0usize;
        for path in paths {
            let path = match path.as_str() {
                Some(path) if !path.trim().is_empty() => path,
                _ => {
                    return Err(OperationError::Failed(input::format(
                        "Referenced image paths must be nonblank strings.",
                    )));
                }
            };
            let request = ReadRequest {
                requested: PathBuf::from(path),
                expected_physical: None,
                links: LinkPolicy::RejectLinks,
                max_bytes: Some(MAXIMUM_REFERENCE_BYTES.min(MAXIMUM_REFERENCE_BATCH_BYTES - total)),
            };
            let reference = self
                .filesystem
                .read(&request, context.security(), &cancellation)
                .await
                .map_err(file_error)?;
            total += reference.bytes.len();
            if reference.bytes.len() > MAXIMUM_REFERENCE_BYTES
                || total > MAXIMUM_REFERENCE_BATCH_BYTES
            {
                return Err(OperationError::Failed(input::format(
                    "Image references exceed the supported byte limits.",
                )));
            }
            let media_type = validate_image(&reference.bytes, false).map_err(|error| {
                OperationError::Failed(Failure::new(FailureKind::Provider, error.to_string()))
            })?;
            references.push(ImageGenerationReference {
                media_type: media_type.to_owned(),
                data: reference.bytes,
            });
        }
        let result = self
            .provider
            .generate(
                context.selection(),
                ImageGenerationRequest {
                    prompt: prompt.to_owned(),
                    references,
                },
                &cancellation,
            )
            .await?;
        if result.data.len() > MAXIMUM_OUTPUT_BYTES {
            return Err(OperationError::Failed(Failure::new(
                FailureKind::Provider,
                "Generated image exceeds the supported byte limit.",
            )));
        }
        validate_image(&result.data, true).map_err(|error| {
            OperationError::Failed(Failure::new(FailureKind::Provider, error.to_string()))
        })?;
        let output = self
            .filesystem
            .write_for_mutation(&mutation, &result.data, context.security(), &cancellation)
            .await
            .map_err(file_error)?;
        text_output(invocation, &output.physical_path.to_string_lossy())
    }
}

impl<P, F> Tool for ImageGenerationTool<P, F>
where
    P: ImageProvider,
    F: AuthorizedReadService<Security = P::Security>
        + AuthorizedMutationService<Security = P::Security>,
{
    type Definition = ConfiguredToolDefinition;
    type Invocation = Invocation;
    type Selection = P::Selection;
    type Security = P::Security;
    type Output = Output;
    type Error = Failure;

    fn definition(&self) -> &Self::Definition {
        &self.definition
    }
    fn parallel_safety(&self, _: &Invocation) -> ParallelSafety {
        ParallelSafety::Sequential
    }
    fn enabled_after_interruption(&self) -> bool {
        false
    }
    fn invoke<'a>(
        &'a self,
        invocation: Invocation,
        context: InvocationContext<'a, P::Security, P::Selection>,
    ) -> BoxFuture<'a, OperationResult<Output, Failure>> {
        Box::pin(async move {
            match self.execute(&invocation, &context).await {
                Err(OperationError::Failed(error))
                    if matches!(
                        error.kind,
                        FailureKind::Authentication | FailureKind::Network
                    ) =>
                {
                    error_output(
                        &invocation,
                        "Image generation authentication or network request failed.",
                    )
                }
                Err(OperationError::Failed(error))
                    if matches!(
                        error.kind,
                        FailureKind::Json
                            | FailureKind::Format
                            | FailureKind::Io
                            | FailureKind::Unauthorized
                            | FailureKind::InvalidOperation
                            | FailureKind::Argument
                            | FailureKind::Provider
                            | FailureKind::Wire
                    ) =>
                {
                    error_output(&invocation, &error.message)
                }
                result => result,
            }
        })
    }
}

fn file_error(error: OperationError<FileReadError>) -> OperationError<Failure> {
    match error {
        OperationError::Cancelled => OperationError::Cancelled,
        OperationError::Failed(error) => {
            let kind = match error.kind {
                FileReadErrorKind::Denied => FailureKind::Unauthorized,
                FileReadErrorKind::InvalidPath => FailureKind::Argument,
                FileReadErrorKind::InvalidOperation => FailureKind::InvalidOperation,
                FileReadErrorKind::NotSupported => FailureKind::Other,
                FileReadErrorKind::TooLarge => {
                    return OperationError::Failed(input::format(
                        "Image references exceed the supported byte limits.",
                    ));
                }
                _ => FailureKind::Io,
            };
            OperationError::Failed(Failure::new(kind, error.message))
        }
    }
}

struct ImageCancellation<'a> {
    parent: &'a dyn Cancellation,
    deadline: tokio::time::Instant,
}
impl Cancellation for ImageCancellation<'_> {
    fn is_cancelled(&self) -> bool {
        self.parent.is_cancelled() || tokio::time::Instant::now() >= self.deadline
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            tokio::select! {
                () = self.parent.cancelled() => {},
                () = tokio::time::sleep_until(self.deadline) => {},
            }
        })
    }
}
