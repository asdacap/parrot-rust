//! OpenRouter alpha.decisions client implementing the command safety contract.

mod wire;

use crate::http_stream::{
    HttpRequest, HttpTransport, MAX_ERROR_BYTES, bearer_headers, endpoint_url,
};
use crate::routing::auth::CredentialSources;
use crate::routing::builder::HttpClients;
use crate::sessions::{Error, provider::ApiKeySource};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_domain::contracts::safety::{
    CommandSafetyClassifier, SafetyClassifierError, SafetyVerdict,
};
use parrot_foundation::config::models::{ModelSettings, split_decision_model};
use std::sync::Arc;
use wire::{
    DecisionsAnswer, DecisionsQuestion, DecisionsRequest, DecisionsResponse, SAFETY_QUESTION_NAME,
};

/// One typed boolean question whose `true` criterion describes the safe
/// outcome and whose `false` criterion describes the unsafe outcome.
#[derive(Clone, Debug, PartialEq)]
pub struct SafetyQuestion {
    pub instructions: String,
    pub safe_criteria: String,
    pub unsafe_criteria: String,
}

/// Connection settings for one decisions provider endpoint.
#[derive(Clone, Debug, PartialEq)]
pub struct DecisionsEndpoint {
    pub id: String,
    pub base_url: String,
    pub allow_insecure_localhost: bool,
    pub allow_insecure_remote: bool,
}

/// Resolves the configured command safety classifier from model settings.
/// The classifier is absent when disabled, when the model's provider is not
/// configured, or when the provider is not a decisions provider.
pub struct DecisionClassifierSource {
    credentials: Arc<dyn CredentialSources>,
    clients: Arc<dyn HttpClients>,
}

impl DecisionClassifierSource {
    pub fn new(credentials: Arc<dyn CredentialSources>, clients: Arc<dyn HttpClients>) -> Self {
        Self {
            credentials,
            clients,
        }
    }

    pub fn resolve(
        &self,
        settings: &ModelSettings,
    ) -> Result<Option<Arc<dyn CommandSafetyClassifier>>, Error> {
        self.resolve_for_model(settings, &settings.decision_model)
    }

    /// Resolves one provider-qualified model id against the configured
    /// providers. The question template comes from the safety classifier
    /// settings; the prefix selects the serving decisions provider.
    pub fn resolve_for_model(
        &self,
        settings: &ModelSettings,
        model: &str,
    ) -> Result<Option<Arc<dyn CommandSafetyClassifier>>, Error> {
        let Some((provider_id, model_id)) = split_decision_model(model) else {
            return Ok(None);
        };
        let Some(config) = settings.providers.get(provider_id) else {
            return Ok(None);
        };
        if config.provider_type != "openrouter-decisions" {
            return Ok(None);
        }
        let transport = self
            .clients
            .resolve(&config.base_url, config.allow_invalid_tls_certificate)?
            .streaming;
        let question = SafetyQuestion {
            instructions: settings.safety_classifier.question_instructions.clone(),
            safe_criteria: settings.safety_classifier.safe_criteria.clone(),
            unsafe_criteria: settings.safety_classifier.unsafe_criteria.clone(),
        };
        let client = OpenRouterDecisionsClient::new(
            DecisionsEndpoint {
                id: provider_id.to_owned(),
                base_url: config.base_url.clone(),
                allow_insecure_localhost: config.allow_insecure_localhost,
                allow_insecure_remote: config.allow_insecure_remote,
            },
            model_id,
            question,
            self.credentials.api_key(provider_id, &config.api_key_env),
            transport,
        )
        .map_err(|error| Error::Provider(error.to_string()))?;
        Ok(Some(Arc::new(client)))
    }
}

/// Sends one typed boolean question about a shell command to the decisions
/// endpoint and maps the typed answer to a safety verdict. Transport, auth,
/// HTTP and interpretability failures surface as classifier errors so the
/// caller can fail safe.
pub struct OpenRouterDecisionsClient {
    endpoint: DecisionsEndpoint,
    model: String,
    question: SafetyQuestion,
    api_key: Arc<dyn ApiKeySource>,
    transport: Arc<dyn HttpTransport>,
}

