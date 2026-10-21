//! Top-level metadata-only session listing over the already-selected state root.

use std::{cmp::Ordering, io::Write};

use parrot_domain::contracts::async_ops::Cancellation;
use parrot_storage::history_files::{SessionCatalog, SessionCatalogState};

use super::{DispatchError, EXIT_SUCCESS};

pub fn run(
    catalog: &SessionCatalog,
    output: &mut (dyn Write + Send),
    cancellation: &dyn Cancellation,
) -> Result<i32, DispatchError> {
    check(cancellation)?;
    let mut listed = catalog.list();
    listed.sort_by(|left, right| {
        ordinal(left.created_at.as_deref(), right.created_at.as_deref()).then_with(|| {
            left.id
                .value()
                .encode_utf16()
                .cmp(right.id.value().encode_utf16())
        })
    });
    for session in &listed {
        check(cancellation)?;
        if session.state == SessionCatalogState::Corrupt {
            writeln!(output, "{}  <corrupt>", session.id.value())?;
        } else {
            writeln!(
                output,
                "{}  {}  {}",
                session.id.value(),
                session.model,
                session.working_directory
            )?;
        }
    }
    if listed.is_empty() {
        check(cancellation)?;
        writeln!(output, "no sessions")?;
    }
    Ok(EXIT_SUCCESS)
}

fn ordinal(left: Option<&str>, right: Option<&str>) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => left.encode_utf16().cmp(right.encode_utf16()),
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
    }
}

fn check(cancellation: &dyn Cancellation) -> Result<(), DispatchError> {
    cancellation.check().map_err(|_| DispatchError::Cancelled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use parrot_domain::contracts::async_ops::BoxFuture;
    use parrot_storage::history_files::SessionMeta;
    use std::{
        io,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering as AtomicOrdering},
    };

    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Token(bool);
    impl Cancellation for Token {
        fn is_cancelled(&self) -> bool {
            self.0
        }
        fn cancelled(&self) -> BoxFuture<'_, ()> {
            Box::pin(async move {
                if !self.0 {
                    std::future::pending::<()>().await;
                }
            })
        }
    }
    const LIVE: Token = Token(false);

    fn root() -> PathBuf {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(".tmp/t65-session-list")
            .join(format!(
                "{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, AtomicOrdering::Relaxed)
            ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn session(root: &std::path::Path, id: &str, created_at: Option<&str>) -> PathBuf {
        let directory = root.join("sessions").join(id);
        std::fs::create_dir_all(&directory).unwrap();
        let mut metadata = SessionMeta::new(id, "/synthetic/workspace", "provider", "model");
        metadata.created_at = created_at.map(str::to_owned);
        std::fs::write(
            directory.join("meta.json"),
            serde_json::to_vec(&metadata).unwrap(),
        )
        .unwrap();
        directory
    }

    #[test]
    fn empty_selected_catalog_prints_exact_no_sessions_line() {
        let mut output = Vec::new();
        assert_eq!(
            run(&SessionCatalog::new(root()), &mut output, &LIVE).unwrap(),
            EXIT_SUCCESS
        );
        assert_eq!(output, b"no sessions\n");
    }

    #[test]
    fn lists_only_metadata_and_renders_corruption_without_history_or_database_reads() {
        let root = root();
        let valid = session(&root, "valid-session", Some("2026-01-01"));
        std::fs::write(valid.join("history.jsonl"), b"not a history projection").unwrap();
        std::fs::write(valid.join("conversation.db"), b"not a database").unwrap();
        let corrupt = root.join("sessions/corrupt-session");
        std::fs::create_dir_all(&corrupt).unwrap();
        std::fs::write(corrupt.join("meta.json"), b"{invalid").unwrap();
        let mut output = Vec::new();
        assert_eq!(
            run(&SessionCatalog::new(&root), &mut output, &LIVE).unwrap(),
            EXIT_SUCCESS
        );
        assert_eq!(
            output,
            b"corrupt-session  <corrupt>\nvalid-session  model  /synthetic/workspace\n"
        );
        assert_eq!(
            std::fs::read(valid.join("history.jsonl")).unwrap(),
            b"not a history projection"
        );
        assert_eq!(
            std::fs::read(valid.join("conversation.db")).unwrap(),
            b"not a database"
        );
    }

    #[test]
    fn sorts_nullable_created_at_then_id_with_source_utf16_ordinal_comparison() {
        let root = root();
        session(&root, "later-b", Some("2026-01-02"));
        session(&root, "later-a", Some("2026-01-02"));
        session(&root, "earlier", Some("2026-01-01"));
        session(&root, "missing", None);
        session(&root, "empty", Some(""));
        // Ordinal compares the surrogate pair before U+E000, unlike scalar order.
        session(&root, "supplementary", Some("\u{10000}"));
        session(&root, "private-use", Some("\u{e000}"));
        let mut output = Vec::new();
        run(&SessionCatalog::new(root), &mut output, &LIVE).unwrap();
        let output = String::from_utf8(output).unwrap();
        let ids: Vec<_> = output
            .lines()
            .map(|line| line.split_once("  ").unwrap().0)
            .collect();
        assert_eq!(
            ids,
            [
                "missing",
                "empty",
                "earlier",
                "later-a",
                "later-b",
                "supplementary",
                "private-use"
            ]
        );
    }

    #[test]
    fn cancellation_prevents_output_and_writer_failures_propagate() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "synthetic output closed",
                ))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let catalog = SessionCatalog::new(root());
        let mut output = Vec::new();
        assert!(matches!(
            run(&catalog, &mut output, &Token(true)),
            Err(DispatchError::Cancelled)
        ));
        assert!(output.is_empty());
        assert!(
            matches!(run(&catalog, &mut Broken, &LIVE), Err(DispatchError::Io(error)) if error.kind() == io::ErrorKind::BrokenPipe)
        );
    }
}
