//! Input-only usage observations over the real selected provider capabilities.
//! Fixture constants are published in source/collaborators.json, usage_collaborators.
use super::common;
use chrono::{DateTime, Timelike, Utc};
use parrot_domain::contracts::async_ops::{
    BoxFuture, Cancellation, OperationError, OperationResult, OwnedWork,
};
use parrot_foundation::{
    config::models::{ModelConfig, ProviderConfig},
    credentials::{Credential, OAuthCredential},
    oauth::{AuthError, CredentialStorage, DeviceAuthorization, OAuthClient},
};
use parrot_provider::{
    catalogs::Model,
    http_stream::{ByteStream, HttpError, HttpMethod, HttpRequest, HttpResponse, HttpTransport},
    image_generation::{ImageHttpRequest, ImageTransport},
    responses_websocket::{ConnectedSocket, Connector, Headers, WebSocketError},
    routing::{
        auth::FoundationCredentials,
        factory::{ProviderBuildContext, build_provider},
        usage::{KimiUsageReporter, OpenCodeGoUsageReporter},
    },
    sessions::{
        Error, Provider,
        decorators::{
            RetryingProvider, SubscriptionUsage, UsageReporter, UsageReportingProvider, UsageWindow,
        },
        provider::{
            ApiKeySource, ChatGptOptions, ChatGptProvider, CompatibleOptions, OAuthAccess,
            OAuthTokenSource, OpenAICompatibleProvider,
        },
    },
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

const BASE_URL: &str = "https://example.test/v1";
const DECLARED_MODEL: &str = "declared-model";

struct Body {
    bytes: Vec<u8>,
    offset: usize,
}
impl ByteStream for Body {
    fn read<'a>(
        &'a mut self,
        buffer: &'a mut [u8],
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(HttpError::Cancelled);
            }
            let length = buffer.len().min(self.bytes.len() - self.offset);
            buffer[..length].copy_from_slice(&self.bytes[self.offset..self.offset + length]);
            self.offset += length;
            Ok(length)
        })
    }
}
struct UsageTransport {
    body: String,
    requests: Mutex<Vec<Value>>,
}
impl UsageTransport {
    fn records(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }
}
impl HttpTransport for UsageTransport {
    fn send<'a>(
        &'a self,
        request: HttpRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpError>> {
        Box::pin(async move {
            let mut headers = BTreeMap::new();
            for (name, value) in request.headers {
                let name = name.to_ascii_lowercase();
                headers
                    .entry(name)
                    .and_modify(|existing: &mut String| {
                        existing.push_str(", ");
                        existing.push_str(&value);
                    })
                    .or_insert(value);
            }
            // This usage fixture records no-content requests as an empty string,
            // unlike the main family fixture's null. The bytes are not reparsed.
            let body_raw = String::from_utf8(request.body)
                .map_err(|error| HttpError::Transport(error.to_string()))?;
            self.requests.lock().unwrap().push(json!({
                "method": match request.method { HttpMethod::Get => "GET", HttpMethod::Post => "POST" },
                "uri": request.endpoint.as_str(), "headers": headers, "body_raw": body_raw,
            }));
            if cancellation.is_cancelled() {
                return Err(HttpError::Cancelled);
            }
            Ok(HttpResponse {
                status: 200,
                reason: "OK".into(),
                headers: vec![(
                    "Content-Type".into(),
                    "application/json; charset=utf-8".into(),
                )],
                body: Box::new(Body {
                    bytes: self.body.as_bytes().to_vec(),
                    offset: 0,
                }),
            })
        })
    }
}