impl OpenRouterDecisionsClient {
    pub fn new(
        endpoint: DecisionsEndpoint,
        model: impl Into<String>,
        question: SafetyQuestion,
        api_key: Arc<dyn ApiKeySource>,
        transport: Arc<dyn HttpTransport>,
    ) -> Result<Self, SafetyClassifierError> {
        endpoint_url(
            &endpoint.base_url,
            wire::DECISIONS_ENDPOINT,
            endpoint.allow_insecure_localhost,
            endpoint.allow_insecure_remote,
        )
        .map_err(transport_error)?;
        Ok(Self {
            endpoint,
            model: model.into(),
            question,
            api_key,
            transport,
        })
    }

    fn endpoint_url(&self) -> Result<crate::http_stream::Endpoint, SafetyClassifierError> {
        endpoint_url(
            &self.endpoint.base_url,
            wire::DECISIONS_ENDPOINT,
            self.endpoint.allow_insecure_localhost,
            self.endpoint.allow_insecure_remote,
        )
        .map_err(transport_error)
    }

    async fn headers(
        &self,
        cancellation: &dyn Cancellation,
    ) -> Result<Vec<(String, String)>, SafetyClassifierError> {
        let key = self.api_key.api_key(cancellation).await.map_err(|error| {
            SafetyClassifierError::Transport(format!("classifier credential failure: {error}"))
        })?;
        let mut headers = bearer_headers(&self.endpoint.id, &key).map_err(transport_error)?;
        headers.push(("Accept".into(), "application/json".into()));
        headers.push((
            "User-Agent".into(),
            format!("parrot/{}", env!("CARGO_PKG_VERSION")),
        ));
        Ok(headers)
    }
}

fn transport_error(error: impl std::fmt::Display) -> SafetyClassifierError {
    SafetyClassifierError::Transport(error.to_string())
}

