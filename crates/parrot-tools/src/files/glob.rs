use std::{
    path::Path,
    time::{Duration, Instant},
};

use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation, OperationError, OperationResult},
        filesystem::{
            AuthorizedReadService, EntryKind, FileMetadata, FileReadErrorKind, LinkPolicy,
        },
        security::InvocationContext,
        tools::{ParallelSafety, Tool},
    },
    values::tools::ConfiguredToolDefinition,
};

use super::{
    FileToolError, FileToolInvocation, FileToolResult, Formatter, file_error, input::GlobInput,
};

const MAX_RESULTS: usize = 1000;
const MAX_VISITED: usize = 100_000;
const TIMEOUT: Duration = Duration::from_secs(5);

pub struct GlobTool<S> {
    service: S,
    definition: ConfiguredToolDefinition,
}

impl<S: AuthorizedReadService> GlobTool<S> {
    pub fn new(service: S, definition: ConfiguredToolDefinition) -> Self {
        Self {
            service,
            definition,
        }
    }
    pub const fn name(&self) -> &'static str {
        "glob"
    }

    pub async fn execute(
        &self,
        invocation: &FileToolInvocation,
        snapshot: &S::Security,
        cancellation: &dyn Cancellation,
    ) -> OperationResult<FileToolResult, FileToolError> {
        let formatter = Formatter::new(invocation);
        let input = match GlobInput::parse(&invocation.arguments_json) {
            Ok(input) => input,
            Err(error) => return formatter.error(&error),
        };
        if input.pattern.is_empty() || input.pattern.contains('\0') {
            return formatter.error("glob pattern must be a non-empty string without NUL");
        }
        if Path::new(&input.pattern).is_absolute() || input.pattern.contains("..") {
            return formatter
                .error("glob pattern must be a relative search-root path without traversal");
        }
        let path = Path::new(if input.path.is_empty() {
            "."
        } else {
            &input.path
        });
        let root = match self
            .service
            .inspect(path, LinkPolicy::Follow, snapshot, cancellation)
            .await
        {
            Ok(root) => root,
            Err(OperationError::Failed(error)) if error.kind == FileReadErrorKind::NotFound => {
                return formatter.error("no such directory");
            }
            Err(OperationError::Failed(error))
                if error.is_io() || error.kind == FileReadErrorKind::InvalidOperation =>
            {
                return formatter.error(&error.message);
            }
            Err(error) => return Err(file_error(error)),
        };
        if root.kind != EntryKind::Directory {
            return formatter.error("no such directory");
        }
        let pattern = Pattern::new(&input.pattern);
        match tokio::time::timeout(
            TIMEOUT,
            self.walk(
                root,
                &pattern,
                snapshot,
                cancellation,
                &formatter,
                Instant::now() + TIMEOUT,
            ),
        )
        .await
        {
            Ok(Err(OperationError::Failed(FileToolError::GlobTimeout))) => {
                formatter.error("glob timed out")
            }
            Ok(result) => result.map(FileToolResult::text),
            Err(_) if cancellation.is_cancelled() => Err(OperationError::Cancelled),
            Err(_) => formatter.error("glob timed out"),
        }
    }

    async fn walk(
        &self,
        root: FileMetadata,
        pattern: &Pattern,
        snapshot: &S::Security,
        cancellation: &dyn Cancellation,
        formatter: &Formatter<'_>,
        deadline: Instant,
    ) -> OperationResult<String, FileToolError> {
        enum Work {
            Directory(FileMetadata, String),
            Entry(FileMetadata, String),
        }
        let mut work = vec![Work::Directory(root, String::new())];
        let mut results = Vec::new();
        let mut visited = 0;
        let mut stop = None;
        while let Some(next) = work.pop() {
            check_deadline(cancellation, deadline)?;
            match next {
                Work::Directory(directory, relative) => {
                    let mut entries = match self
                        .service
                        .list_directory(
                            &directory.lexical_path,
                            LinkPolicy::Follow,
                            snapshot,
                            cancellation,
                        )
                        .await
                    {
                        Ok(entries) => entries,
                        Err(OperationError::Failed(error))
                            if error.is_io() || error.kind == FileReadErrorKind::Denied =>
                        {
                            continue;
                        }
                        Err(error) => return Err(file_error(error)),
                    };
                    entries.sort_by_key(|entry| {
                        entry
                            .lexical_path
                            .to_string_lossy()
                            .encode_utf16()
                            .collect::<Vec<_>>()
                    });
                    for entry in entries.into_iter().rev() {
                        work.push(Work::Entry(entry, relative.clone()));
                    }
                }
                Work::Entry(entry, directory) => {
                    if results.len() > MAX_RESULTS {
                        stop = Some("result");
                        break;
                    }
                    if visited >= MAX_VISITED {
                        stop = Some("visit");
                        break;
                    }
                    visited += 1;
                    let name = entry
                        .lexical_path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy();
                    let relative = if directory.is_empty() {
                        name.into_owned()
                    } else {
                        format!("{directory}/{name}")
                    };
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
                            if error.is_io()
                                || error.kind == FileReadErrorKind::InvalidOperation =>
                        {
                            continue;
                        }
                        Err(error) => return Err(file_error(error)),
                    };
                    let real_directory = entry.kind == EntryKind::Directory;
                    if pattern.matches(&relative, || check_deadline(cancellation, deadline))? {
                        results.push(
                            if real_directory
                                || (entry.kind == EntryKind::SymbolicLink
                                    && resolved.kind == EntryKind::Directory)
                            {
                                format!("{relative}/")
                            } else {
                                relative.clone()
                            },
                        );
                        if results.len() > MAX_RESULTS {
                            stop = Some("result");
                            break;
                        }
                    }
                    if real_directory {
                        work.push(Work::Directory(resolved, relative));
                    }
                }
            }
        }
        results.sort_by_key(|value| value.encode_utf16().collect::<Vec<_>>());
        results.truncate(MAX_RESULTS);
        let mut output = if results.is_empty() {
            String::new()
        } else {
            format!("{}\n", results.join("\n"))
        };
        if let Some(reason) = stop {
            output.push_str(&formatter.text(&format!(
                "[glob results truncated: {reason} limit reached]\n"
            ))?);
        }
        Ok(output)
    }
}

