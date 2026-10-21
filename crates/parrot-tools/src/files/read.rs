use std::path::Path;

use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation, OperationError, OperationResult},
        filesystem::{
            AuthorizedReadService, EntryKind, FileMetadata, FileReadErrorKind, LinkPolicy,
            ReadRequest,
        },
        security::InvocationContext,
        tools::{ParallelSafety, Tool},
    },
    values::tools::ConfiguredToolDefinition,
};

use super::{
    FileToolError, FileToolInvocation, FileToolResult, Formatter, WorkspacePaths, file_error,
    input::ReadInput, outline::MarkdownOutline,
};

const MAX_LINES: i32 = 2000;
const MAX_OUTPUT_BYTES: usize = 8192;
const BINARY_PROBE_SIZE: usize = 8192;

pub struct ReadTool<S> {
    service: S,
    paths: WorkspacePaths,
    definition: ConfiguredToolDefinition,
}

impl<S: AuthorizedReadService> ReadTool<S> {
    pub fn new(service: S, paths: WorkspacePaths, definition: ConfiguredToolDefinition) -> Self {
        Self {
            service,
            paths,
            definition,
        }
    }

    pub const fn name(&self) -> &'static str {
        "read"
    }

    pub async fn execute(
        &self,
        invocation: &FileToolInvocation,
        snapshot: &S::Security,
        cancellation: &dyn Cancellation,
    ) -> OperationResult<FileToolResult, FileToolError> {
        let formatter = Formatter::new(invocation);
        let input = match ReadInput::parse(&invocation.arguments_json) {
            Ok(input) => input,
            Err(error) => return formatter.error(&error),
        };
        if input.offset < 1 {
            return formatter.error("offset must be at least 1");
        }
        if !(1..=MAX_LINES).contains(&input.limit) {
            return formatter.error("limit must be between 1 and 2000");
        }
        let expanded = self.paths.expand_path(&input.path);
        let path = Path::new(&expanded);
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
        if metadata.kind == EntryKind::Directory {
            return self
                .list_directory(&metadata, snapshot, cancellation, &formatter)
                .await
                .map(FileToolResult::text);
        }
        if metadata.kind != EntryKind::Regular {
            return formatter.error("no such file or directory");
        }
        let request = ReadRequest {
            requested: path.to_owned(),
            expected_physical: Some(metadata.physical_path.clone()),
            links: LinkPolicy::Follow,
            max_bytes: None,
        };
        let read = self
            .service
            .read(&request, snapshot, cancellation)
            .await
            .map_err(file_error)?;
        render_file(
            &read.physical_path,
            &read.bytes,
            input.offset,
            input.limit,
            cancellation,
            &formatter,
        )
        .map(FileToolResult::text)
    }

    async fn list_directory(
        &self,
        directory: &FileMetadata,
        snapshot: &S::Security,
        cancellation: &dyn Cancellation,
        formatter: &Formatter<'_>,
    ) -> OperationResult<String, FileToolError> {
        let mut entries = self
            .service
            .list_directory(
                &directory.lexical_path,
                LinkPolicy::Follow,
                snapshot,
                cancellation,
            )
            .await
            .map_err(file_error)?;
        entries.sort_by(|left, right| {
            ordinal_path(&left.lexical_path).cmp(&ordinal_path(&right.lexical_path))
        });
        let mut output = String::new();
        let mut count = 0;
        for entry in entries {
            let resolved = match self
                .service
                .inspect(
                    &entry.lexical_path,
                    LinkPolicy::Follow,
                    snapshot,
                    cancellation,
                )
                .await
            {
                Ok(resolved) => resolved,
                Err(OperationError::Failed(error))
                    if error.is_io() || error.kind == FileReadErrorKind::InvalidOperation =>
                {
                    continue;
                }
                Err(error) => return Err(file_error(error)),
            };
            if count >= MAX_LINES || output.encode_utf16().count() >= MAX_OUTPUT_BYTES {
                output.push_str(&formatter.text("[listing truncated]\n")?);
                break;
            }
            output.push_str(
                &entry
                    .lexical_path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy(),
            );
            if resolved.kind == EntryKind::Directory {
                output.push('/');
            }
            output.push('\n');
            count += 1;
        }
        Ok(output)
    }
}

impl<S: AuthorizedReadService + 'static> Tool for ReadTool<S> {
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

fn ordinal_path(path: &Path) -> Vec<u16> {
    path.to_string_lossy().encode_utf16().collect()
}

pub(super) fn render_file(
    physical_path: &Path,
    bytes: &[u8],
    offset: i32,
    limit: i32,
    cancellation: &dyn Cancellation,
    formatter: &Formatter<'_>,
) -> OperationResult<String, FileToolError> {
    if bytes[..bytes.len().min(BINARY_PROBE_SIZE)].contains(&0) {
        return formatter.error("binary file").map(|result| result.text);
    }
    let text = crate::task_artifacts::decode_stream_reader_bytes(bytes);
    let mut output = String::new();
    let mut output_bytes = 0;
    let mut line_number = 0_i32;
    let mut truncated = false;
    let markdown = physical_path.extension().is_some_and(|extension| {
        extension.eq_ignore_ascii_case("md") || extension.eq_ignore_ascii_case("markdown")
    });
    let mut outline = MarkdownOutline::new();
    let mut start = 0;
    let text_bytes = text.as_bytes();
    while start < text.len() {
        cancellation
            .check()
            .map_err(|_| OperationError::Cancelled)?;
        let mut end = start;
        while end < text.len() && text_bytes[end] != b'\r' && text_bytes[end] != b'\n' {
            end += 1;
        }
        let line = &text[start..end];
        line_number = line_number.wrapping_add(1);
        if markdown {
            outline.record(line_number as usize, line);
        }
        if line_number >= offset && line_number < offset.wrapping_add(limit) && !truncated {
            let formatted = format!("{line_number}: {line}\n");
            if output_bytes + formatted.len() > MAX_OUTPUT_BYTES {
                truncated = true;
            } else {
                output_bytes += formatted.len();
                output.push_str(&formatted);
            }
        }
        start = end;
        if start < text.len() {
            let carriage_return = text_bytes[start] == b'\r';
            start += 1;
            if carriage_return && text_bytes.get(start) == Some(&b'\n') {
                start += 1;
            }
        }
    }
    if truncated {
        output.push_str(&formatter.text("[output truncated]\n")?);
    }
    output.push_str(&format!("total lines in file: {line_number}\n"));
    if markdown
        && !outline.is_empty()
        && (truncated || offset > 1 || line_number >= offset.wrapping_add(limit))
    {
        output.push_str(&formatter.outline(&outline.render())?);
    }
    Ok(output)
}
