use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_foundation::oauth::AuthError;
use parrot_provider::{
    catalogs::{Model, ModelVariant},
    http_stream::HttpError,
    routing::{
        aliases::{ModelAliasDefinition, ModelRoutingSnapshot, ResolvedModelSelection},
        configuration::CoordinatorError,
        registry::ProviderModel,
    },
    sessions::Error,
};
use serde_json::{Value, json};
use std::error::Error as StdError;

pub struct Token(pub bool);
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
pub const ACTIVE: Token = Token(false);
pub fn text<'a>(value: &'a Value, name: &str) -> &'a str {
    value.get(name).and_then(Value::as_str).unwrap_or_default()
}
pub fn boolean(value: &Value, name: &str, default: bool) -> bool {
    value.get(name).and_then(Value::as_bool).unwrap_or(default)
}
pub fn integer(value: &Value, name: &str, default: i64) -> i64 {
    value.get(name).and_then(Value::as_i64).unwrap_or(default)
}
pub fn array<'a>(value: &'a Value, name: &str) -> &'a [Value] {
    value
        .get(name)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}
pub fn model(value: &Model) -> Value {
    json!({"id":value.id,"provider_id":value.provider_id,"name":value.name,
        "context_window":value.context_window,"max_output_tokens":value.max_output_tokens,
        "max_input_tokens":value.max_input_tokens,"input_token_limit":value.input_token_limit(),
        "input_price":value.input_price,"cached_input_price":value.cached_input_price,"output_price":value.output_price,
        "tools":value.capabilities.tools,"reasoning":value.capabilities.reasoning,"output":value.capabilities.output,
        "variants":value.capabilities.variants.iter().map(|v|json!({"name":v.name,"effort":v.reasoning_effort})).collect::<Vec<_>>(),
        "fields":value.fields.bits()})
}
pub fn seed_model(provider: &str, input: &Value) -> Model {
    let mut value = Model::new(text(input, "id"), provider);
    value.name = text(input, "name").into();
    value.context_window = integer(input, "context_window", 0) as i32;
    value.max_output_tokens = integer(input, "max_output_tokens", 0) as i32;
    value.max_input_tokens = integer(input, "max_input_tokens", 0) as i32;
    value.capabilities.tools = boolean(input, "tools", false);
    value.capabilities.reasoning = boolean(input, "reasoning", false);
    value.capabilities.output = array(input, "output")
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    value.capabilities.variants = array(input, "variants")
        .iter()
        .map(|v| ModelVariant::new(text(v, "name"), text(v, "effort")))
        .collect();
    value
}
pub fn selection(value: &ProviderModel) -> Value {
    json!({"selector":value.selector(),"provider_id":value.provider.id(),"model":model(&value.model),
        "variant":value.variant.as_ref().map(|v|json!({"name":v.name,"effort":v.reasoning_effort})),
        "reasoning":value.reasoning().map(|v|json!({"effort":v.effort,"summary":v.summary}))})
}
pub fn alias(value: &ModelAliasDefinition) -> Value {
    json!({"name":value.name,"target":value.model_string,"usage":value.usage,
        "augment":value.augment_system_prompt,"context_limit":value.context_limit.as_ref().map(ToString::to_string)})
}
pub fn snapshot(value: &ModelRoutingSnapshot) -> Value {
    json!({"configured_default":value.configured_default_selector,"revision":value.revision,
        "context_limit":value.context_limit.as_ref().map(ToString::to_string),
        "aliases":value.aliases.definitions().iter().map(alias).collect::<Vec<_>>()})
}
pub fn resolved(value: &ResolvedModelSelection) -> Value {
    json!({"requested":value.requested_selector.value,"alias":value.alias.as_ref().map(|alias| &alias.name),
        "canonical":selection(&value.canonical_model),"canonical_base":value.canonical_base(),
        "context_limit":value.context_limit().map(ToString::to_string),"revision":value.routing_snapshot.revision})
}