struct Store {
    id: String,
    credential: Credential,
}
impl CredentialStorage for Store {
    fn get<'a>(
        &'a self,
        name: &'a str,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Option<Credential>, AuthError>> {
        Box::pin(async move { Ok((name == self.id).then(|| self.credential.clone())) })
    }
    fn set<'a>(
        &'a self,
        _: &'a str,
        _: &'a Credential,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<(), AuthError>> {
        Box::pin(async {
            Err(OperationError::Failed(AuthError::InvalidOperation(
                "The fixed usage credential store cannot be changed.",
            )))
        })
    }
}
struct NoOAuthClient;
impl OAuthClient for NoOAuthClient {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
    fn authorization_url(&self, _: &str, _: &str, _: &str) -> String {
        String::new()
    }
    fn browser_login<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>> {
        Box::pin(async {
            Err(OperationError::Failed(AuthError::InvalidOperation(
                "The usage probe does not open a browser.",
            )))
        })
    }
    fn start_device_authorization<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<DeviceAuthorization, AuthError>> {
        Box::pin(async {
            Err(OperationError::Failed(AuthError::InvalidOperation(
                "The usage probe does not open a browser.",
            )))
        })
    }
    fn await_device_authorization<'a>(
        &'a self,
        _: &'a DeviceAuthorization,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>> {
        Box::pin(async {
            Err(OperationError::Failed(AuthError::InvalidOperation(
                "The usage probe does not open a browser.",
            )))
        })
    }
    fn refresh<'a>(
        &'a self,
        _: &'a OAuthCredential,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>> {
        Box::pin(async {
            Err(OperationError::Failed(AuthError::InvalidOperation(
                "The usage probe does not open a browser.",
            )))
        })
    }
}
struct FixedKey(String);
impl ApiKeySource for FixedKey {
    fn api_key<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<String, Error>> {
        Box::pin(async move { Ok(self.0.clone()) })
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async move { Ok(!self.0.is_empty()) })
    }
}
impl OAuthTokenSource for FixedKey {
    fn token<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<OAuthAccess, Error>> {
        Box::pin(async move {
            Ok(OAuthAccess {
                access_token: self.0.clone(),
                account_id: "usage-account".into(),
            })
        })
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async move { Ok(!self.0.is_empty()) })
    }
}

struct NoSocket;
impl Connector for NoSocket {
    fn connect<'a>(
        &'a self,
        _: &'a str,
        _: &'a Headers,
        _: Option<Duration>,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ConnectedSocket, WebSocketError>> {
        Box::pin(async {
            Err(WebSocketError::Transport(
                "The usage probe does not open a WebSocket.".into(),
            ))
        })
    }
}
struct NoImages;
impl ImageTransport for NoImages {
    fn send<'a>(
        &'a self,
        _: ImageHttpRequest,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, Error>> {
        Box::pin(async {
            Err(Error::InvalidOperation(
                "The usage probe does not generate images.".into(),
            ))
        })
    }
}

fn provider_config() -> ProviderConfig {
    ProviderConfig {
        provider_type: String::new(),
        protocol: String::new(),
        base_url: BASE_URL.into(),
        api_key_env: String::new(),
        headers: BTreeMap::new(),
        allow_insecure_localhost: false,
        allow_insecure_remote: false,
        allow_invalid_tls_certificate: false,
        disable_websocket: true,
        image_urls: false,
        stream_idle_timeout_ms: 300_000,
        header_timeout_ms: 60_000,
        header_timeout_max_retries: 5,
        provider_preferences: String::new(),
        session_header: String::new(),
        models_dev_id: String::new(),
        model_defaults: BTreeMap::new(),
        models: BTreeMap::from([(DECLARED_MODEL.into(), ModelConfig::default())]),
    }
}

pub async fn run(case: &Value) -> Result<Value, String> {
    run_observation(case, false).await
}

pub async fn run_composed(case: &Value) -> Result<Value, String> {
    run_observation(case, true).await
}

fn composed_provider(
    id: &str,
    credential: &str,
    transport: Arc<UsageTransport>,
) -> Result<Arc<dyn Provider>, Error> {
    let models = vec![Model::new(DECLARED_MODEL, id)];
    if id == "chatgpt" {
        let mut options = ChatGptOptions::new(Arc::new(FixedKey(credential.into())));
        options.models = models;
        return Ok(Arc::new(ChatGptProvider::new_with_images(
            options,
            transport,
            Arc::new(NoImages),
            Arc::new(NoSocket),
        )?));
    }
    let mut options = CompatibleOptions::new(id, BASE_URL, Arc::new(FixedKey(credential.into())));
    options.models = models;
    let reporter: Option<Arc<dyn UsageReporter>> = match id {
        "kimi" => Some(Arc::new(KimiUsageReporter::new(
            &options,
            transport.clone(),
        )?)),
        "opencode-go" => Some(Arc::new(OpenCodeGoUsageReporter::new(
            &options,
            transport.clone(),
        )?)),
        "compatible" => None,
        _ => {
            return Err(Error::InvalidOperation(
                "Unsupported original usage provider".into(),
            ));
        }
    };
    let provider: Arc<dyn Provider> = Arc::new(OpenAICompatibleProvider::new_with_images(
        options,
        transport,
        Arc::new(NoImages),
        Arc::new(NoSocket),
    )?);
    Ok(match reporter {
        Some(reporter) => Arc::new(UsageReportingProvider::new(provider, reporter)),
        None => provider,
    })
}

