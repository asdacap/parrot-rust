use super::copy::{Fixture, Security, definition};
use parrot_domain::values::tools::ToolDefinitionCatalog;
use parrot_tools::catalog::{ClipboardFactories, ClipboardServices};
use std::collections::BTreeMap;

#[tokio::test]
async fn clipboard_copy_is_supported_only_for_interactive_top_level_sessions() {
    for (interactive, depth, expected) in [
        (true, 0, true),
        (true, 1, false),
        (false, 0, false),
        (false, 1, false),
    ] {
        let fixture = Fixture::new();
        let (history, publisher) = fixture.services();
        let mut entries = BTreeMap::new();
        entries.insert("clipboard_copy".to_owned(), definition());
        let definitions = ToolDefinitionCatalog::new(&entries);
        let factories = ClipboardFactories::<Security, ()>::new(
            ClipboardServices {
                history,
                publisher,
                interactive,
                agent_depth: depth,
            },
            &definitions,
        )
        .unwrap();
        assert_eq!(factories.clipboard().name(), "clipboard_copy");
        assert_eq!(
            factories.clipboard().supports(),
            expected,
            "interactive={interactive} depth={depth}"
        );
    }
}
