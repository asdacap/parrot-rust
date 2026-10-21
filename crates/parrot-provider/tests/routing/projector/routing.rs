use super::{
    common::{array, error, resolved, snapshot, text},
    registry::no_refresh_registry,
};
use parrot_domain::values::context::ContextSize;
use parrot_provider::{
    routing::aliases::{
        ModelAliasCatalog, ModelAliasDefinition, ModelRouter, ModelRouting, ModelRoutingSnapshot,
        ResolvedModelSelection,
    },
    sessions::Error,
};
use serde_json::{Value, json};
use std::sync::Arc;

pub fn aliases(input: &Value) -> Result<Vec<ModelAliasDefinition>, String> {
    array(input, "aliases")
        .iter()
        .map(|a| {
            let mut alias =
                ModelAliasDefinition::new(text(a, "name"), text(a, "target"), text(a, "usage"));
            alias.augment_system_prompt =
                a.get("augment").and_then(Value::as_str).map(str::to_owned);
            alias.context_limit = a
                .get("context_limit")
                .and_then(Value::as_str)
                .map(ContextSize::parse)
                .transpose()
                .map_err(|e| e.to_string())?;
            Ok(alias)
        })
        .collect()
}
fn resolution(requested: &str, result: Result<ResolvedModelSelection, Error>) -> Value {
    match result {
        Ok(value) => json!({"requested":requested,"result":resolved(&value),"error":null}),
        Err(value) => json!({"requested":requested,"result":null,"error":error(&value)}),
    }
}
fn resolve_all(
    input: &Value,
    router: &ModelRouter,
    captured: Option<Arc<ModelRoutingSnapshot>>,
) -> Value {
    Value::Array(
        array(input, "selectors")
            .iter()
            .map(|v| {
                let requested = v.as_str().unwrap_or_default();
                let result = match &captured {
                    Some(snapshot) => router.resolve_from(snapshot.clone(), requested),
                    None => router.resolve(requested),
                };
                resolution(requested, result)
            })
            .collect(),
    )
}
pub async fn run(input: &Value) -> Result<Value, String> {
    let registry = match no_refresh_registry(input) {
        Ok(v) => v,
        Err(e) => return Ok(json!({"error":error(&e)})),
    };
    let catalog = match ModelAliasCatalog::new(registry.clone(), aliases(input)?) {
        Ok(v) => Arc::new(v),
        Err(e) => return Ok(json!({"error":error(&e)})),
    };
    let routing = Arc::new(ModelRouting::new(
        catalog,
        text(input, "configured_default"),
    ));
    if let Some(limit) = input.get("context_limit").and_then(Value::as_str) {
        let mut value = (*routing.capture()).clone();
        value.context_limit = Some(ContextSize::parse(limit).map_err(|e| e.to_string())?);
        routing.publish(Arc::new(value));
    }
    let router = ModelRouter::new(registry, routing.clone());
    let captured = routing.capture();
    let initial = snapshot(&captured);
    let before = resolve_all(input, &router, None);
    let mut replacement_error = Value::Null;
    if let Some(replacement) = input.get("replacement") {
        match routing.prepare(
            text(replacement, "configured_default"),
            aliases(replacement)?,
        ) {
            Ok(value) => routing.publish(value),
            Err(value) => replacement_error = error(&value),
        }
    }
    Ok(
        json!({"initial":initial,"before":before,"replacement_error":replacement_error,"current":snapshot(&routing.capture()),"after":resolve_all(input,&router,None),"captured":resolve_all(input,&router,Some(captured))}),
    )
}
