//! Prompt image linking over the caller-owned T30 model and owning pull streams.
use super::bucket::ImageBucket;
use crate::{
    catalogs::Model,
    sessions::{
        Content, Error, Event, EventStream, Provider, Request, Session,
        decorators::{UsageReporter, classify_failure},
        diagnostics::CallTrace,
        retry::{Failure, is_context_length_exceeded, is_engine_overloaded, is_usage_limit},
    },
};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::watch;

type Upload = watch::Receiver<Option<Result<String, Error>>>;
struct State {
    bucket: Arc<dyn ImageBucket>,
    links: Mutex<HashMap<String, Upload>>,
}
/// Uploads are shared by exact path across direct calls and all sessions.
pub struct ImageLinkingProvider {
    inner: Arc<dyn Provider>,
    state: Arc<State>,
}
impl ImageLinkingProvider {
    pub fn new(inner: Arc<dyn Provider>, bucket: Arc<dyn ImageBucket>) -> Self {
        Self {
            inner,
            state: Arc::new(State {
                bucket,
                links: Mutex::new(HashMap::new()),
            }),
        }
    }
    fn stream(
        &self,
        request: Request,
        target: Target,
        scope: Arc<AtomicBool>,
    ) -> Box<dyn EventStream> {
        Box::new(LinkStream {
            target,
            state: self.state.clone(),
            scope,
            original: Some(request),
            initialized: false,
            verification: None,
            linked: None,
            rewritten: false,
            current: None,
            repaired: false,
            visible: false,
            done: false,
        })
    }
}
impl Provider for ImageLinkingProvider {
    fn id(&self) -> &str {
        self.inner.id()
    }
    fn usage_reporter(&self) -> Option<Arc<dyn UsageReporter>> {
        self.inner.usage_reporter()
    }
    fn generate_image<'a>(
        &'a self,
        request: super::ImageGenerationRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<super::ImageGenerationResult, Error>> {
        self.inner.generate_image(request, cancellation)
    }
    fn call(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        Ok(self.stream(
            request,
            Target::Provider(self.inner.clone()),
            Arc::new(AtomicBool::new(false)),
        ))
    }
    fn has_credential<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<bool, Error>> {
        self.inner.has_credential(cancellation)
    }
    fn seed_models(&self) -> Vec<Model> {
        self.inner.seed_models()
    }
    fn list_models<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<Model>, Error>> {
        self.inner.list_models(cancellation)
    }
    fn calculate_image_tokens(&self, model: &Model, content: &Content) -> i64 {
        self.inner.calculate_image_tokens(model, content)
    }
    fn open_session(&self) -> Result<Arc<dyn Session>, Error> {
        Ok(Arc::new(LinkSession {
            inner: self.inner.open_session()?,
            state: self.state.clone(),
            scope: Arc::new(AtomicBool::new(false)),
        }))
    }
}
struct LinkSession {
    inner: Arc<dyn Session>,
    state: Arc<State>,
    scope: Arc<AtomicBool>,
}
impl LinkSession {
    fn stream(&self, request: Request, trace: Option<Arc<CallTrace>>) -> Box<dyn EventStream> {
        Box::new(LinkStream {
            target: Target::Session(self.inner.clone(), trace),
            state: self.state.clone(),
            scope: self.scope.clone(),
            original: Some(request),
            initialized: false,
            verification: None,
            linked: None,
            rewritten: false,
            current: None,
            repaired: false,
            visible: false,
            done: false,
        })
    }
}
impl Session for LinkSession {
    fn begin_turn(&self) -> BoxFuture<'_, Result<(), Error>> {
        self.inner.begin_turn()
    }
    fn call(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        Ok(self.stream(request, None))
    }
    fn call_with_trace(
        &self,
        request: Request,
        trace: Arc<CallTrace>,
    ) -> Result<Box<dyn EventStream>, Error> {
        Ok(self.stream(request, Some(trace)))
    }
    fn try_fall_back_to_http(&self) -> BoxFuture<'_, Result<bool, Error>> {
        self.inner.try_fall_back_to_http()
    }
    fn close(&self) -> BoxFuture<'_, Result<(), Error>> {
        self.inner.close()
    }
}
struct Never;
impl Cancellation for Never {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}
impl State {
    fn upload(&self, path: &str, media_type: &str) -> Upload {
        let mut links = self.links.lock().unwrap();
        if let Some(upload) = links.get(path) {
            return upload.clone();
        }
        let (sender, receiver) = watch::channel(None);
        links.insert(path.to_owned(), receiver.clone());
        let bucket = self.bucket.clone();
        let path = path.to_owned();
        let media_type = media_type.to_owned();
        let mut work: BoxFuture<'static, Result<String, Error>> = Box::pin(async move {
            bucket.prepare(&Never).await?;
            let bytes = tokio::task::spawn_blocking(move || std::fs::read(path))
                .await
                .map_err(|e| Error::Io(e.to_string()))?
                .map_err(|e| Error::Io(e.to_string()))?;
            bucket.put_image(bytes, media_type, &Never).await
        });
        match work
            .as_mut()
            .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
        {
            std::task::Poll::Ready(result) => {
                sender.send_replace(Some(result));
            }
            std::task::Poll::Pending => {
                tokio::spawn(async move {
                    sender.send_replace(Some(work.await));
                });
            }
        }
        receiver
    }
    async fn wait_upload(
        &self,
        path: &str,
        mut upload: Upload,
        cancellation: &dyn Cancellation,
    ) -> Result<String, Error> {
        let result = loop {
            if let Some(result) = upload.borrow().clone() {
                break result;
            }
            if cancellation.is_cancelled() {
                return Err(Error::Cancelled);
            }
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(Error::Cancelled),
                changed = upload.changed() => if changed.is_err() { break Err(Error::InvalidOperation("Image upload task stopped without a result.".into())); },
            }
        };
        if result.is_err() && !cancellation.is_cancelled() {
            self.links.lock().unwrap().remove(path);
        }
        result
    }
    async fn rewrite(
        &self,
        request: &Request,
        inline: bool,
        cancellation: &dyn Cancellation,
    ) -> Result<Option<Request>, Error> {
        if inline
            || !request
                .messages
                .iter()
                .any(|message| message.contents().iter().any(needs_link))
        {
            return Ok(None);
        }
        let mut linked = request.clone();
        for message in &mut linked.messages {
            let mut uploads = HashMap::new();
            for content in message.contents() {
                if needs_link(content) {
                    uploads
                        .entry(content.image_path().to_owned())
                        .or_insert_with(|| self.upload(content.image_path(), content.media_type()));
                }
            }
            let mut urls: HashMap<String, String> = HashMap::new();
            let mut contents = message.contents().to_vec();
            for content in &mut contents {
                if !needs_link(content) {
                    continue;
                }
                let path = content.image_path().to_owned();
                let url = if let Some(url) = urls.get(&path) {
                    url.clone()
                } else {
                    let url = self
                        .wait_upload(&path, uploads[&path].clone(), cancellation)
                        .await?;
                    urls.insert(path.clone(), url.clone());
                    url
                };
                *content = Content::image_link(
                    path,
                    url,
                    content.media_type(),
                    content.image_width(),
                    content.image_height(),
                );
            }
            message.set_contents(contents);
        }
        Ok(Some(linked))
    }
    async fn reupload_missing(
        &self,
        linked: &Request,
        cancellation: &dyn Cancellation,
    ) -> Result<bool, Error> {
        let mut seen = HashSet::new();
        let mut missing = Vec::new();
        for content in linked
            .messages
            .iter()
            .flat_map(|message| message.contents())
        {
            if !content.image_url().is_empty()
                && seen.insert(content.image_path())
                && !self
                    .bucket
                    .verify_public(content.image_url(), cancellation)
                    .await?
            {
                missing.push(content.image_path().to_owned());
            }
        }
        if missing.is_empty() {
            return Ok(false);
        }
        self.bucket.forget_preparation();
        let mut links = self.links.lock().unwrap();
        for path in missing {
            links.remove(&path);
        }
        Ok(true)
    }
}
fn needs_link(content: &Content) -> bool {
    matches!(content, Content::Image { url, .. } if url.is_empty())
}
fn rejection(error: &Error) -> bool {
    matches!(
        classify_failure(error),
        Failure::Http { .. } | Failure::Response { .. }
    )
}
fn url_rejection(error: &Error) -> bool {
    let failure = classify_failure(error);
    !is_usage_limit(&failure)
        && !is_context_length_exceeded(&failure)
        && !is_engine_overloaded(&failure)
        && matches!(failure, Failure::Response { .. } | Failure::Http { status: 400..=499, .. } if !matches!(&failure, Failure::Http { status: 401 | 403 | 408 | 429, .. }))
}
enum Target {
    Provider(Arc<dyn Provider>),
    Session(Arc<dyn Session>, Option<Arc<CallTrace>>),
}
impl Target {
    fn call(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        match self {
            Self::Provider(provider) => provider.call(request),
            Self::Session(session, Some(trace)) => session.call_with_trace(request, trace.clone()),
            Self::Session(session, None) => session.call(request),
        }
    }
}
struct LinkStream {
    target: Target,
    state: Arc<State>,
    scope: Arc<AtomicBool>,
    original: Option<Request>,
    initialized: bool,
    verification: Option<Request>,
    linked: Option<Request>,
    rewritten: bool,
    current: Option<Box<dyn EventStream>>,
    repaired: bool,
    visible: bool,
    done: bool,
}
impl LinkStream {
    async fn rewrite(&mut self, cancellation: &dyn Cancellation) -> Result<(), Error> {
        let rewritten = self
            .state
            .rewrite(
                self.original.as_ref().expect("retained original request"),
                self.scope.load(Ordering::Acquire),
                cancellation,
            )
            .await?;
        self.rewritten = rewritten.is_some();
        self.linked = rewritten.or_else(|| self.original.take());
        Ok(())
    }
    async fn advance(&mut self, cancellation: &dyn Cancellation) -> Result<Option<Event>, Error> {
        if self.done {
            return Ok(None);
        }
        if !self.initialized {
            self.rewrite(cancellation).await?;
            self.initialized = true;
        }
        if self.current.is_none() {
            let request = self.linked.take().expect("prepared linking request");
            self.verification = self.rewritten.then(|| request.clone());
            self.current = Some(self.target.call(request)?);
        }
        let result = self.current.as_mut().unwrap().next(cancellation).await;
        match result {
            Ok(Some(event)) => {
                self.visible |= event.is_visible_output();
                Ok(Some(event))
            }
            other => {
                self.current.as_mut().unwrap().close().await?;
                self.current = None;
                let failure = match other {
                    Ok(None) => {
                        self.done = true;
                        return Ok(None);
                    }
                    Err(error) => error,
                    Ok(Some(_)) => unreachable!(),
                };
                if self.repaired
                    || self.visible
                    || !self.rewritten
                    || cancellation.is_cancelled()
                    || !rejection(&failure)
                {
                    return Err(failure);
                }
                self.repaired = true;
                let reason = if self
                    .state
                    .reupload_missing(self.verification.as_ref().unwrap(), cancellation)
                    .await?
                {
                    self.rewrite(cancellation).await?;
                    "Re-uploaded missing image objects. Retrying."
                } else if url_rejection(&failure) {
                    self.scope.store(true, Ordering::Release);
                    self.linked = self.original.take();
                    self.rewritten = false;
                    "Provider rejected image URLs. Retrying with inline image data."
                } else {
                    return Err(failure);
                };
                self.visible = false;
                Ok(Some(Event::Retry {
                    attempt: 1,
                    retry_after: Duration::ZERO,
                    reason: reason.into(),
                }))
            }
        }
    }
}
impl EventStream for LinkStream {
    fn next<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<Event>, Error>> {
        Box::pin(async move {
            let result = self.advance(cancellation).await;
            if result.is_err() {
                self.done = true;
            }
            result
        })
    }
    fn close(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            self.done = true;
            if let Some(mut stream) = self.current.take() {
                stream.close().await?;
            }
            Ok(())
        })
    }
}