async fn read_body(
    response: &mut crate::http_stream::HttpResponse,
    cancellation: &dyn Cancellation,
) -> Result<String, SafetyClassifierError> {
    let mut bytes = Vec::new();
    let mut buffer = vec![0_u8; MAX_ERROR_BYTES];
    loop {
        let count = response
            .body
            .read(&mut buffer, cancellation)
            .await
            .map_err(transport_error)?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..count]);
        if bytes.len() > MAX_ERROR_BYTES {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

impl CommandSafetyClassifier for OpenRouterDecisionsClient {
    fn classify<'a>(
        &'a self,
        command: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<SafetyVerdict, SafetyClassifierError>> {
        Box::pin(async move {
            let question = DecisionsQuestion::boolean(
                &self.question.instructions,
                &self.question.safe_criteria,
                &self.question.unsafe_criteria,
            );
            let request = DecisionsRequest {
                model: self.model.clone(),
                questions: [(SAFETY_QUESTION_NAME.to_owned(), question)].into(),
                state: serde_json::json!({"command": command}),
            };
            let endpoint = self.endpoint_url()?;
            let headers = self.headers(cancellation).await?;
            let mut response = self
                .transport
                .send(
                    HttpRequest {
                        method: "POST".into(),
                        endpoint,
                        headers,
                        content_type: Some("application/json".into()),
                        filter_content_headers: true,
                        body: serde_json::to_vec(&request)
                            .map_err(|error| SafetyClassifierError::Transport(error.to_string()))?,
                    },
                    cancellation,
                )
                .await
                .map_err(transport_error)?;
            if !(200..300).contains(&response.status) {
                let body = read_body(&mut response, cancellation)
                    .await
                    .unwrap_or_default();
                return Err(SafetyClassifierError::Api {
                    status: response.status,
                    message: if body.is_empty() {
                        format!("HTTP {}", response.status)
                    } else {
                        body
                    },
                });
            }
            let text = read_body(&mut response, cancellation).await?;
            let decoded: DecisionsResponse = serde_json::from_str(&text).map_err(|error| {
                SafetyClassifierError::InvalidResponse(format!(
                    "classifier reply was not a decisions response: {error}"
                ))
            })?;
            let answer = decoded.answers.get(SAFETY_QUESTION_NAME).ok_or_else(|| {
                SafetyClassifierError::InvalidResponse(
                    "classifier reply carries no answer for the safety question".into(),
                )
            })?;
            match answer.answer_type.as_str() {
                DecisionsAnswer::BOOLEAN_TYPE => {
                    let Some(noul) = answer.noul else {
                        return Err(SafetyClassifierError::InvalidResponse(
                            "the boolean answer carries no probability".into(),
                        ));
                    };
                    if noul >= 0.5 {
                        Ok(SafetyVerdict::Safe)
                    } else {
                        Ok(SafetyVerdict::Unsafe {
                            reason: self.question.unsafe_criteria.clone(),
                        })
                    }
                }
                DecisionsAnswer::REFUSAL_TYPE => Err(SafetyClassifierError::InvalidResponse(
                    "the classifier refused to answer the safety question".into(),
                )),
                _ => Err(SafetyClassifierError::InvalidResponse(
                    "classifier answered with an unsupported question type".into(),
                )),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http_stream::{ByteStream, HttpError, HttpResponse};
    use parrot_domain::contracts::async_ops::BoxFuture;
    use std::sync::Mutex;

    struct StaticBody(Vec<u8>);
    impl ByteStream for StaticBody {
        fn read<'a>(
            &'a mut self,
            buffer: &'a mut [u8],
            _: &'a dyn Cancellation,
        ) -> BoxFuture<'a, Result<usize, HttpError>> {
            Box::pin(async move {
                if self.0.is_empty() {
                    return Ok(0);
                }
                let count = self.0.len().min(buffer.len());
                buffer[..count].copy_from_slice(&self.0[..count]);
                self.0.drain(..count);
                Ok(count)
            })
        }
    }

    struct StaticKey;
    impl ApiKeySource for StaticKey {
        fn api_key<'a>(
            &'a self,
            _: &'a dyn Cancellation,
        ) -> BoxFuture<'a, Result<String, crate::sessions::Error>> {
            Box::pin(async { Ok("key-1".into()) })
        }
        fn has_credential<'a>(
            &'a self,
            _: &'a dyn Cancellation,
        ) -> BoxFuture<'a, Result<bool, crate::sessions::Error>> {
            Box::pin(async { Ok(true) })
        }
    }

    /// Serves one canned response, recording each sent request.
    struct ScriptedTransport {
        status: u16,
        body: Mutex<Option<Vec<u8>>>,
        fail: bool,
        requests: Mutex<Vec<HttpRequest>>,
    }
    impl ScriptedTransport {
        fn ok(body: &str) -> Arc<Self> {
            Arc::new(Self {
                status: 200,
                body: Mutex::new(Some(body.as_bytes().to_vec())),
                fail: false,
                requests: Mutex::new(Vec::new()),
            })
        }
        fn status(status: u16, body: &str) -> Arc<Self> {
            Arc::new(Self {
                status,
                body: Mutex::new(Some(body.as_bytes().to_vec())),
                fail: false,
                requests: Mutex::new(Vec::new()),
            })
        }
        fn failing() -> Arc<Self> {
            Arc::new(Self {
                status: 200,
                body: Mutex::new(None),
                fail: true,
                requests: Mutex::new(Vec::new()),
            })
        }
    }
    impl HttpTransport for ScriptedTransport {
        fn send<'a>(
            &'a self,
            request: HttpRequest,
            _: &'a dyn Cancellation,
        ) -> BoxFuture<'a, Result<HttpResponse, HttpError>> {
            Box::pin(async move {
                self.requests.lock().unwrap().push(request);
                if self.fail {
                    return Err(HttpError::Provider("socket dropped".into()));
                }
                Ok(HttpResponse {
                    status: self.status,
                    reason: "OK".into(),
                    headers: Vec::new(),
                    body: Box::new(StaticBody(
                        self.body.lock().unwrap().take().unwrap_or_default(),
                    )),
                })
            })
        }
    }

    fn client(transport: Arc<dyn HttpTransport>) -> OpenRouterDecisionsClient {
        OpenRouterDecisionsClient::new(
            DecisionsEndpoint {
                id: "openrouter-decisions".into(),
                base_url: "https://openrouter.ai/api/v1".into(),
                allow_insecure_localhost: false,
                allow_insecure_remote: false,
            },
            "openrouter/perplexity/pplx-decider-v1.2",
            SafetyQuestion {
                instructions: "Is the command safe?".into(),
                safe_criteria: "reads only".into(),
                unsafe_criteria: "destroys data".into(),
            },
            Arc::new(StaticKey),
            transport,
        )
        .unwrap()
    }

    fn answer_body(value: &str) -> String {
        format!(
            r#"{{"id":"req_1","model":"openrouter/perplexity/pplx-decider-v1.2","answers":{{"is_command_safe":{value}}}}}"#
        )
    }

    #[tokio::test]
    async fn high_boolean_probability_classifies_safe() {
        let transport = ScriptedTransport::ok(&answer_body(r#"{"type":"noul","noul":0.97}"#));
        let verdict = client(transport.clone())
            .classify("ls", &parrot_core::NeverCancelled)
            .await
            .unwrap();
        assert_eq!(verdict, SafetyVerdict::Safe);
        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].endpoint.as_str().ends_with("/alpha/decisions"));
        assert!(
            String::from_utf8_lossy(&requests[0].body).contains("pplx-decider-v1.2"),
            "the request carries the configured model"
        );
    }

    #[tokio::test]
    async fn low_boolean_probability_classifies_unsafe_with_reason() {
        let transport = ScriptedTransport::ok(&answer_body(r#"{"type":"noul","noul":0.2}"#));
        let verdict = client(transport)
            .classify("rm -rf /", &parrot_core::NeverCancelled)
            .await
            .unwrap();
        assert_eq!(
            verdict,
            SafetyVerdict::Unsafe {
                reason: "destroys data".into()
            }
        );
    }

    #[tokio::test]
    async fn refusal_is_an_error_not_a_verdict() {
        let transport = ScriptedTransport::ok(&answer_body(r#"{"type":"refusal"}"#));
        let error = client(transport)
            .classify("ls", &parrot_core::NeverCancelled)
            .await
            .unwrap_err();
        assert!(matches!(error, SafetyClassifierError::InvalidResponse(_)));
    }

    #[tokio::test]
    async fn http_error_status_surfaces_as_an_api_error() {
        let transport = ScriptedTransport::status(402, r#"{"error":{"message":"no credit"}}"#);
        let error = client(transport)
            .classify("ls", &parrot_core::NeverCancelled)
            .await
            .unwrap_err();
        match error {
            SafetyClassifierError::Api { status, message } => {
                assert_eq!(status, 402);
                assert!(message.contains("no credit"));
            }
            other => panic!("expected an api error, got {other}"),
        }
    }

    #[tokio::test]
    async fn transport_failure_surfaces_as_a_transport_error() {
        let transport = ScriptedTransport::failing();
        let error = client(transport)
            .classify("ls", &parrot_core::NeverCancelled)
            .await
            .unwrap_err();
        assert!(matches!(error, SafetyClassifierError::Transport(_)));
    }

    #[tokio::test]
    async fn uninterpretable_body_surfaces_as_an_invalid_response() {
        let transport = ScriptedTransport::ok("<html>not json</html>");
        let error = client(transport)
            .classify("ls", &parrot_core::NeverCancelled)
            .await
            .unwrap_err();
        assert!(matches!(error, SafetyClassifierError::InvalidResponse(_)));
    }

    #[tokio::test]
    async fn missing_answer_surfaces_as_an_invalid_response() {
        let transport = ScriptedTransport::ok(r#"{"id":"req_1","model":"m","answers":{}}"#);
        let error = client(transport)
            .classify("ls", &parrot_core::NeverCancelled)
            .await
            .unwrap_err();
        assert!(matches!(error, SafetyClassifierError::InvalidResponse(_)));
    }
}