impl<S: AuthorizedReadService + 'static> Tool for GlobTool<S> {
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

#[derive(Clone, Copy)]
enum Token {
    Literal(u16),
    Character,
    Segment,
    Any,
    Prefix,
}

struct Pattern(Vec<Token>);
impl Pattern {
    fn new(pattern: &str) -> Self {
        let units = pattern.encode_utf16().collect::<Vec<_>>();
        let mut tokens = Vec::new();
        let mut index = 0;
        while index < units.len() {
            match units[index] {
                42 if units.get(index + 1) == Some(&42) => {
                    index += 2;
                    if units.get(index) == Some(&47) {
                        tokens.push(Token::Prefix);
                        index += 1;
                    } else {
                        tokens.push(Token::Any);
                    }
                }
                42 => {
                    tokens.push(Token::Segment);
                    index += 1;
                }
                63 => {
                    tokens.push(Token::Character);
                    index += 1;
                }
                unit => {
                    tokens.push(Token::Literal(unit));
                    index += 1;
                }
            }
        }
        Self(tokens)
    }

    fn matches(
        &self,
        value: &str,
        mut check: impl FnMut() -> OperationResult<(), FileToolError>,
    ) -> OperationResult<bool, FileToolError> {
        let text = value.encode_utf16().collect::<Vec<_>>();
        let mut current = vec![false; text.len() + 1];
        current[0] = true;
        for token in &self.0 {
            check()?;
            let mut next = vec![false; text.len() + 1];
            match token {
                Token::Literal(unit) => {
                    for index in 0..text.len() {
                        if index % 128 == 0 {
                            check()?;
                        }
                        next[index + 1] = current[index] && text[index] == *unit;
                    }
                }
                Token::Character => {
                    for index in 0..text.len() {
                        if index % 128 == 0 {
                            check()?;
                        }
                        next[index + 1] = current[index] && text[index] != 47;
                    }
                }
                Token::Segment | Token::Any => {
                    next.clone_from(&current);
                    for index in 0..text.len() {
                        if index % 128 == 0 {
                            check()?;
                        }
                        if next[index]
                            && match token {
                                Token::Segment => text[index] != 47,
                                _ => text[index] != 10,
                            }
                        {
                            next[index + 1] = true;
                        }
                    }
                }
                Token::Prefix => {
                    next.clone_from(&current);
                    let mut active = false;
                    for index in 0..text.len() {
                        if index % 128 == 0 {
                            check()?;
                        }
                        active |= current[index];
                        if text[index] == 10 {
                            active = false;
                        } else if active && text[index] == 47 {
                            next[index + 1] = true;
                        }
                    }
                }
            }
            current = next;
        }
        Ok(current[text.len()] || (text.last() == Some(&10) && current[text.len() - 1]))
    }
}

fn check_deadline(
    cancellation: &dyn Cancellation,
    deadline: Instant,
) -> OperationResult<(), FileToolError> {
    cancellation
        .check()
        .map_err(|_| OperationError::Cancelled)?;
    if Instant::now() >= deadline {
        return Err(OperationError::Failed(FileToolError::GlobTimeout));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Pattern;

    #[test]
    fn synchronous_pattern_matching_checks_its_cancellation_boundary() {
        let mut checks = 0;
        let result = Pattern::new("**?**?**?**?**").matches(&"x".repeat(2048), || {
            checks += 1;
            if checks == 3 {
                Err(parrot_domain::contracts::async_ops::OperationError::Cancelled)
            } else {
                Ok(())
            }
        });
        assert_eq!(
            result,
            Err(parrot_domain::contracts::async_ops::OperationError::Cancelled)
        );
        assert_eq!(checks, 3);
    }

    #[test]
    fn source_glob_translation_matches_utf16_and_regex_anchor_rules() {
        for (pattern, value, expected) in [
            ("*.rs", "a.rs", true),
            ("*.rs", "src/a.rs", false),
            ("**/*.rs", "a.rs", true),
            ("**/*.rs", "src/a.rs", true),
            ("?", "😀", false),
            ("??", "😀", true),
            ("a", "a\n", true),
            ("**", "a\nb", false),
            ("*", "a\nb", true),
            ("**/x", "a/b/x", true),
            ("**/x", "a\nb/x", false),
            ("[x]", "[x]", true),
        ] {
            assert_eq!(
                Pattern::new(pattern).matches(value, || Ok(())).unwrap(),
                expected,
                "{pattern:?} {value:?}"
            );
        }
    }
}
