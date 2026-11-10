use super::*;

impl<S, T> parrot_core::Provider for RuntimeProvider<'_, S, T>
where
    S: AccessSnapshot + 'static,
    T: TurnTools<S> + 'static,
    T::Snapshot: 'static,
{
    fn estimate_input_tokens(
        &self,
        instructions: &str,
        definitions: &[ToolDefinition],
        messages: &[parrot_core::Message],
    ) -> Result<i64, CoreError> {
        let selection = self.binding.capture_selection().map_err(CoreError::from)?;
        crate::compaction::CompactionEstimates::new(&self.backend.compactor, &selection.model)
            .estimate_context_tokens(instructions, definitions, messages)
            .map_err(Into::into)
    }
    fn estimate_tokens(&self, messages: &[parrot_core::Message]) -> Result<i64, CoreError> {
        let selection = self.binding.capture_selection().map_err(CoreError::from)?;
        crate::compaction::CompactionEstimates::new(&self.backend.compactor, &selection.model)
            .estimate_tokens(messages)
            .map_err(Into::into)
    }
    fn estimate_summary_tokens(
        &self,
        preceding_summary: &str,
        chunk_tokens: i64,
        next_group: &[parrot_core::Message],
    ) -> Result<i64, CoreError> {
        let selection = self.binding.capture_selection().map_err(CoreError::from)?;
        crate::compaction::CompactionEstimates::new(&self.backend.compactor, &selection.model)
            .estimate_summary_tokens(preceding_summary, chunk_tokens, next_group)
            .map_err(Into::into)
    }
    fn summarize<'a, 'c>(
        &'a self,
        request: ProviderRequest,
        cancellation: parrot_core::CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<parrot_core::compaction::SummaryResponse, CoreError>>
    where
        'c: 'a,
    {
        Box::pin(async move {
            let selection = self.binding.capture_selection().map_err(CoreError::from)?;
            let provider = crate::compaction::SummaryProviderAdapter::new(
                selection.model.canonical_model,
                &self.backend.sessions,
                ContextEmitter {
                    backend: self.backend,
                },
            );
            provider
                .summarize_native(request, cancellation)
                .await
                .map_err(Into::into)
        })
    }

    fn invoke<'a, 'c>(
        &'a self,
        request: ProviderRequest,
        cancellation: parrot_core::CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<ProviderResponse, CoreError>>
    where
        'c: 'a,
    {
        Box::pin(async move {
            let backend = self.backend;
            let selection = self.binding.capture_selection().map_err(CoreError::from)?;
            let selected = &selection.model.canonical_model;
            let mut state = backend.state.lock().await;
            let request: sessions::Request = request.into();
            let estimate = crate::compaction::CompactionPlanner::estimate_input_tokens(
                selected,
                &request.instructions,
                &request.tools,
                &request.messages,
            )
            .map_err(|error| CoreError::from(TurnError::Planning(error)))?;
            state.request_accounting = Some(RequestAccounting {
                selected: selected.clone(),
                estimated_input_tokens: estimate,
            });
            let session = backend
                .sessions
                .get(selected.provider.clone())
                .await
                .map_err(|error| CoreError::from(TurnError::Provider(error)))?;
            let output = OutputBridge(backend.output.as_ref());
            let activity = ActivityBridge(backend.activity.as_ref());
            let mut call = ProviderCall {
                history: &backend.history,
                publisher: backend.publisher.as_ref(),
                event_id: backend.event_id.as_ref(),
                output: &output,
                activity: &activity,
                usage: &mut *state,
            };
            let event = match call.stream(session.as_ref(), request, &cancellation).await {
                Ok(event) => event,
                Err(
                    error @ provider_call::ProviderCallError::Provider(
                        sessions::Error::Http(
                            parrot_provider::http_stream::HttpError::RequestTooLarge(_),
                        )
                        | sessions::Error::Socket(
                            parrot_provider::responses_websocket::WebSocketError::RequestTooLarge {
                                ..
                            },
                        ),
                    ),
                ) => {
                    let mut error = CoreError::from(TurnError::ProviderCall(error));
                    error.kind = ErrorKind::RequestTooLarge;
                    return Err(error);
                }
                Err(error) => return Err(CoreError::from(TurnError::ProviderCall(error))),
            };
            match event.base_event() {
                sessions::Event::Completed(completed) => Ok(completed.clone().into()),
                _ => unreachable!("provider call returns completion"),
            }
        })
    }
}