/// Explicit source-type counterpart identities for structured port errors.
/// Native Rust identity/Debug/Display/source chains are independently retained on stderr.
pub fn error(value: &Error) -> Value {
    let counterpart = match value {
        Error::Provider(_)
        | Error::ProviderWithCause { .. }
        | Error::Http(HttpError::Provider(_)) => "Parrot.Llm.LLMProviderException",
        Error::HeaderTimeout(_)
        | Error::HeaderRetryLimit { .. }
        | Error::Http(HttpError::HeaderTimeout(_)) => "Parrot.Llm.Wire.HeaderTimeoutException",
        Error::Http(HttpError::Response(_) | HttpError::ProviderHttp(_)) => {
            "Parrot.Llm.Wire.ProviderHttpException"
        }
        Error::Http(HttpError::RequestCancelled(_)) => {
            "System.Threading.Tasks.TaskCanceledException"
        }
        Error::Http(HttpError::Http(_) | HttpError::Transport(_)) => {
            "System.Net.Http.HttpRequestException"
        }
        Error::Wire(_) | Error::Http(HttpError::Wire(_)) => "Parrot.Llm.Wire.WireProtocolException",
        Error::Cancelled | Error::Cancellation(_) | Error::Http(HttpError::Cancelled) => {
            "System.OperationCanceledException"
        }
        Error::Io(_) => "System.IO.IOException",
        Error::InvalidOperation(_) => "System.InvalidOperationException",
        Error::Authentication(auth) => match auth {
            AuthError::Message(_)
            | AuthError::Http { .. }
            | AuthError::Credential(_)
            | AuthError::BrowserLaunch(_) => "Parrot.Auth.AuthException",
            AuthError::Transport | AuthError::TransportWithMessage(_) => {
                "System.Net.Http.HttpRequestException"
            }
            AuthError::Io | AuthError::IoWithMessage(_) => "System.IO.IOException",
            AuthError::InvalidOperation(_) => "System.InvalidOperationException",
            AuthError::Format | AuthError::FormatWithMessage(_) => "System.FormatException",
            AuthError::ArgumentOutOfRange(_)
            | AuthError::ArgumentOutOfRangeWithParameter { .. } => {
                "System.ArgumentOutOfRangeException"
            }
            AuthError::NotSupported(_) => "System.NotSupportedException",
        },
        Error::Http(HttpError::RequestTooLarge(_)) => {
            "Parrot.Llm.Wire.ProviderRequestTooLargeException"
        }
        Error::Disposed | Error::Http(HttpError::Disposed) => "System.ObjectDisposedException",
        // Parser diagnostics are actual native Rust observations, not fabricated CLR values.
        Error::Encode(_) | Error::Http(HttpError::InvalidJsonString(_)) => {
            "parrot_provider::sessions::Error"
        }
        Error::Response(_)
        | Error::ChatProvider(_)
        | Error::Socket(_)
        | Error::WebSocketTransport { .. } => "parrot_provider::sessions::Error",
    };
    native_error(value);
    json!({"type":counterpart,"message":value.to_string()})
}
fn native_error(value: &Error) {
    let mut sources = Vec::new();
    let mut source = value.source();
    while let Some(cause) = source {
        sources.push(cause.to_string());
        source = cause.source();
    }
    eprintln!(
        "{}",
        json!({"native_error":{"type":"parrot_provider::sessions::Error","debug":format!("{value:?}"),"display":value.to_string(),"source_chain":sources}})
    );
}
pub fn coordinator_error(value: &CoordinatorError) -> Value {
    let mut sources = Vec::new();
    let mut source = value.source();
    while let Some(cause) = source {
        sources.push(cause.to_string());
        source = cause.source();
    }
    eprintln!(
        "{}",
        json!({"native_error":{"type":"parrot_provider::routing::configuration::CoordinatorError","debug":format!("{value:?}"),"display":value.to_string(),"source_chain":sources}})
    );
    match value {
        CoordinatorError::Provider(value) => error(value),
        CoordinatorError::Foundation(value) => {
            json!({"type":"System.IO.InvalidDataException","message":value.to_string()})
        }
        CoordinatorError::MissingPreset(_) => {
            json!({"type":"System.Collections.Generic.KeyNotFoundException","message":value.to_string()})
        }
        CoordinatorError::Icon(_) => {
            json!({"type":"System.ArgumentException","message":value.to_string()})
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_counterparts_follow_typed_origins_not_messages() {
        let message = "same untouched message";
        for (native, expected) in [
            (
                HttpError::ProviderHttp(message.into()),
                "Parrot.Llm.Wire.ProviderHttpException",
            ),
            (
                HttpError::RequestCancelled(message.into()),
                "System.Threading.Tasks.TaskCanceledException",
            ),
            (
                HttpError::Http(message.into()),
                "System.Net.Http.HttpRequestException",
            ),
        ] {
            let observation = error(&Error::Http(native));
            assert_eq!(observation["type"], expected);
            assert_eq!(observation["message"], message);
        }
        assert_eq!(
            error(&Error::Http(HttpError::Cancelled))["type"],
            "System.OperationCanceledException"
        );
    }
}
