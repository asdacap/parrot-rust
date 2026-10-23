use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation, OperationError, OperationResult},
        filesystem::{AuthorizedMutationService, FileReadErrorKind, MutationRequest},
        security::InvocationContext,
        tools::{ParallelSafety, Tool},
    },
    values::tools::ConfiguredToolDefinition,
};

use super::{
    FileToolError, FileToolInvocation, FileToolResult, Formatter, WorkspacePaths,
    diff::{FileChange, render},
    file_error,
    input::{EditInput, WriteInput},
};

const NO_CHANGES: &str = "No changes made.";
const UTF8_PREAMBLE: &[u8] = &[0xef, 0xbb, 0xbf];

pub struct WriteTool<S> {
    service: S,
    paths: WorkspacePaths,
    definition: ConfiguredToolDefinition,
}

impl<S: AuthorizedMutationService> WriteTool<S> {
    pub fn new(service: S, paths: WorkspacePaths, definition: ConfiguredToolDefinition) -> Self {
        Self {
            service,
            paths,
            definition,
        }
    }

    pub const fn name(&self) -> &'static str {
        "write"
    }

    pub async fn execute(
        &self,
        invocation: &FileToolInvocation,
        snapshot: &S::Security,
        cancellation: &dyn Cancellation,
    ) -> OperationResult<FileToolResult, FileToolError> {
        let formatter = Formatter::new(invocation);
        let input = match WriteInput::parse(&invocation.arguments_json) {
            Ok(input) => input,
            Err(error) => return formatter.error(&error),
        };
        let path = self.paths.expand_path(&input.path);
        if path.is_empty() {
            return formatter.error("Tool argument 'path' must not be empty.");
        }
        cancellation
            .check()
            .map_err(|_| OperationError::Cancelled)?;
        let request = MutationRequest {
            requested: path.into(),
            create: true,
        };
        let before = match self
            .service
            .read_for_mutation(&request, snapshot, cancellation)
            .await
        {
            Ok(before) => before,
            Err(error) => return mutation_error(error, &formatter),
        };
        let after = input.content.as_bytes();
        if before.bytes.as_deref() != Some(after) {
            match self
                .service
                .write_for_mutation(&request, after, snapshot, cancellation)
                .await
            {
                Ok(_) => {}
                Err(error) => return mutation_error(error, &formatter),
            }
        }
        let before_lines = before
            .bytes
            .as_deref()
            .map_or(0, |bytes| String::from_utf8_lossy(bytes).lines().count());
        let after_lines = input.content.lines().count();
        Ok(FileToolResult::text(format!(
            "Lines before: {before_lines}\nLines after: {after_lines}"
        )))
    }
}

pub struct EditTool<S> {
    service: S,
    paths: WorkspacePaths,
    definition: ConfiguredToolDefinition,
}

impl<S: AuthorizedMutationService> EditTool<S> {
    pub fn new(service: S, paths: WorkspacePaths, definition: ConfiguredToolDefinition) -> Self {
        Self {
            service,
            paths,
            definition,
        }
    }

    pub const fn name(&self) -> &'static str {
        "edit"
    }

    pub async fn execute(
        &self,
        invocation: &FileToolInvocation,
        snapshot: &S::Security,
        cancellation: &dyn Cancellation,
    ) -> OperationResult<FileToolResult, FileToolError> {
        let formatter = Formatter::new(invocation);
        let input = match EditInput::parse(&invocation.arguments_json) {
            Ok(input) => input,
            Err(error) => return formatter.error(&error),
        };
        let path = self.paths.expand_path(&input.path);
        if path.is_empty() {
            return formatter.error("Tool argument 'path' must not be empty.");
        }
        if input.old_string.is_empty() {
            return formatter.error("Tool argument 'old_string' must not be empty.");
        }
        if input.old_string.contains('\0') || input.new_string.contains('\0') {
            return formatter.error("Edit arguments must not contain NUL.");
        }
        cancellation
            .check()
            .map_err(|_| OperationError::Cancelled)?;
        let request = MutationRequest {
            requested: path.into(),
            create: false,
        };
        let read = match self
            .service
            .read_for_mutation(&request, snapshot, cancellation)
            .await
        {
            Ok(read) => read,
            Err(error) => return mutation_error(error, &formatter),
        };
        let before = read.bytes.ok_or_else(|| {
            OperationError::Failed(FileToolError::File(
                parrot_domain::contracts::filesystem::FileReadError::new(
                    FileReadErrorKind::NotFound,
                    "Source is missing.",
                ),
            ))
        })?;
        let (has_preamble, text_bytes) = match before.strip_prefix(UTF8_PREAMBLE) {
            Some(bytes) => (true, bytes),
            None => (false, before.as_slice()),
        };
        if text_bytes.contains(&0) {
            return formatter.error("Cannot edit a binary file.");
        }
        let text = match std::str::from_utf8(text_bytes) {
            Ok(text) => text,
            Err(error) => return formatter.error(&error.to_string()),
        };
        let matches = text.matches(&input.old_string).count();
        if !input.replace_all && matches != 1 {
            return formatter.error(&format!(
                "Expected exactly one old_string match, but found {matches}."
            ));
        }
        if matches == 0 {
            return Ok(FileToolResult::text(NO_CHANGES));
        }
        let replaced = if input.replace_all {
            text.replace(&input.old_string, &input.new_string)
        } else {
            text.replacen(&input.old_string, &input.new_string, 1)
        };
        let mut after = Vec::with_capacity(replaced.len() + if has_preamble { 3 } else { 0 });
        if has_preamble {
            after.extend_from_slice(UTF8_PREAMBLE);
        }
        after.extend_from_slice(replaced.as_bytes());
        if before == after {
            return Ok(FileToolResult::text(NO_CHANGES));
        }
        let target = match self
            .service
            .write_for_mutation(&request, &after, snapshot, cancellation)
            .await
        {
            Ok(target) => target,
            Err(error) => return mutation_error(error, &formatter),
        };
        Ok(FileToolResult::text(render(&[FileChange {
            path: &target.display_path,
            before: Some(&before),
            after: Some(&after),
        }])))
    }
}

fn mutation_error(
    error: OperationError<parrot_domain::contracts::filesystem::FileReadError>,
    formatter: &Formatter<'_>,
) -> OperationResult<FileToolResult, FileToolError> {
    match error {
        OperationError::Failed(error) if error.kind != FileReadErrorKind::NotSupported => {
            formatter.error(&error.message)
        }
        error => Err(file_error(error)),
    }
}

macro_rules! mutation_tool {
    ($tool:ident) => {
        impl<S: AuthorizedMutationService + 'static> Tool for $tool<S> {
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
                ParallelSafety::Sequential
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
    };
}

mutation_tool!(WriteTool);
mutation_tool!(EditTool);
