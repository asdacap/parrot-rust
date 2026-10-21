use super::common::{Token, array, boolean, error, model, seed_model, selection, text};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_foundation::oauth::AuthError;
use parrot_provider::{
    catalogs::Model,
    http_stream::{HttpError, HttpFailure},
    routing::registry::{ProviderModel, ProviderRegistry},
    sessions::{Content, Error, Event, EventStream, Provider, Request, Session},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    future::poll_fn,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::Poll,
};
use tokio::sync::watch;

pub struct ScriptProvider {
    id: String,
    seeds: Vec<Model>,
    live: Vec<Model>,
    has_credential: bool,
    credential_failure: Option<Error>,
    list_failure: Option<Error>,
    honor_cancellation: bool,
    gate: Option<watch::Receiver<bool>>,
    credential_checks: AtomicUsize,
    model_lists: AtomicUsize,
}
impl Provider for ScriptProvider {
    fn id(&self) -> &str {
        &self.id
    }
    fn seed_models(&self) -> Vec<Model> {
        self.seeds.clone()
    }
    fn has_credential<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async move {
            self.credential_checks.fetch_add(1, Ordering::SeqCst);
            if let Some(mut gate) = self.gate.clone() {
                while !*gate.borrow() {
                    gate.changed()
                        .await
                        .map_err(|_| Error::InvalidOperation("fixture gate closed".into()))?;
                }
            }
            if self.honor_cancellation && cancellation.is_cancelled() {
                return Err(Error::Cancelled);
            }
            self.credential_failure
                .clone()
                .map_or(Ok(self.has_credential), Err)
        })
    }
    fn list_models<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<Model>, Error>> {
        Box::pin(async move {
            self.model_lists.fetch_add(1, Ordering::SeqCst);
            if self.honor_cancellation && cancellation.is_cancelled() {
                return Err(Error::Cancelled);
            }
            self.list_failure
                .clone()
                .map_or_else(|| Ok(self.live.clone()), Err)
        })
    }
    fn call(&self, _: Request) -> Result<Box<dyn EventStream>, Error> {
        Ok(Box::new(Empty))
    }
    fn open_session(&self) -> Result<Arc<dyn Session>, Error> {
        Ok(Arc::new(Empty))
    }
    fn calculate_image_tokens(&self, _: &Model, _: &Content) -> i64 {
        4096
    }
}
struct Empty;
impl EventStream for Empty {
    fn next<'a>(
        &'a mut self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<Event>, Error>> {
        Box::pin(async { Ok(None) })
    }
    fn close(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
}
impl Session for Empty {
    fn begin_turn(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
    fn call(&self, _: Request) -> Result<Box<dyn EventStream>, Error> {
        Ok(Box::new(Empty))
    }
    fn try_fall_back_to_http(&self) -> BoxFuture<'_, Result<bool, Error>> {
        Box::pin(async { Ok(false) })
    }
    fn close(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
}
pub fn injected_failure(name: &str) -> Option<Error> {
    Some(match name {
        "" => return None,
        "provider" => Error::Provider("synthetic provider failure".into()),
        "http" => Error::Http(HttpError::Response(HttpFailure {
            status: 503,
            error_type: String::new(),
            error_code: String::new(),
            detail: "synthetic HTTP failure".into(),
            response_body: String::new(),
        })),
        "header" => Error::HeaderTimeout("synthetic header failure".into()),
        "wire" => Error::Wire("synthetic wire failure".into()),
        "auth" => Error::Authentication(AuthError::Message("synthetic auth failure")),
        "io" => Error::Io("synthetic IO failure".into()),
        "transport" => Error::Http(HttpError::Transport("synthetic transport failure".into())),
        "cancelled" => Error::Cancellation("synthetic cancellation".into()),
        "unexpected" => Error::InvalidOperation("synthetic unexpected failure".into()),
        other => {
            return Some(Error::InvalidOperation(format!(
                "unknown fixture failure {other}"
            )));
        }
    })
}
pub fn make_registry(
    input: &Value,
    gate: Option<watch::Receiver<bool>>,
) -> Result<(Arc<ProviderRegistry>, Vec<Arc<ScriptProvider>>), Error> {
    let mut catalogues = BTreeMap::new();
    let mut scripts = Vec::new();
    for p in array(input, "providers") {
        let id = text(p, "id").to_owned();
        let seeds = array(p, "models")
            .iter()
            .map(|m| seed_model(&id, m))
            .collect::<Vec<_>>();
        let live = array(p, "live_models")
            .iter()
            .map(|m| seed_model(&id, m))
            .collect();
        catalogues.insert(id.clone(), seeds.clone());
        scripts.push(Arc::new(ScriptProvider {
            id,
            seeds,
            live,
            has_credential: boolean(p, "has_credential", true),
            credential_failure: injected_failure(text(p, "credential_failure")),
            list_failure: injected_failure(text(p, "list_failure")),
            honor_cancellation: boolean(p, "honor_cancellation", false),
            gate: gate.clone(),
            credential_checks: AtomicUsize::new(0),
            model_lists: AtomicUsize::new(0),
        }));
    }
    let providers = scripts
        .iter()
        .map(|p| p.clone() as Arc<dyn Provider>)
        .collect();
    Ok((
        Arc::new(ProviderRegistry::new(providers, catalogues)?),
        scripts,
    ))
}
fn catalogues(registry: &ProviderRegistry) -> Value {
    Value::Object(
        registry
            .list()
            .iter()
            .map(|p| {
                (
                    p.id().into(),
                    Value::Array(registry.models(p.id()).iter().map(model).collect()),
                )
            })
            .collect(),
    )
}
pub fn resolution(requested: &str, result: Result<ProviderModel, Error>) -> Value {
    match result {
        Ok(value) => json!({"requested":requested,"result":selection(&value),"error":null}),
        Err(value) => json!({"requested":requested,"result":null,"error":error(&value)}),
    }
}
pub async fn run(input: &Value) -> Result<Value, String> {
    let concurrent = boolean(input, "concurrent", false);
    let (sender, gate) = watch::channel(false);
    let (registry, scripts) = match make_registry(input, concurrent.then_some(gate)) {
        Ok(v) => v,
        Err(e) => return Ok(json!({"error":error(&e)})),
    };
    let initial = catalogues(&registry);
    let mut refresh_result = Value::Null;
    let mut refresh_error = Value::Null;
    let mut starts = Value::Null;
    if boolean(input, "refresh", false) {
        let token = Token(boolean(input, "cancelled", false));
        let result = if concurrent {
            let mut future = registry.available_models(&token);
            let ready = poll_fn(|cx| {
                Poll::Ready(match future.as_mut().poll(cx) {
                    Poll::Ready(result) => Some(result),
                    Poll::Pending => None,
                })
            })
            .await;
            starts = Value::Bool(
                ready.is_none()
                    && scripts
                        .iter()
                        .all(|p| p.credential_checks.load(Ordering::SeqCst) > 0),
            );
            sender.send_replace(true);
            match ready {
                Some(result) => result,
                None => future.await,
            }
        } else {
            registry.available_models(&token).await
        };
        match result {
            Ok(models) => refresh_result = Value::Array(models.iter().map(selection).collect()),
            Err(value) => refresh_error = error(&value),
        }
    }
    let mut ordered = scripts.clone();
    ordered.sort_by(|a, b| a.id.encode_utf16().cmp(b.id.encode_utf16()));
    let resolutions = array(input, "selectors")
        .iter()
        .map(|s| {
            let s = s.as_str().unwrap_or_default();
            resolution(s, registry.resolve_canonical(s))
        })
        .collect::<Vec<_>>();
    Ok(
        json!({"ordered_ids":registry.list().iter().map(|p|p.id().to_owned()).collect::<Vec<_>>(),"initial_catalogues":initial,"refresh_result":refresh_result,"refresh_error":refresh_error,"concurrent_credential_starts":starts,"final_catalogues":catalogues(&registry),"resolutions":resolutions,"default_resolution":resolution("",registry.resolve_default_canonical()),"calls":ordered.iter().map(|p|json!({"id":p.id,"credential_checks":p.credential_checks.load(Ordering::SeqCst),"model_lists":p.model_lists.load(Ordering::SeqCst)})).collect::<Vec<_>>() }),
    )
}

pub fn no_refresh_registry(input: &Value) -> Result<Arc<ProviderRegistry>, Error> {
    make_registry(input, None).map(|v| v.0)
}
