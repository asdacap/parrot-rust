use super::*;
use parrot_domain::contracts::async_ops::BoxFuture;
use std::sync::atomic::{AtomicBool, Ordering};

struct Flag(AtomicBool);
impl Cancellation for Flag {
    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}

#[test]
fn exact_a008_native_configuration_is_applied() {
    let engine = MiniJinjaPromptTemplateEngine::compile("prompt_templates.settings", "").unwrap();
    let env = &engine.environment;
    assert_eq!(env.undefined_behavior(), UndefinedBehavior::Strict);
    assert!(!env.debug());
    assert_eq!(env.fuel(), Some(1_000_000));
    assert_eq!(env.recursion_limit(), 500);
    assert!(!env.syntax().trim_blocks());
    assert!(!env.syntax().lstrip_blocks());
    assert!(env.syntax().keep_trailing_newline());
}

#[test]
fn writer_keeps_prior_bytes_private_and_uses_a_nonretryable_cancel_error() {
    let flag = Flag(AtomicBool::new(false));
    let mut writer = CancellationWriter {
        cancellation: &flag,
        output: Vec::new(),
        cancelled: false,
    };
    writer.write_all(b"prefix").unwrap();
    flag.0.store(true, Ordering::SeqCst);
    let error = writer.write_all(b"must-not-append").unwrap_err();
    assert_ne!(error.kind(), io::ErrorKind::Interrupted);
    assert_eq!(writer.output, b"prefix");
    assert!(writer.cancelled);
}

#[test]
fn fuel_is_a_fresh_one_million_instruction_budget_for_each_native_state() {
    let engine = MiniJinjaPromptTemplateEngine::compile(
        "prompt_templates.fuel",
        "{% for i in range(1001) %}{{ i }};{% endfor %}",
    )
    .unwrap();
    for _ in 0..2 {
        let captured = engine
            .environment
            .get_template(&engine.name)
            .unwrap()
            .render_captured(())
            .unwrap();
        let (used, remaining) = captured.state().fuel_levels().unwrap();
        assert!(used > 0 && remaining > 0);
        assert_eq!(used + remaining, 1_000_000);
    }
}
