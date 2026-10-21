use super::{FetchError, Request, WebFetcher};
use parrot_domain::contracts::async_ops::{BoxFuture, OperationError};
use parrot_domain::contracts::security::{AccessSnapshot, InvocationContext};
use parrot_domain::contracts::tools::{ParallelSafety, Tool};
use parrot_domain::values::tools::ConfiguredToolDefinition;
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use std::marker::PhantomData;
use std::sync::Arc;

pub struct Invocation {
    pub arguments_json: String,
    pub prompt_templates: Option<Arc<PromptTemplateCatalog>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Output {
    pub text: String,
}

pub struct WebFetchTool<Security, Selection> {
    fetcher: Arc<WebFetcher>,
    definition: ConfiguredToolDefinition,
    context: PhantomData<fn() -> (Security, Selection)>,
}

impl<Security, Selection> WebFetchTool<Security, Selection> {
    pub const NAME: &'static str = "web_fetch";

    pub fn new(fetcher: Arc<WebFetcher>, definition: ConfiguredToolDefinition) -> Self {
        Self {
            fetcher,
            definition,
            context: PhantomData,
        }
    }
}

impl<Security: AccessSnapshot + 'static, Selection: Send + 'static> Tool
    for WebFetchTool<Security, Selection>
{
    type Definition = ConfiguredToolDefinition;
    type Invocation = Invocation;
    type Selection = Selection;
    type Security = Security;
    type Output = Output;
    type Error = FetchError;

    fn definition(&self) -> &Self::Definition {
        &self.definition
    }

    fn parallel_safety(&self, _invocation: &Self::Invocation) -> ParallelSafety {
        ParallelSafety::ParallelSafe
    }

    fn enabled_after_interruption(&self) -> bool {
        false
    }

    fn invoke<'a>(
        &'a self,
        invocation: Invocation,
        context: InvocationContext<'a, Security, Selection>,
    ) -> BoxFuture<'a, Result<Output, OperationError<FetchError>>> {
        Box::pin(async move {
            let result = match Request::parse(&invocation.arguments_json) {
                Ok(request) => self
                    .fetcher
                    .fetch(request, context.cancellation())
                    .await
                    .map(|result| Output { text: result.text }),
                Err(error) => Err(OperationError::Failed(error)),
            };
            match result {
                Err(OperationError::Failed(error)) if error.is_tool_error() => {
                    let message = error.to_string();
                    let text = match invocation.prompt_templates {
                        Some(templates) => templates
                            .render(
                                "tool-result.error",
                                &[("message".to_owned(), serde_json::Value::String(message))],
                                context.cancellation(),
                            )
                            .map_err(|error| match error {
                                OperationError::Cancelled => OperationError::Cancelled,
                                OperationError::Failed(error) => {
                                    OperationError::Failed(FetchError::Template(error.to_string()))
                                }
                            })?,
                        None => format!("error: {message}"),
                    };
                    Ok(Output { text })
                }
                result => result,
            }
        })
    }
}
