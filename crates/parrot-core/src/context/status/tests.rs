use super::*;
use crate::{CompactionFailure, ConfigKey, ConfigValue, ErrorCode};
use std::{
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll, Wake, Waker},
    thread,
};

struct ThreadWake(thread::Thread);
impl Wake for ThreadWake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}
fn finish<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(ThreadWake(thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(output) => return output,
            Poll::Pending => thread::park(),
        }
    }
}
fn message(text: &str) -> Message {
    Message {
        role: Role::User,
        contents: vec![Content::Text(text.into())],
        ..Message::default()
    }
}
fn configuration(input_limit: i32) -> ContextConfiguration {
    ContextConfiguration {
        canonical_model: "provider/model".into(),
        context_window: input_limit,
        maximum_input_tokens: input_limit,
        maximum_output_tokens: 1,
        trigger_percent: 90,
        context_limit: None,
    }
}
struct Status {
    text: Option<String>,
    contexts: Mutex<Vec<ContextSnapshot>>,
}

impl Status {
    fn new(text: &str) -> Self {
        Self {
            text: Some(text.into()),
            contexts: Mutex::default(),
        }
    }
}

impl States for Status {
    fn get_config(&self, _: ConfigKey) -> crate::BoxFuture<'_, Result<ConfigValue, Error>> {
        Box::pin(async { Err(Error::failed("status configuration is unavailable")) })
    }

    fn read_status_offering<'a>(
        &'a self,
        context: ContextSnapshot,
    ) -> crate::BoxFuture<'a, Result<crate::StatusOffering, Error>> {
        Box::pin(async move {
            let content = self
                .text
                .clone()
                .unwrap_or_else(|| context.estimated_tokens.to_string());
            self.contexts.lock().unwrap().push(context);
            let mut offering = crate::StatusOffering::default();
            offering.insert(
                crate::StatusKey::new("status"),
                crate::StatusOffer {
                    hash: String::new(),
                    text: content,
                    is_auto_update: false,
                },
            );
            Ok(offering)
        })
    }
}

enum StatusEstimate {
    Content,
    Increase,
    Failure(Error),
}

struct Estimates {
    contexts: Mutex<Vec<ProviderRequest>>,
    calls: AtomicUsize,
    status: StatusEstimate,
}

impl Estimates {
    fn new(status: StatusEstimate) -> Self {
        Self {
            contexts: Mutex::default(),
            calls: AtomicUsize::new(0),
            status,
        }
    }
}

impl crate::Provider for Estimates {
    fn estimate_input_tokens(
        &self,
        instructions: &str,
        definitions: &[crate::ToolDefinition],
        messages: &[Message],
    ) -> Result<i64, Error> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        match &self.status {
            StatusEstimate::Content => {
                self.contexts.lock().unwrap().push(ProviderRequest {
                    instructions: instructions.into(),
                    tools: definitions.to_vec(),
                    messages: messages.to_vec(),
                    ..ProviderRequest::default()
                });
                Ok(messages
                    .iter()
                    .flat_map(|message| &message.contents)
                    .map(|content| match content {
                        Content::Text(text) => text.len() as i64,
                        Content::Image { .. } => 10,
                    })
                    .sum())
            }
            StatusEstimate::Increase => Ok(call as i64),
            StatusEstimate::Failure(error) => Err(error.clone()),
        }
    }
}

#[test]
fn core_status_fitting_estimates_the_full_request_without_mutating_it() {
    let provider = Estimates::new(StatusEstimate::Content);
    let status = Status {
        text: None,
        contexts: Mutex::default(),
    };
    let request = ProviderRequest {
        instructions: "sampled instructions".into(),
        messages: vec![message("history"), message("pending input")],
        tools: vec![crate::ToolDefinition {
            name: "test tool".into(),
            description: "test definition".into(),
            parameters_json: "{}".into(),
        }],
        ..ProviderRequest::default()
    };
    let original = request.clone();
    let configuration = configuration(100);
    let fitted = finish(fit_status(&request, &configuration, &status, &provider)).unwrap();
    assert_eq!(fitted, "22");
    assert_eq!(request, original);
    let contexts = provider.contexts.lock().unwrap();
    assert!(contexts.len() > 1);
    for estimate in contexts.iter() {
        assert_eq!(estimate.instructions, request.instructions);
        assert_eq!(estimate.tools, request.tools);
        assert_eq!(&estimate.messages[..2], request.messages);
        assert_eq!(estimate.messages.len(), 3);
        assert_eq!(estimate.messages[2].role, Role::System);
    }
    assert_eq!(
        contexts.last().unwrap().messages[2].contents,
        [Content::Text(fitted)]
    );
    assert_eq!(
        status.contexts.lock().unwrap().last(),
        Some(&ContextSnapshot::estimate(22, &configuration))
    );
}

#[test]
fn core_status_fitting_caps_rerenders() {
    let provider = Estimates::new(StatusEstimate::Increase);
    let status = Status {
        text: None,
        contexts: Mutex::default(),
    };
    let fitted = finish(fit_status(
        &ProviderRequest::default(),
        &configuration(100),
        &status,
        &provider,
    ))
    .unwrap();
    assert_eq!(provider.calls.load(Ordering::SeqCst), 9);
    assert_eq!(status.contexts.lock().unwrap().len(), 9);
    assert_eq!(fitted, "9");
}

#[test]
fn core_status_fitting_propagates_estimate_failure_before_rendering() {
    let error = Error {
        code: ErrorCode::Timeout,
        response_body: "native estimate diagnostic".into(),
        detail: Some(crate::ErrorDetail::Compaction(
            CompactionFailure::NoSummary("native detail".into()),
        )),
        ..Error::failed("native estimate failed")
    };
    let provider = Estimates::new(StatusEstimate::Failure(error.clone()));
    let status = Status::new("must not render");
    let outcome = finish(fit_status(
        &ProviderRequest::default(),
        &configuration(100),
        &status,
        &provider,
    ));
    assert_eq!(outcome, Err(error));
    assert!(status.contexts.lock().unwrap().is_empty());
}
