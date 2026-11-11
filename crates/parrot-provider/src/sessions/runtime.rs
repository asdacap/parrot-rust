use super::{Event, Request};
pub use crate::provider_error::Error;
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use std::sync::Arc;

/// Owning, pull-driven stream: opening a call does not start its transport.
pub trait EventStream: Send {
    fn next<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<Event>, Error>>;
    fn close(&mut self) -> BoxFuture<'_, Result<(), Error>>;
}

pub trait Session: Send + Sync {
    fn begin_turn(&self) -> BoxFuture<'_, Result<(), Error>>;
    fn call(&self, request: Request) -> Result<Box<dyn EventStream>, Error>;
    fn call_with_trace(
        &self,
        request: Request,
        _trace: Arc<super::diagnostics::CallTrace>,
    ) -> Result<Box<dyn EventStream>, Error> {
        self.call(request)
    }
    fn try_fall_back_to_http(&self) -> BoxFuture<'_, Result<bool, Error>>;
    fn close(&self) -> BoxFuture<'_, Result<(), Error>>;
}
pub trait Provider: Send + Sync {
    fn generate_image<'a>(
        &'a self,
        _request: crate::image_generation::ImageGenerationRequest,
        _cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<crate::image_generation::ImageGenerationResult, Error>> {
        Box::pin(async {
            Err(Error::Provider(
                "provider: image generation is not supported".into(),
            ))
        })
    }
    fn id(&self) -> &str;
    fn usage_reporter(&self) -> Option<Arc<dyn super::decorators::UsageReporter>> {
        None
    }
    fn has_credential<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<bool, Error>>;
    fn seed_models(&self) -> Vec<crate::catalogs::Model>;
    fn list_models<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<crate::catalogs::Model>, Error>>;
    fn calculate_image_tokens(
        &self,
        _model: &crate::catalogs::Model,
        _content: &super::Content,
    ) -> i64 {
        crate::image_inspection::FALLBACK_IMAGE_TOKENS
    }
    fn open_session(&self) -> Result<Arc<dyn Session>, Error>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        chat_completions::ProviderFailure,
        http_stream::{HttpError, HttpFailure},
        responses::ResponsesError,
        responses_websocket::WebSocketError,
    };

    fn response_error(body: &str) -> ResponsesError {
        ResponsesError::ProviderResponse {
            error_type: "type".into(),
            error_code: "code".into(),
            detail: "detail".into(),
            response_body: body.into(),
        }
    }

    #[test]
    fn response_body_borrows_the_http_failure_body() {
        let body = String::from("http body");
        let original = body.as_ptr();
        let error = Error::from(HttpError::Response(HttpFailure {
            status: 400,
            error_type: "type".into(),
            error_code: "code".into(),
            detail: "detail".into(),
            response_body: body,
        }));

        assert_eq!(error.response_body(), "http body");
        assert_eq!(error.response_body().as_ptr(), original);
    }

    #[test]
    fn response_body_reads_the_provider_stream_failure() {
        let failure = response_error("stream body");
        let ResponsesError::ProviderResponse { response_body, .. } = &failure else {
            unreachable!();
        };
        let original = response_body.as_ptr();
        let error = Error::Response(failure);

        assert_eq!(error.response_body(), "stream body");
        assert_eq!(error.response_body().as_ptr(), original);
    }

    #[test]
    fn response_body_reads_chat_and_socket_provider_failures() {
        let chat = Error::ChatProvider(ProviderFailure {
            error_type: "type".into(),
            error_code: "code".into(),
            message: "detail".into(),
            response_body: "chat body".into(),
        });
        let socket = Error::from(WebSocketError::Response(response_error("socket body")));

        assert_eq!(chat.response_body(), "chat body");
        assert_eq!(socket.response_body(), "socket body");
    }

    #[test]
    fn response_body_is_empty_for_ordinary_and_non_provider_errors() {
        for error in [
            Error::Io("network".into()),
            Error::Provider("provider message".into()),
            Error::Http(HttpError::Transport("transport message".into())),
            Error::Response(ResponsesError::WireProtocol("wire message".into())),
            Error::Socket(WebSocketError::Wire("socket message".into())),
        ] {
            assert_eq!(error.response_body(), "");
        }
    }

    #[test]
    fn response_body_does_not_traverse_exception_causes() {
        for error in [
            Error::ProviderWithCause {
                message: "outer provider".into(),
                cause: Box::new(Error::Response(response_error("hidden provider body"))),
            },
            Error::HeaderRetryLimit {
                maximum_retries: 1,
                attempts: 2,
                cause: Box::new(Error::Response(response_error("hidden retry body"))),
            },
            Error::WebSocketTransport {
                message: "outer transport".into(),
                cause: Box::new(Error::Response(response_error("hidden transport body"))),
            },
            Error::Socket(WebSocketError::TransportFailure {
                message: "outer socket".into(),
                cause: Box::new(WebSocketError::Response(response_error(
                    "hidden socket body",
                ))),
            }),
        ] {
            assert_eq!(error.response_body(), "");
        }
    }
}
