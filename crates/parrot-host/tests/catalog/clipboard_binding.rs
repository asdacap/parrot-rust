use std::{collections::BTreeMap, sync::Arc};

use super::binding::clipboard_history;
use super::support::Snapshot;
use parrot_domain::values::tools::{ConfiguredToolDefinition, ToolDefinitionCatalog};
use parrot_host::composition::catalog::{ClipboardFactories, ClipboardServices};
use parrot_runtime::event_broker::EventBroker;

#[tokio::test]
async fn clipboard_copy_is_supported_only_for_interactive_top_level_sessions() {
    for (interactive, depth, expected) in [
        (true, 0, true),
        (true, 1, false),
        (false, 0, false),
        (false, 1, false),
    ] {
        let mut entries = BTreeMap::new();
        entries.insert(
            "clipboard_copy".to_owned(),
            ConfiguredToolDefinition::new(
                "Copy text to the user's clipboard with the terminal-native OSC 52 sequence.",
                r#"{"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false}"#,
            ),
        );
        let definitions = ToolDefinitionCatalog::new(&entries);
        let factories = ClipboardFactories::<Snapshot, ()>::new(
            ClipboardServices {
                history: clipboard_history(),
                publisher: Arc::new(EventBroker::new()),
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
