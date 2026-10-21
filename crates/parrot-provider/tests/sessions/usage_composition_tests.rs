use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_provider::{
    catalogs::Model,
    http_stream::{HttpError, HttpRequest, HttpResponse, HttpTransport},
    responses_websocket::{ConnectedSocket, Connector, Headers, WebSocketError},
    sessions::{
        Error, Provider,
        decorators::{RetryingProvider, SubscriptionUsage, UsageReporter, UsageReportingProvider},
        provider::{
            ApiKeySource, ChatGptOptions, ChatGptProvider, CompatibleOptions, OAuthAccess,
            OAuthTokenSource, OpenAICompatibleProvider,
        },
    },
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

struct Credentials(String);
impl ApiKeySource for Credentials {
    fn api_key<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<String, Error>> {
        Box::pin(async { Ok(self.0.clone()) })
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(!self.0.is_empty()) })
    }
}
impl OAuthTokenSource for Credentials {
    fn token<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<OAuthAccess, Error>> {
        Box::pin(async {
            Ok(OAuthAccess {
                access_token: self.0.clone(),
                account_id: "usage-account".into(),
            })
        })
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(!self.0.is_empty()) })
    }
}
#[derive(Default)]
struct NoRequests(AtomicUsize);
impl HttpTransport for NoRequests {
    fn send<'a>(
        &'a self,
        _: HttpRequest,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpError>> {
        Box::pin(async {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(HttpError::Transport(
                "construction must not request HTTP".into(),
            ))
        })
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
        Box::pin(async { panic!("construction must not connect") })
    }
}
// Only the T30 construction/delegation slices are exercised here. T32 owns
// selection and request/decoding of the Kimi/OpenCode usage capability.
struct SelectedUsageBoundary;
impl UsageReporter for SelectedUsageBoundary {
    fn usage<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<SubscriptionUsage, Error>> {
        Box::pin(async {
            panic!("T32 usage request/decoding is outside this construction assertion")
        })
    }
}

#[test]
fn composed_usage_preserves_identity_declared_seeds_and_no_construction_requests_for_all_eight_source_tuples()
 {
    for id in ["chatgpt", "kimi", "opencode-go", "compatible"] {
        for has_credential in [true, false] {
            let credentials = Arc::new(Credentials(if has_credential {
                "usage-token".into()
            } else {
                String::new()
            }));
            let transport = Arc::new(NoRequests::default());
            let declared = Model::new("declared-model", id);
            let provider: Arc<dyn Provider> = if id == "chatgpt" {
                let mut options = ChatGptOptions::new(credentials);
                options.models = vec![declared.clone()];
                Arc::new(
                    ChatGptProvider::new(options, transport.clone(), Arc::new(NoSocket)).unwrap(),
                )
            } else {
                let mut options =
                    CompatibleOptions::new(id, "https://example.test/v1", credentials);
                options.models = vec![declared.clone()];
                let compatible: Arc<dyn Provider> = Arc::new(
                    OpenAICompatibleProvider::new(options, transport.clone(), Arc::new(NoSocket))
                        .unwrap(),
                );
                if id == "compatible" {
                    compatible
                } else {
                    Arc::new(UsageReportingProvider::new(
                        compatible,
                        Arc::new(SelectedUsageBoundary),
                    ))
                }
            };
            let retrying = RetryingProvider::new(provider.clone(), 5);
            assert_eq!(retrying.id(), provider.id());
            let seeds = provider.seed_models();
            assert_eq!(seeds.len(), 1);
            assert_eq!(seeds[0].id, "declared-model");
            assert_eq!(retrying.seed_models(), seeds);
            assert_eq!(transport.0.load(Ordering::SeqCst), 0);
            if id == "compatible" {
                assert!(provider.usage_reporter().is_none());
                assert!(retrying.usage_reporter().is_none());
            } else {
                assert!(Arc::ptr_eq(
                    &provider.usage_reporter().unwrap(),
                    &retrying.usage_reporter().unwrap()
                ));
            }
        }
    }
}
