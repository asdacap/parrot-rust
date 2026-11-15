#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScalarPromptTemplateEngine {
    pub(crate) path: String,
    pub(crate) parts: Vec<crate::template::Part>,
}

/// An owned compiled template. Each call creates a fresh native evaluation state.
///
/// The API deliberately cannot accept host objects, functions or MiniJinja values.
///
/// ```compile_fail
/// use parrot_core::{CancellationToken, template::MiniJinjaPromptTemplateEngine};
/// use minijinja::value::Value;
/// fn host_values(engine: &MiniJinjaPromptTemplateEngine, cancellation: CancellationToken<'_>) {
///     engine.render(&Value::from(42), cancellation);
/// }
/// ```
pub struct MiniJinjaPromptTemplateEngine {
    pub(crate) name: String,
    pub(crate) environment: minijinja::Environment<'static>,
}
