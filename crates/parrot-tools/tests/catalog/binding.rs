use std::{
    collections::BTreeMap,
    io,
    net::{IpAddr, SocketAddr},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use super::support::{Artifacts, NoCancel, Selection, Services, Snapshot};
use parrot_domain::contracts::{async_ops::BoxFuture, security::InvocationContext};
use parrot_foundation::{config::Configuration, template_catalog::PromptTemplateCatalog};
use parrot_tools::{
    catalog::{self, Catalog, Invocation},
    files::WorkspacePaths,
    process::ownership::owner::ShellProcessOwner,
    web_fetch::{
        self, FetchError, GzipDecoder, Options, Request, Resolver, Response, ResponseBody,
        Transport, TransportSession, WebFetcher,
    },
};

#[derive(Default)]
pub struct WebState {
    pub sessions: AtomicUsize,
    pub requests: Mutex<Vec<String>>,
}
struct NetworkFixture(Arc<WebState>);
impl Resolver for NetworkFixture {
    fn resolve<'a>(&'a self, host: &'a str) -> BoxFuture<'a, io::Result<Vec<IpAddr>>> {
        Box::pin(async move {
            assert_eq!(host, "catalog.invalid");
            Ok(vec!["8.8.8.8".parse().unwrap()])
        })
    }
}
impl Transport for NetworkFixture {
    fn session(&self) -> Result<Box<dyn TransportSession + '_>, FetchError> {
        self.0.sessions.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(NetworkFixture(self.0.clone())))
    }
}
impl TransportSession for NetworkFixture {
    fn send<'a>(
        &'a mut self,
        request: &'a Request,
        addresses: &'a [SocketAddr],
        _: &'a Options,
    ) -> BoxFuture<'a, Result<Response, FetchError>> {
        Box::pin(async move {
            assert_eq!(addresses, &["8.8.8.8:443".parse::<SocketAddr>().unwrap()]);
            self.0
                .requests
                .lock()
                .unwrap()
                .push(request.method.as_str().into());
            Ok(Response {
                status_code: 200,
                location: None,
                content_type: Some("text/plain; charset=utf-8".into()),
                content_encodings: vec![],
                body: Box::new(BytesBody(b"catalog web".to_vec())),
            })
        })
    }
}
struct BytesBody(Vec<u8>);
impl ResponseBody for BytesBody {
    fn read<'a>(&'a mut self, output: &'a mut [u8]) -> BoxFuture<'a, Result<usize, FetchError>> {
        Box::pin(async move {
            let count = output.len().min(self.0.len());
            output[..count].copy_from_slice(&self.0[..count]);
            self.0.drain(..count);
            Ok(count)
        })
    }
}
struct NoGzip;
impl GzipDecoder for NoGzip {
    fn decode(&self, _: Box<dyn ResponseBody>) -> Result<Box<dyn ResponseBody>, FetchError> {
        panic!("plain catalog fixture is not a production gzip decoder")
    }
}

pub struct Fixture {
    pub catalog: Catalog<Snapshot, Selection>,
    pub configuration: Configuration,
    pub templates: Arc<PromptTemplateCatalog>,
    pub services: Services,
    pub artifacts: Artifacts,
    pub owner: Arc<ShellProcessOwner>,
    pub web: Arc<WebState>,
}
impl Fixture {
    pub fn new(overrides: &str, allow_parent: bool, depth: i32) -> Self {
        let artifacts = Artifacts::new();
        let env = [
            "HOME",
            "XDG_CACHE_HOME",
            "XDG_DATA_HOME",
            "XDG_CONFIG_HOME",
            "XDG_STATE_HOME",
            "WORKDIR",
        ]
        .into_iter()
        .map(|name| {
            (
                name.to_owned(),
                artifacts.path.join(name).to_string_lossy().into_owned(),
            )
        })
        .collect::<BTreeMap<_, _>>();
        let config_path = artifacts.path.join("config.yaml");
        std::fs::write(&config_path, overrides).unwrap();
        let (configuration, templates) = PromptTemplateCatalog::load(
            &config_path,
            &artifacts.path.join("predefined.yaml"),
            &env,
        )
        .unwrap();
        let templates = Arc::new(templates);
        let definitions = catalog::configured_definitions(&configuration);
        let services = Services::default();
        let shared = Arc::new(services.clone());
        let process_services = services.process_services(templates.clone());
        let owner = process_services.owner.clone();
        let process = catalog::ProcessFactories::new(process_services, &definitions).unwrap();
        let agents = catalog::AgentFactories::new(
            catalog::AgentServices {
                runtime: shared.clone(),
                models: shared.clone(),
                questions: shared.clone(),
                answers: shared.clone(),
                templates: templates.clone(),
                allow_parent,
            },
            &definitions,
        )
        .unwrap();
        let files = catalog::FileFactories::new(
            services.clone(),
            WorkspacePaths::default(),
            artifacts.repository.clone(),
            configuration.runtime.request_limits.image_bytes_per_read,
            &definitions,
        )
        .unwrap();
        let session = catalog::SessionFactories::new(
            catalog::SessionBindings {
                control: shared.clone(),
                images: shared.clone(),
                permission: shared.clone(),
                filesystem: shared.clone(),
                templates: templates.clone(),
                agent_depth: depth,
            },
            &definitions,
        )
        .unwrap();
        let tasks = catalog::TaskQueueFactories::new(
            catalog::TaskQueueServices {
                tasks: shared.clone(),
                filesystem: shared.clone(),
                templates: templates.clone(),
                queues: shared.clone(),
                clock: shared.clone(),
                diagnostics: shared,
            },
            &definitions,
        )
        .unwrap();
        let web = Arc::new(WebState::default());
        let network = Arc::new(NetworkFixture(web.clone()));
        let fetcher = Arc::new(WebFetcher::new(
            Arc::new(web_fetch::PublicAddressPolicy),
            network.clone(),
            network,
            Arc::new(NoGzip),
            Options::default(),
        ));
        let web_factory = catalog::WebFetchFactory::new(fetcher, &definitions).unwrap();
        let catalog = Catalog::new(process, agents, files, session, tasks, web_factory);
        Self {
            catalog,
            configuration,
            templates,
            services,
            artifacts,
            owner,
            web,
        }
    }
    pub async fn invoke(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<catalog::Output, parrot_domain::contracts::async_ops::OperationError<catalog::Error>>
    {
        let mut invocation = Invocation::new("catalog-call", arguments.to_string());
        invocation.assistant_sequence = 9_007_199_254_740_993;
        invocation.prompt_templates = Some(self.templates.clone());
        let snapshot = self.catalog.snapshot().unwrap();
        let tool = snapshot.find(name).unwrap().create();
        tool.invoke(
            invocation,
            InvocationContext::new(
                Snapshot { id: 71 },
                catalog::Selection {
                    requested_model: "captured-alias".into(),
                    scoped: Selection { id: 89 },
                },
                &NoCancel,
            ),
        )
        .await
    }
    pub async fn close(self) {
        self.owner.dispose().await.unwrap();
        drop(self.catalog);
        self.artifacts.close().await;
    }
}
