use parrot_cli::slash_sessions::{
    CommandContext, SlashCommand, SlashCommandCompletion, SlashCommandRegistry, SlashFailure,
};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};

struct CompletionCommand {
    name: &'static str,
    summary: &'static str,
}

impl SlashCommand for CompletionCommand {
    fn name(&self) -> &str {
        self.name
    }
    fn summary(&self) -> &str {
        self.summary
    }
    fn run<'a>(
        &'a self,
        _: &'a str,
        _: &'a mut CommandContext<'_>,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async { panic!("completion must not dispatch a command") })
    }
}

fn command(name: &'static str, summary: &'static str) -> Box<dyn SlashCommand> {
    Box::new(CompletionCommand { name, summary })
}

#[test]
fn completion_filters_wraps_preserves_selection_and_accepts_command_token() {
    let registry = SlashCommandRegistry::new(vec![
        command("/model", "model"),
        command("/mode", "mode"),
        command("/compact", "compact"),
    ]);
    let mut completion = SlashCommandCompletion::new();
    completion.refresh("/mo", &registry);
    completion.select_next();
    completion.refresh("/mod", &registry);
    assert_eq!(
        completion
            .commands()
            .iter()
            .map(|command| command.name.as_str())
            .collect::<Vec<_>>(),
        ["/mode", "/model"]
    );
    assert_eq!(completion.selected(), 1);
    assert_eq!(
        completion.accept("/mod trailing text").as_deref(),
        Some("/model trailing text")
    );
    completion.refresh("/com", &registry);
    assert_eq!(
        completion
            .commands()
            .iter()
            .map(|command| command.name.as_str())
            .collect::<Vec<_>>(),
        ["/compact"]
    );
    completion.select_next();
    assert_eq!(completion.selected(), 0);
    completion.select_previous();
    assert_eq!(completion.selected(), 0);
    completion.refresh("/model arguments", &registry);
    assert!(completion.commands().is_empty());
    completion.refresh("normal prompt", &registry);
    assert!(completion.commands().is_empty());
    assert!(completion.accept("/unknown").is_none());
}

#[test]
fn completion_retains_command_object_index_even_for_duplicate_spellings() {
    let registry = SlashCommandRegistry::new(vec![
        command("/mode", "first object"),
        command("/model", "model"),
        command("/mode", "second object"),
    ]);
    let mut completion = SlashCommandCompletion::new();
    completion.refresh("/mo", &registry);
    completion.select_next();
    assert_eq!(
        completion.commands()[completion.selected()].summary,
        "second object"
    );
    completion.refresh("/mod", &registry);
    assert_eq!(completion.selected(), 1);
    assert_eq!(
        completion.commands()[completion.selected()].summary,
        "second object"
    );
    completion.select_previous();
    assert_eq!(completion.selected(), 0);
    completion.select_previous();
    assert_eq!(completion.selected(), 2);
    completion.select_next();
    assert_eq!(completion.selected(), 0);
}

#[test]
fn completion_uses_ordinal_prefix_and_exact_ascii_delimiters_and_suffixes() {
    let registry = SlashCommandRegistry::new(vec![
        command("/mode", "mode"),
        command("/Mode", "uppercase"),
    ]);
    let mut completion = SlashCommandCompletion::new();
    completion.refresh("/M", &registry);
    assert_eq!(completion.commands()[0].name, "/Mode");
    completion.refresh("/m", &registry);
    assert_eq!(completion.commands()[0].name, "/mode");
    for separator in [' ', '\t', '\r', '\n'] {
        assert_eq!(
            completion
                .accept(&format!("/m{separator}  argument"))
                .as_deref(),
            Some(format!("/mode{separator}  argument").as_str())
        );
        completion.refresh(&format!("/m{separator}"), &registry);
        assert!(completion.commands().is_empty());
        completion.select_next();
        completion.select_previous();
        assert_eq!(completion.selected(), 0);
        completion.refresh("/m", &registry);
    }
    completion.refresh("/m\u{a0}", &registry);
    assert!(completion.commands().is_empty());
    completion.refresh("/", &registry);
    assert_eq!(
        completion
            .commands()
            .iter()
            .map(|command| command.name.as_str())
            .collect::<Vec<_>>(),
        ["/Mode", "/mode"]
    );
    completion.refresh("", &registry);
    assert!(completion.commands().is_empty());
}
