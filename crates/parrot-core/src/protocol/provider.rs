use super::compaction::SummaryResponse;
use super::{
    BoxFuture, CancellationToken, Error, Message, ProviderRequest, ProviderResponse, ToolCatalog,
    ToolDefinition, ToolResultMetadata,
};

/// Invokes models using the core protocol while owning transport and stream cleanup.
/// Unsupported invocation and estimation capabilities return explicit errors.
pub trait Provider: Send + Sync {
    /// Estimates exactly the supplied request using model-specific token costs.
    fn estimate_input_tokens(
        &self,
        _instructions: &str,
        _definitions: &[ToolDefinition],
        _messages: &[Message],
    ) -> Result<i64, Error> {
        Err(Error::unsupported("Provider.estimate_input_tokens"))
    }
    /// Collects a private summary stream without publishing its generated content.
    fn summarize<'a, 'c>(
        &'a self,
        _request: ProviderRequest,
        _cancellation: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<SummaryResponse, Error>>
    where
        'c: 'a,
    {
        Box::pin(async { Err(Error::unsupported("Provider.summarize")) })
    }

    /// Invokes the request and joins stream cleanup before returning, including
    /// after errors or cancellation.
    fn invoke<'a, 'c>(
        &'a self,
        _request: ProviderRequest,
        _cancellation: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<ProviderResponse, Error>>
    where
        'c: 'a,
    {
        Box::pin(async { Err(Error::unsupported("Provider.invoke")) })
    }
}

/// Retains executable tools and executes explicitly supplied calls.
pub trait Tools<M: ToolResultMetadata>: Send + Sync {
    /// Captures permitted definitions backed by the original executable identities
    /// for this turn. The final catalog is the subset permitted in the final phase.
    fn capture_catalog(&self) -> BoxFuture<'_, Result<ToolCatalog, Error>>;
    /// Executes one ordered prefix of the supplied calls, joining all admitted work.
    /// Core settles the returned chunk before requesting the next prefix.
    fn execute<'a>(
        &'a self,
        request: crate::ToolExecutionRequest,
        cancellation: CancellationToken<'a>,
    ) -> BoxFuture<'a, Result<Vec<crate::ToolResult<M>>, Error>>;
}