async fn run_observation(case: &Value, composed: bool) -> Result<Value, String> {
    let id = common::text(case, "provider_id");
    let credential = common::text(case, "credential");
    let transport = Arc::new(UsageTransport {
        body: common::text(case, "body_raw").into(),
        requests: Mutex::new(Vec::new()),
    });
    let stored = if id == "chatgpt" {
        Credential::for_oauth(OAuthCredential::create(
            credential,
            "synthetic-refresh-token",
            "9999-12-31T23:59:59.9999999+00:00",
            "usage-account",
        ))
    } else {
        Credential::for_api_key(credential)
    };
    let credentials = FoundationCredentials::new(
        Arc::new(Store {
            id: id.into(),
            credential: stored,
        }),
        Arc::new(|_| None),
        Arc::new(NoOAuthClient),
    );
    let provider: Arc<dyn Provider> = if composed {
        match composed_provider(id, credential, transport.clone()) {
            Ok(provider) => provider,
            Err(error) => return Ok(construction_error(id, &transport, &error)),
        }
    } else if id == "kimi" || common::text(case, "mode") == "direct_reporter" {
        let mut options =
            CompatibleOptions::new(id, BASE_URL, Arc::new(FixedKey(credential.into())));
        options.models = vec![Model::new(DECLARED_MODEL, id)];
        let reporter: Arc<dyn UsageReporter> =
            match KimiUsageReporter::new(&options, transport.clone()) {
                Ok(value) => Arc::new(value),
                Err(value) => return Ok(construction_error(id, &transport, &value)),
            };
        let compatible = match OpenAICompatibleProvider::new_with_images(
            options,
            transport.clone(),
            Arc::new(NoImages),
            Arc::new(NoSocket),
        ) {
            Ok(value) => Arc::new(value),
            Err(value) => return Ok(construction_error(id, &transport, &value)),
        };
        Arc::new(UsageReportingProvider::new(compatible, reporter))
    } else {
        let config = provider_config();
        match build_provider(ProviderBuildContext {
            id,
            config: &config,
            credentials: &credentials,
            transport: transport.clone(),
            images: Arc::new(NoImages),
            connector: Arc::new(NoSocket),
            external_models: &[],
            maximum_request_bytes: 64 << 20,
        }) {
            Ok(value) => value.provider,
            Err(value) => return Ok(construction_error(id, &transport, &value)),
        }
    };
    let retrying = RetryingProvider::new(provider.clone(), 5);
    let reporter = provider.usage_reporter();
    let forwarded = retrying.usage_reporter();
    let same = match (&reporter, &forwarded) {
        (Some(left), Some(right)) => Arc::ptr_eq(left, right),
        (None, None) => true,
        _ => false,
    };
    let composed_fields = composed.then(|| {
        let provider_seeds = provider.seed_models();
        let retry_seeds = retrying.seed_models();
        json!({
            "provider_id": provider.id(),
            "retry_provider_id": retrying.id(),
            "provider_seed_model_ids": provider_seeds.iter().map(|m| &m.id).collect::<Vec<_>>(),
            "seed_model_ids": retry_seeds.iter().map(|m| &m.id).collect::<Vec<_>>(),
            "seed_sequence_equal": retry_seeds == provider_seeds,
            "provider_reporter_present": reporter.is_some(),
            "reporter_present": forwarded.is_some(),
        })
    });
    let construction_request_count = transport.records().len();
    let (usage, error) = match forwarded {
        Some(reporter) => match reporter.usage(&common::ACTIVE).await {
            Ok(value) => (project_usage(&value)?, Value::Null),
            Err(value) => (Value::Null, common::error(&value)),
        },
        None => (Value::Null, Value::Null),
    };
    credentials
        .settle()
        .await
        .map_err(|error| error.to_string())?;
    let mut result = json!({
        "provider_id":retrying.id(),
        "seed_model_ids":retrying.seed_models().iter().map(|model| model.id.clone()).collect::<Vec<_>>(),
        "reporter_present":reporter.is_some(), "reporter_same_through_retry":same,
        "construction_request_count":construction_request_count,
        "requests":transport.records(), "usage":usage, "error":error,
    });
    if let Some(fields) = composed_fields {
        result
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
    }
    Ok(result)
}

