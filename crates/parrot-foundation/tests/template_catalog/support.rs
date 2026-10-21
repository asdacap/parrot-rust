use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation, OperationError};
use parrot_foundation::{config::ConfigError, template_catalog::PromptTemplateCatalog};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

pub fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

pub fn fixture(path: &str) -> Value {
    serde_json::from_slice(&std::fs::read(root().join(path)).unwrap()).unwrap()
}

pub fn environment() -> BTreeMap<String, String> {
    [
        ("HOME", "/tmp/c/home"),
        ("XDG_CONFIG_HOME", "/tmp/c/config"),
        ("XDG_STATE_HOME", "/tmp/c/state"),
        ("XDG_DATA_HOME", "/tmp/c/data"),
        ("WORKDIR", "/tmp/c/workspace"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect()
}

pub struct CancelAt {
    calls: AtomicUsize,
    at: usize,
}
impl CancelAt {
    pub fn new(at: usize) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            at,
        }
    }
    #[cfg(test)]
    pub fn never() -> Self {
        Self::new(usize::MAX)
    }
    #[cfg(test)]
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}
impl Cancellation for CancelAt {
    fn is_cancelled(&self) -> bool {
        self.calls.fetch_add(1, Ordering::SeqCst) + 1 >= self.at
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}

pub fn arguments(value: &Value) -> Vec<(String, Value)> {
    value
        .as_object()
        .unwrap()
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

pub fn probe(input: &Value) -> Result<String, OperationError<ConfigError>> {
    let content = input["yaml"].as_str().unwrap_or("");
    let (_, catalog) = PromptTemplateCatalog::from_content(content, &environment())
        .map_err(OperationError::Failed)?;
    let values = if let Some(list) = input["argument_list"].as_array() {
        list.iter()
            .map(|pair| (pair["name"].as_str().unwrap().into(), pair["value"].clone()))
            .collect()
    } else if input["arguments"].is_object() {
        arguments(&input["arguments"])
    } else {
        Vec::new()
    };
    let cancellation = CancelAt::new(if input["cancel"].as_bool() == Some(true) {
        1
    } else {
        usize::MAX
    });
    catalog.render(
        input["template_id"].as_str().unwrap(),
        &values,
        &cancellation,
    )
}

#[cfg(test)]
pub fn error(result: Result<String, OperationError<ConfigError>>) -> String {
    match result {
        Err(OperationError::Failed(error)) => error.0,
        other => panic!("expected configuration error, got {other:?}"),
    }
}

#[cfg(test)]
pub fn native_yaml(text: &str, allowed: &[&str], required: &[&str]) -> String {
    // This creates a single explicit test descriptor; it is not a syntax translator.
    format!(
        "prompt_templates:\n  custom.catalog:\n    engine: minijinja\n    template: {}\n    allowed_arguments: [{}]\n    required_arguments: [{}]\n",
        serde_json::to_string(text).unwrap(),
        allowed.join(", "),
        required.join(", ")
    )
}
