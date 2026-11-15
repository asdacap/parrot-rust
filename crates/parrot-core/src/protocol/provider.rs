use super::{
    BoxFuture, CancellationToken, Error, Message, ProviderRequest, ProviderResponse, ToolCatalog,
    ToolDefinition,
};

/// An invocation failure with usage observed before the failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderInvokeError {
    pub error: Error,
    pub input_tokens: Option<i32>,
}

impl From<Error> for ProviderInvokeError {
    fn from(error: Error) -> Self {
        Self {
            error,
            input_tokens: None,
        }
    }
}

/// Invokes models using the core protocol while owning transport and stream cleanup.
/// Unsupported invocation and estimation capabilities return explicit errors.
pub trait Provider: Send + Sync {
    /// Compacts and finalizes conversation context before returning its replacement.
    /// Returns no replacement for a successful no-op. An error may follow a durable commit.
    fn compact<'a, 'c>(
        &'a self,
        _request: crate::CompactionRequest,
        _cancellation: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<Option<Message>, Error>>
    where
        'c: 'a,
    {
        Box::pin(async { Err(Error::unsupported("Provider.compact")) })
    }

    /// Estimates exactly the supplied request using model-specific token costs.
    fn estimate_input_tokens(
        &self,
        _instructions: &str,
        _definitions: &[ToolDefinition],
        _messages: &[Message],
    ) -> Result<i64, Error> {
        Err(Error::unsupported("Provider.estimate_input_tokens"))
    }
    /// Invokes the request and joins stream cleanup before returning, including
    /// after errors or cancellation. Returns the last positive input-token usage
    /// observed during the invocation on either success or failure.
    fn invoke<'a, 'c>(
        &'a self,
        _request: ProviderRequest,
        _cancellation: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<ProviderResponse, ProviderInvokeError>>
    where
        'c: 'a,
    {
        Box::pin(async { Err(Error::unsupported("Provider.invoke").into()) })
    }
}

/// Retains executable tools and executes explicitly supplied calls.
pub trait Tools: Send + Sync {
    /// Captures permitted definitions backed by the original executable identities
    /// for this step. The final catalog is the subset permitted in the final phase.
    fn capture_catalog(&self) -> BoxFuture<'_, Result<ToolCatalog, Error>>;
    /// Executes one ordered prefix of the supplied calls, joining all admitted work.
    /// Core settles the returned chunk before requesting the next prefix.
    fn execute<'a>(
        &'a self,
        request: crate::ToolExecutionRequest,
        cancellation: CancellationToken<'a>,
    ) -> BoxFuture<'a, Result<Vec<crate::ToolResult>, Error>>;
}