fn construction_error(id: &str, transport: &UsageTransport, error: &Error) -> Value {
    json!({
        "provider_id":id, "seed_model_ids":[],
        "reporter_present":false, "reporter_same_through_retry":false,
        "construction_request_count":transport.records().len(),
        "requests":transport.records(), "usage":null, "error":common::error(error),
    })
}

fn project_usage(value: &SubscriptionUsage) -> Result<Value, String> {
    Ok(json!({
        "plan_type":value.plan_type,
        "primary_window":value.primary_window.as_ref().map(project_window).transpose()?,
        "secondary_window":value.secondary_window.as_ref().map(project_window).transpose()?,
        "credits":value.credits.as_ref().map(|credits| json!({"has_credits":credits.has_credits,"balance":credits.balance})),
    }))
}
fn project_window(value: &UsageWindow) -> Result<Value, String> {
    let instant = DateTime::parse_from_rfc3339(&value.reset_at)
        .map_err(|error| {
            format!(
                "production UsageWindow reset_at {:?}: {error}",
                value.reset_at
            )
        })?
        .with_timezone(&Utc);
    let reset_datetime = format!(
        "{}.{:07}+00:00",
        instant.format("%Y-%m-%dT%H:%M:%S"),
        instant.nanosecond() / 100
    );
    let percent = if value.used_percent.is_nan() {
        json!("NaN")
    } else if value.used_percent == f64::INFINITY {
        json!("Infinity")
    } else if value.used_percent == f64::NEG_INFINITY {
        json!("-Infinity")
    } else {
        json!(value.used_percent)
    };
    Ok(
        json!({"used_percent":percent,"reset_unix_seconds":instant.timestamp(),
        "reset_datetime":reset_datetime,"limit_window_seconds":value.limit_window_seconds}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn usage_projection_runs_selected_reporter_and_records_actual_request() {
        let result = run(&json!({"provider_id":"kimi-api","credential":"synthetic-key","mode":"factory","body_raw":"{\"data\":{\"available_balance\":12.50}}"})).await.unwrap();
        assert_eq!(result["construction_request_count"], 0);
        assert_eq!(result["reporter_present"], true);
        assert_eq!(result["reporter_same_through_retry"], true);
        assert_eq!(result["requests"].as_array().unwrap().len(), 1);
        assert_eq!(result["requests"][0]["method"], "GET");
        assert_eq!(
            result["requests"][0]["uri"],
            "https://example.test/v1/users/me/balance"
        );
        assert_eq!(
            result["requests"][0]["headers"]["authorization"],
            "Bearer synthetic-key"
        );
        assert_eq!(result["requests"][0]["body_raw"], "");
        assert_eq!(result["usage"]["credits"]["balance"], "12.50");
    }
    #[tokio::test]
    async fn usage_projection_preserves_absent_capability_and_raw_escaped_balance() {
        let result = run(&json!({"provider_id":"compatible","credential":"synthetic-key","mode":"factory","body_raw":"{}"})).await.unwrap();
        assert_eq!(result["reporter_present"], false);
        assert_eq!(result["requests"], json!([]));
        let result = run(&json!({"provider_id":"kimi","credential":"synthetic-key","mode":"direct_reporter","body_raw":"{\"data\":{\"available_balance\":\"\\u0031\"}}"})).await.unwrap();
        assert_eq!(result["usage"]["credits"]["balance"], "\\u0031");
    }
    #[test]
    fn quota_projection_preserves_ticks_and_nonfinite_values() {
        let result = project_window(&UsageWindow {
            used_percent: f64::NAN,
            reset_at: "2025-01-01T00:00:00.1234568+00:00".into(),
            limit_window_seconds: 0,
        })
        .unwrap();
        assert_eq!(result["used_percent"], "NaN");
        assert_eq!(
            result["reset_datetime"],
            "2025-01-01T00:00:00.1234568+00:00"
        );
        assert_eq!(result["reset_unix_seconds"], 1735689600);
    }
}
