use super::Error;
use std::collections::BTreeSet;

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
/// use parrot_core::{CancellationToken, MiniJinjaPromptTemplateEngine};
/// use minijinja::value::Value;
/// fn host_values(engine: &MiniJinjaPromptTemplateEngine, cancellation: CancellationToken<'_>) {
///     engine.render(&Value::from(42), cancellation);
/// }
/// ```
pub struct MiniJinjaPromptTemplateEngine {
    pub(crate) name: String,
    pub(crate) environment: minijinja::Environment<'static>,
}

/// Validates a catalog call without imposing a map's sorting order on source argument names.
/// `names` and `required` retain the exact supplied order for duplicate/diagnostic behavior.
pub fn validate_arguments(
    path: &str,
    allowed: &BTreeSet<String>,
    required: &[String],
    names: &[String],
) -> Result<(), Error> {
    let mut seen = BTreeSet::new();
    for name in names {
        if !seen.insert(name.as_str()) {
            return Err(Error::template(format!(
                "{path} received duplicate argument '{name}'"
            )));
        }
    }
    for name in names {
        if !allowed.contains(name) {
            return Err(Error::template(format!(
                "{path} does not allow argument '{name}'"
            )));
        }
    }
    for name in required {
        if !seen.contains(name.as_str()) {
            return Err(Error::template(format!(
                "{path} requires argument '{name}'"
            )));
        }
    }
    Ok(())
}
