use std::{
    collections::{BTreeMap, HashMap},
    io::{self, BufReader, IsTerminal, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use parrot_cli::{
    attachments::uploader::{AttachmentError, PromptAttachmentUploader},
    dispatch::{
        self, CommandBackend, CommandDispatcher, DispatchError, RuntimeCommand,
        arguments::{ChatOptions, Command, ServeOptions},
        basic_loop::{BasicChatRequest, BasicCli, NativeLineInput},
        dialog::DialogInput,
        interrupts::Interrupts,
        startup::{LocalChatStartup, OpenLocalClient},
    },
    slash_sessions::WebCommandFactory,
    slash_settings::{AuthServices, ModelSelection, model_alias},
    terminal_dialogs::{
        EnhancedChatRequest, EnhancedCli, EnhancedTerminal, LocalTerminalSessionNavigation,
        PreparePrompt, TerminalSessionNavigation,
    },
    terminal_input::RawTerminal,
};
use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation},
    protocol::messages::parrot as wire,
};
use parrot_foundation::{
    config::Configuration,
    credentials::{CredentialPresets, CredentialStore},
    oauth::{OAuthClient, OpenAiOAuthClient, OpenAiOAuthOptions, SystemBrowserOpener},
    paths::{ApplicationRoots, StatePaths},
    template_catalog::PromptTemplateCatalog,
};
use parrot_host::{
    composition::factory::user::ordinary_workspace_resolver,
    grpc::{
        address::TransportAddress,
        service::GrpcService,
        token::TransportToken,
        transport::{GrpcServer, GrpcTransportClient},
    },
    production::{ProductionHost, ProductionHostConfig, ProductionReadContext},
    web::{
        proxy::WebParrotProxy,
        router::{WebClient, WebSessionRouter},
        server::WebServer,
        slash::WebSlashService,
    },
};
use parrot_provider::routing::builder::ProviderRegistryBuilder;
use parrot_runtime::agent_turn::lifecycle::Signal;
use parrot_storage::{
    diagnostics::{DiagnosticEvent, DiagnosticSeverity, FileDiagnosticLog},
    history_files::SessionCatalog,
    ownership::{capture_runtime_identity, host_key},
};

mod https_certificate;
#[path = "dispatch/process_signals.rs"]
mod process_signals;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    let paths = StatePaths::resolve_from_environment();
    let diagnostics = match FileDiagnosticLog::open_global(
        paths.state().join("logs"),
        FileDiagnosticLog::create_instance_id(),
        |warning| eprintln!("{warning}"),
    ) {
        Ok(log) => Arc::new(log),
        Err(failure) => {
            eprintln!("parrot: {failure}");
            std::process::exit(dispatch::EXIT_FAILURE);
        }
    };
    diagnostics.write(&DiagnosticEvent::new(
        "process",
        "start",
        DiagnosticSeverity::Information,
    ));
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(failure) => {
            eprintln!("parrot: {failure}");
            std::process::exit(dispatch::EXIT_FAILURE);
        }
    };
    let outcome = run_process(&runtime, arguments, paths, diagnostics.clone());
    let exit = match outcome {
        Ok(exit) => {
            let mut event =
                DiagnosticEvent::new("process", "stop", DiagnosticSeverity::Information);
            event.outcome = Some(exit.to_string());
            diagnostics.write(&event);
            exit
        }
        Err(failure) => {
            let mut event = DiagnosticEvent::new(
                "process",
                "failure",
                if matches!(failure, DispatchError::Cancelled) {
                    DiagnosticSeverity::Information
                } else {
                    DiagnosticSeverity::Error
                },
            );
            event.outcome = Some(
                if matches!(failure, DispatchError::Cancelled) {
                    "cancelled"
                } else {
                    "failed"
                }
                .into(),
            );
            event.error_code = Some(
                match failure {
                    DispatchError::Cancelled => "cancelled",
                    DispatchError::Io(_) => "io",
                    DispatchError::Rpc(_) => "rpc",
                    DispatchError::Command(_) => "invalid_operation",
                }
                .into(),
            );
            diagnostics.write(&event);
            eprintln!("parrot: {failure}");
            dispatch::EXIT_FAILURE
        }
    };
    // Final process publication also precedes dropping the construction Runtime.
    drop(runtime);
    std::process::exit(exit);
}

fn run_process(
    runtime: &tokio::runtime::Runtime,
    arguments: Vec<String>,
    paths: StatePaths,
    diagnostics: Arc<FileDiagnosticLog>,
) -> Result<i32, DispatchError> {
    // main retains the actual construction Runtime through command completion,
    // input/render/transport owners, provider refresh joins, and final diagnostics.
    let cancellation = Arc::new(Signal::new());
    let stop = cancellation.clone();
    let interrupts = Arc::new(Interrupts::new(Arc::new(move || stop.cancel())));
    // Help/version/usage do not require process signal registration or runtime
    // command construction. Actual signal delivery is not part of the test suite.
    let _registrations = if matches!(dispatch::parse(&arguments), Ok(Command::Runtime(_))) {
        Some(process_signals::ProcessSignals::register()?)
    } else {
        None
    };
    let observe_stop = Signal::new();
    let mut backend = Application {
        paths,
        diagnostics,
        interrupts: interrupts.clone(),
    };
    let (mut output, mut error) = (io::stdout(), io::stderr());
    let mut dispatcher = CommandDispatcher {
        output: &mut output,
        error: &mut error,
        version: VERSION,
    };
    let result = runtime.block_on(async {
        let command = async {
            let result = dispatcher
                .run(&arguments, &mut backend, cancellation.as_ref())
                .await;
            observe_stop.cancel();
            result
        };
        let (result, ()) = tokio::join!(
            command,
            process_signals::observe(&interrupts, &observe_stop)
        );
        result
    });
    // The command has settled every constructible owned scope before main
    // publishes its final process event and drops the SAME actual Runtime.
    drop(backend);
    result
}

struct Application {
    paths: StatePaths,
    diagnostics: Arc<FileDiagnosticLog>,
    interrupts: Arc<Interrupts>,
}

impl CommandBackend for Application {
    fn execute<'a>(
        &'a mut self,
        command: RuntimeCommand,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<i32, DispatchError>> {
        Box::pin(async move {
            cancellation.check().map_err(|_| DispatchError::Cancelled)?;
            match command {
                RuntimeCommand::Authenticate(options) => {
                    let credentials =
                        CredentialStore::at_selected_path(self.paths.credentials_file());
                    let oauth = oauth_client()?;
                    dispatch::auth_command::run(
                        &options,
                        &mut BufReader::new(io::stdin()),
                        &mut io::stdout(),
                        &mut io::stderr(),
                        &credentials,
                        oauth.as_ref(),
                        &self.diagnostics,
                        cancellation,
                    )
                    .await
                }
                RuntimeCommand::Sessions => dispatch::session_list::run(
                    &SessionCatalog::new(self.paths.state()),
                    &mut io::stdout(),
                    cancellation,
                ),
                RuntimeCommand::Models => {
                    let configuration = self.configuration()?;
                    let mut catalog = dispatch::provider_catalog::build(
                        configuration,
                        Arc::new(CredentialStore::at_selected_path(
                            self.paths.credentials_file(),
                        )),
                        oauth_client()?,
                        cancellation,
                    )
                    .await?;
                    let result = dispatch::provider_catalog::list(
                        catalog.registry(),
                        &mut io::stdout(),
                        cancellation,
                    )
                    .await;
                    let closed = catalog.close().await;
                    combine(result, closed)
                }
                RuntimeCommand::Chat(options) => self.chat(options, cancellation).await,
                RuntimeCommand::Serve(options) => self.serve(options, cancellation).await,
                RuntimeCommand::Web { port } => self.web(port, cancellation).await,
            }
        })
    }
}

impl Application {
    fn configuration(&self) -> Result<Configuration, DispatchError> {
        self.configuration_and_templates()
            .map(|(configuration, _)| configuration)
    }

    fn configuration_and_templates(
        &self,
    ) -> Result<(Configuration, PromptTemplateCatalog), DispatchError> {
        let environment: BTreeMap<_, _> = std::env::vars().collect();
        PromptTemplateCatalog::load(
            &self.paths.config_file(),
            &self.paths.predefined_config_file(),
            &environment,
        )
        .map_err(command_failure)
    }

    fn auth_services(
        &self,
        configuration: &Configuration,
    ) -> Result<Arc<AuthServices>, DispatchError> {
        Ok(Arc::new(AuthServices {
            credentials: Arc::new(CredentialStore::at_selected_path(
                self.paths.credentials_file(),
            )),
            presets: Arc::new(CredentialPresets::at_selected_config_directory(
                self.paths.config(),
            )),
            oauth: oauth_client()?,
            provider_ids: ProviderRegistryBuilder::buildable_provider_ids(configuration),
            diagnostics: self.diagnostics.clone(),
        }))
    }

    fn local_host(
        &self,
        configuration: Configuration,
        templates: PromptTemplateCatalog,
        auth: Arc<AuthServices>,
        roots: ApplicationRoots,
        directory: std::path::PathBuf,
    ) -> Arc<LocalHost> {
        Arc::new(LocalHost {
            configuration,
            templates: Arc::new(templates),
            roots,
            directory,
            auth,
            diagnostics: self.diagnostics.clone(),
            owner: tokio::sync::Mutex::new(None),
        })
    }

    async fn serve(
        &self,
        options: ServeOptions,
        cancellation: &dyn Cancellation,
    ) -> Result<i32, DispatchError> {
        let address = options
            .listen
            .as_deref()
            .map(TransportAddress::parse)
            .transpose()
            .map_err(command_failure)?
            .unwrap_or_else(|| TransportAddress::Unix(self.paths.control().join("parrot.sock")));
        address
            .validate_external_listen(options.unsafe_allow_external)
            .map_err(command_failure)?;
        let token = if options.token_file.is_empty() {
            None
        } else {
            Some(TransportToken::load(std::path::Path::new(
                &options.token_file,
            ))?)
        };
        if address.is_tcp() && token.is_none() {
            return Err(command_failure(
                "TCP transport requires --token-file <owner-only-file>",
            ));
        }
        let tls = if address.tcp_url().is_some_and(|url| url.scheme() == "https") {
            Some(https_certificate::load().map_err(command_failure)?)
        } else {
            None
        };
        let (configuration, templates) = self.configuration_and_templates()?;
        let auth = self.auth_services(&configuration)?;
        let local = self.local_host(
            configuration,
            templates,
            auth,
            ApplicationRoots::resolve(&std::env::vars().collect()),
            std::env::current_dir()?,
        );
        let result = async {
            let service = local.service(cancellation).await?;
            let mut server = GrpcServer::start(
                GrpcService::new(service),
                address,
                token,
                tls,
                self.diagnostics.clone(),
            )
            .await
            .map_err(command_failure)?;
            let result = writeln!(io::stderr(), "parrot: listening on {}", server.address())
                .map_err(DispatchError::Io);
            if result.is_ok() {
                cancellation.cancelled().await;
            }
            combine(
                result.map(|()| dispatch::EXIT_SUCCESS),
                server.close().await.map_err(command_failure),
            )
        }
        .await;
        combine(result, local.close().await)
    }

    async fn web(&self, port: u16, cancellation: &dyn Cancellation) -> Result<i32, DispatchError> {
        let (configuration, templates) = self.configuration_and_templates()?;
        let auth = self.auth_services(&configuration)?;
        let roots = ApplicationRoots::resolve(&std::env::vars().collect());
        let directory = std::env::current_dir()?;
        let workspace = ordinary_workspace_resolver()(&directory).map_err(command_failure)?;
        let legacy = configuration.legacy_path_read_fallback;
        let request = wire::CreateSessionRequest {
            model: configuration.model.clone(),
            mode: configuration.runtime.default_profile.clone(),
            ..Default::default()
        };
        let local = self.local_host(
            configuration.clone(),
            templates,
            auth.clone(),
            roots.clone(),
            directory,
        );
        let result = async {
            let client = local.open(cancellation).await.map_err(DispatchError::Rpc)?;
            let startup = Arc::new(
                LocalChatStartup::new(
                    roots.clone(),
                    workspace.clone(),
                    capture_runtime_identity(&host_key()?)?,
                    Arc::new(Mutex::new(io::stderr())),
                    self.diagnostics.clone(),
                    local.opener(),
                    Arc::new(move |_, _| {
                        let request = request.clone();
                        Box::pin(async move { Ok(request) })
                    }),
                    false,
                )
                .with_legacy_path_read_fallback(legacy),
            );
            let router = Arc::new(WebSessionRouter::new(
                client.clone(),
                roots,
                workspace.clone(),
                startup,
                self.diagnostics.clone(),
            ));
            let slash = WebSlashService::new(
                router.clone(),
                client,
                Arc::new(WebCommandFactory::new(
                    Arc::new(Mutex::new(configuration)),
                    auth,
                    VERSION,
                )),
                workspace.launch_directory().to_string_lossy().into_owned(),
            );
            let service = local.service(cancellation).await?;
            let mut server = WebServer::start(
                WebParrotProxy::new(GrpcService::new(service), router.clone()),
                slash,
                port,
            )
            .await
            .map_err(command_failure)?;
            let result = writeln!(
                io::stderr(),
                "parrot: web UI at http://{}",
                server.address()
            )
            .map_err(DispatchError::Io);
            if result.is_ok() {
                cancellation.cancelled().await;
            }
            let result = combine(
                result.map(|()| dispatch::EXIT_SUCCESS),
                server.close().await.map_err(command_failure),
            );
            drop(server);
            drop(router);
            result
        }
        .await;
        combine(result, local.close().await)
    }

    async fn chat(
        &self,
        mut options: ChatOptions,
        cancellation: &dyn Cancellation,
    ) -> Result<i32, DispatchError> {
        let (configuration, templates) = self.configuration_and_templates()?;
        options.apply_configuration(&configuration.model, &configuration.runtime.default_profile);
        let auth = self.auth_services(&configuration)?;
        let configuration = Arc::new(Mutex::new(configuration));
        let stdin_owner = io::stdin();
        #[cfg(unix)]
        let descriptor = {
            use std::os::fd::AsRawFd;
            stdin_owner.as_raw_fd()
        };
        #[cfg(not(unix))]
        let descriptor = 0;
        let mut input = NativeLineInput::borrowed(descriptor);
        let mut redirected = !stdin_owner.is_terminal();
        if options.connect.is_empty() {
            if options.prompt.is_empty() && redirected {
                options.prompt = read_all(&mut input, cancellation).await?.trim().into();
                if options.prompt.is_empty() {
                    return Ok(dispatch::EXIT_USAGE);
                }
                redirected = false;
            }
            let environment: HashMap<_, _> = std::env::vars().collect();
            let roots = ApplicationRoots::resolve(&environment);
            let directory = std::env::current_dir()?;
            let workspace = ordinary_workspace_resolver()(&directory).map_err(command_failure)?;
            let local = self.local_host(
                configuration
                    .lock()
                    .expect("configuration poisoned")
                    .clone(),
                templates,
                auth.clone(),
                roots.clone(),
                directory,
            );
            let open_local = local.opener();
            let request = wire::CreateSessionRequest {
                model: options.model.clone(),
                mode: options.mode.clone(),
                ..Default::default()
            };
            let variant = options.variant.clone();
            let invalid_variant = Arc::new(AtomicBool::new(false));
            let configure: dispatch::startup::ConfigureFresh = {
                let invalid_variant = invalid_variant.clone();
                Arc::new(move |client, cancellation| {
                    let mut request = request.clone();
                    let variant = variant.clone();
                    let invalid_variant = invalid_variant.clone();
                    Box::pin(async move {
                        if let Some(variant) = variant {
                            request.model = override_variant(
                                client,
                                &request.model,
                                &variant,
                                &mut io::stderr(),
                                cancellation,
                            )
                            .await
                            .map_err(dispatch_status)?
                            .ok_or_else(|| {
                                invalid_variant.store(true, Ordering::Relaxed);
                                tonic::Status::invalid_argument("invalid reasoning variant")
                            })?;
                        }
                        Ok(request)
                    })
                })
            };
            let legacy = configuration
                .lock()
                .expect("configuration poisoned")
                .legacy_path_read_fallback;
            let navigation: Option<Arc<dyn TerminalSessionNavigation>> = Some(Arc::new(
                LocalTerminalSessionNavigation::new(
                    roots.clone(),
                    workspace.clone(),
                    self.diagnostics.clone(),
                    open_local.clone(),
                )
                .with_legacy_path_read_fallback(legacy),
            ));
            let startup = LocalChatStartup::new(
                roots,
                workspace,
                capture_runtime_identity(&host_key()?)?,
                Arc::new(Mutex::new(io::stderr())),
                self.diagnostics.clone(),
                open_local,
                configure,
                options.take_over,
            )
            .with_legacy_path_read_fallback(legacy);
            let result = async {
                let opened = match startup.open(options.prompt.is_empty(), cancellation).await {
                    Ok(opened) => opened,
                    Err(_) if invalid_variant.load(Ordering::Relaxed) => {
                        cancellation.check().map_err(|_| DispatchError::Cancelled)?;
                        return Ok(dispatch::EXIT_USAGE);
                    }
                    Err(failure) => return Err(DispatchError::Rpc(failure.into_status())),
                };
                let initial = Some(opened.session);
                let mut connection = opened.remote;
                let result = self
                    .run_chat(
                        opened.client,
                        options,
                        configuration,
                        auth,
                        initial,
                        navigation,
                        input,
                        redirected,
                        cancellation,
                    )
                    .await;
                if let Some(owner) = &mut connection {
                    owner.close();
                }
                result
            }
            .await;
            return combine(result, local.close().await);
        }
        let address = match TransportAddress::parse(&options.connect) {
            Ok(address) => address,
            Err(failure) => {
                eprintln!("parrot: {failure}");
                return Ok(dispatch::EXIT_USAGE);
            }
        };
        let token = if options.token_file.is_empty() {
            None
        } else {
            Some(TransportToken::load(std::path::Path::new(
                &options.token_file,
            ))?)
        };
        if address.is_tcp() && token.is_none() {
            eprintln!("parrot: TCP transport requires --token-file <owner-only-file>");
            return Ok(dispatch::EXIT_USAGE);
        }
        eprintln!("parrot: opening a remote user session...");
        io::stderr().flush()?;
        let mut owner =
            GrpcTransportClient::connect(&address, token.as_ref(), None, self.diagnostics.clone())
                .map_err(command_failure)?;
        let client = owner.client().map_err(command_failure)?.clone();
        let result = async {
            if let Some(variant) = &options.variant {
                let Some(model) = override_variant(
                    client.clone(),
                    &options.model,
                    variant,
                    &mut io::stderr(),
                    cancellation,
                )
                .await?
                else {
                    return Ok(dispatch::EXIT_USAGE);
                };
                options.model = model;
            }
            self.run_chat(
                client,
                options,
                configuration,
                auth,
                None,
                None,
                input,
                redirected,
                cancellation,
            )
            .await
        }
        .await;
        owner.close();
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_chat(
        &self,
        client: parrot_host::web::router::WebClient,
        options: ChatOptions,
        configuration: Arc<Mutex<Configuration>>,
        auth: Arc<AuthServices>,
        initial_session: Option<wire::UserSession>,
        navigation: Option<Arc<dyn TerminalSessionNavigation>>,
        input: NativeLineInput,
        input_redirected: bool,
        cancellation: &dyn Cancellation,
    ) -> Result<i32, DispatchError> {
        let session = wire::CreateSessionRequest {
            model: options.model,
            mode: options.mode,
            ..Default::default()
        };
        let workspace =
            ordinary_workspace_resolver()(&std::env::current_dir()?).map_err(command_failure)?;
        let reads = ProductionReadContext::new(
            &configuration.lock().expect("configuration poisoned"),
            workspace,
        )
        .map_err(command_failure)?;
        let uploader = Arc::new(PromptAttachmentUploader::new(reads.reader, reads.modes));
        let prepare: PreparePrompt = Arc::new(
            move |mut client, session, mode, entered, delivery, error, cancellation| {
                let uploader = uploader.clone();
                Box::pin(async move {
                    uploader
                        .prepare(
                            &mut client,
                            session,
                            mode,
                            entered,
                            delivery,
                            error,
                            cancellation,
                        )
                        .await
                })
            },
        );
        let raw = if options.basic || !io::stdout().is_terminal() {
            None
        } else {
            RawTerminal::open_stdin()?
        };
        if let Some(raw) = raw {
            EnhancedCli::new(
                client,
                self.interrupts.clone(),
                auth,
                configuration,
                EnhancedChatRequest {
                    session,
                    prompt: options.prompt,
                    initial_session,
                    navigation,
                },
                VERSION.into(),
                EnhancedTerminal::new(
                    raw,
                    Box::new(io::stdout()),
                    Box::new(io::stderr()),
                    Arc::new(terminal_dimensions),
                    enhanced_terminal_color_enabled(),
                ),
                prepare,
            )
            .run(cancellation)
            .await
            .map_err(enhanced_failure)
        } else {
            BasicCli::new(
                client,
                self.interrupts.clone(),
                auth,
                configuration,
                BasicChatRequest {
                    session,
                    prompt: options.prompt,
                    initial_session,
                    input_redirected,
                    navigation,
                },
                input,
                Box::new(io::stdout()),
                Box::new(io::stderr()),
                VERSION.into(),
                prepare,
            )
            .run(cancellation)
            .await
            .map_err(basic_failure)
        }
    }
}

/// Lazy composition lets ordinary startup attach to a live session without
/// constructing a second local host or its configured providers.
struct LocalHost {
    configuration: Configuration,
    templates: Arc<PromptTemplateCatalog>,
    roots: ApplicationRoots,
    directory: std::path::PathBuf,
    auth: Arc<AuthServices>,
    diagnostics: Arc<FileDiagnosticLog>,
    owner: tokio::sync::Mutex<Option<LocalHostOwner>>,
}

struct LocalHostOwner {
    host: ProductionHost,
    providers: dispatch::provider_catalog::ProviderCatalog,
}

impl LocalHost {
    fn opener(self: &Arc<Self>) -> OpenLocalClient {
        let local = self.clone();
        Arc::new(move |cancellation| {
            let local = local.clone();
            Box::pin(async move { local.open(cancellation).await })
        })
    }

    async fn owner(
        &self,
        cancellation: &dyn Cancellation,
    ) -> Result<tokio::sync::MutexGuard<'_, Option<LocalHostOwner>>, DispatchError> {
        cancellation.check().map_err(|_| DispatchError::Cancelled)?;
        let mut owner = self.owner.lock().await;
        if owner.is_none() {
            let mut providers = dispatch::provider_catalog::build(
                self.configuration.clone(),
                self.auth.credentials.clone(),
                self.auth.oauth.clone(),
                cancellation,
            )
            .await?;
            let host = ProductionHost::new(ProductionHostConfig {
                configuration: self.configuration.clone(),
                templates: self.templates.clone(),
                roots: self.roots.clone(),
                launch_directory: self.directory.clone(),
                providers: providers.shared_registry(),
                diagnostics: self.diagnostics.clone(),
                owner_runtime: tokio::runtime::Handle::current(),
            });
            match host {
                Ok(host) => *owner = Some(LocalHostOwner { host, providers }),
                Err(failure) => {
                    return combine(Err(command_failure(failure)), providers.close().await);
                }
            }
        }
        Ok(owner)
    }

    async fn service(
        &self,
        cancellation: &dyn Cancellation,
    ) -> Result<Arc<parrot_host::composition::protocol::ParrotService>, DispatchError> {
        let owner = self.owner(cancellation).await?;
        Ok(owner
            .as_ref()
            .expect("local host initialized")
            .host
            .service()
            .clone())
    }

    async fn open(&self, cancellation: &dyn Cancellation) -> Result<WebClient, tonic::Status> {
        let owner = self.owner(cancellation).await.map_err(dispatch_status)?;
        owner
            .as_ref()
            .expect("local host initialized")
            .host
            .open_local_client(cancellation)
            .await
    }

    async fn close(&self) -> Result<(), DispatchError> {
        let mut retained = self.owner.lock().await;
        let Some(owner) = retained.as_mut() else {
            return Ok(());
        };
        let result = owner.host.close().await.map_err(command_failure);
        let providers = owner.providers.close().await;
        retained.take();
        combine(result, providers)
    }
}

fn oauth_client() -> Result<Arc<dyn OAuthClient>, DispatchError> {
    let http = reqwest::Client::builder()
        .build()
        .map_err(command_failure)?;
    Ok(Arc::new(OpenAiOAuthClient::new(
        http,
        Arc::new(SystemBrowserOpener::default()),
        OpenAiOAuthOptions::default(),
    )))
}

async fn read_all(
    input: &mut impl DialogInput,
    cancellation: &dyn Cancellation,
) -> Result<String, DispatchError> {
    let mut bytes = Vec::new();
    while let Some(byte) = input.read_byte(cancellation).await? {
        bytes.push(byte);
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

async fn override_variant(
    mut client: parrot_host::web::router::WebClient,
    selector: &str,
    variant: &str,
    error: &mut (dyn Write + Send),
    cancellation: &dyn Cancellation,
) -> Result<Option<String>, DispatchError> {
    let canonical = model_alias::resolve(&client, selector, cancellation)
        .await
        .map_err(slash_failure)?;
    let listed = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(DispatchError::Cancelled),
        response = client.list_models(wire::ListModelsRequest::new()) => response.map_err(DispatchError::Rpc)?.into_inner(),
    };
    let Some(current) = ModelSelection::resolve(&listed.models, &canonical) else {
        writeln!(error, "parrot: unknown model selection {selector}")?;
        return Ok(None);
    };
    let Some(replacement) = current.with_variant(variant) else {
        writeln!(
            error,
            "parrot: model {} does not support effort {variant}; choose one of: {}",
            current.base_selector(),
            current
                .model
                .variants
                .iter()
                .map(|variant| variant.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )?;
        return Ok(None);
    };
    Ok(Some(replacement.selector()))
}

fn slash_failure(failure: parrot_cli::slash_sessions::SlashFailure) -> DispatchError {
    match failure {
        parrot_cli::slash_sessions::SlashFailure::Rpc(failure) => DispatchError::Rpc(failure),
        other => command_failure(other),
    }
}
fn enhanced_failure(failure: parrot_cli::terminal_dialogs::EnhancedFailure) -> DispatchError {
    use parrot_cli::terminal_dialogs::EnhancedFailure;
    match failure {
        EnhancedFailure::Rpc(status) => DispatchError::Rpc(status),
        EnhancedFailure::Slash(failure) => slash_failure(failure),
        EnhancedFailure::Io(failure) => DispatchError::Io(failure),
        EnhancedFailure::Attachment(failure) => attachment_failure(failure),
        EnhancedFailure::Navigation(failure) => navigation_failure(failure),
    }
}
fn basic_failure(failure: dispatch::basic_loop::BasicFailure) -> DispatchError {
    use dispatch::basic_loop::BasicFailure;
    match failure {
        BasicFailure::Rpc(status) => DispatchError::Rpc(status),
        BasicFailure::Slash(failure) => slash_failure(failure),
        BasicFailure::Io(failure) => DispatchError::Io(failure),
        BasicFailure::Attachment(failure) => attachment_failure(failure),
        BasicFailure::Navigation(failure) => navigation_failure(failure),
    }
}
fn attachment_failure(failure: AttachmentError) -> DispatchError {
    match failure {
        AttachmentError::Cancelled => DispatchError::Cancelled,
        AttachmentError::Io(failure) => DispatchError::Io(failure),
        AttachmentError::Rpc(status) => DispatchError::Rpc(status),
        other => command_failure(other),
    }
}
fn navigation_failure(failure: parrot_cli::slash_sessions::NavigationFailure) -> DispatchError {
    use parrot_cli::slash_sessions::NavigationFailure;
    match failure {
        NavigationFailure::Rpc(status) => DispatchError::Rpc(status),
        NavigationFailure::Recoverable(message) | NavigationFailure::Other(message) => {
            DispatchError::Command(message)
        }
    }
}
fn dispatch_status(failure: DispatchError) -> tonic::Status {
    match failure {
        DispatchError::Cancelled => tonic::Status::cancelled("operation cancelled"),
        DispatchError::Rpc(status) => status,
        other => tonic::Status::internal(other.to_string()),
    }
}
fn command_failure(failure: impl std::fmt::Display) -> DispatchError {
    DispatchError::Command(failure.to_string())
}
fn combine<T>(
    result: Result<T, DispatchError>,
    cleanup: Result<(), DispatchError>,
) -> Result<T, DispatchError> {
    match (result, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(failure), Ok(())) | (Ok(_), Err(failure)) => Err(failure),
        (Err(failure), Err(cleanup)) => Err(DispatchError::Command(format!(
            "{failure}; cleanup failed: {cleanup}"
        ))),
    }
}

fn enhanced_terminal_color_enabled() -> bool {
    std::env::var_os("NO_COLOR").is_none()
}

fn terminal_dimensions() -> (isize, isize) {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        #[repr(C)]
        #[derive(Default)]
        struct WindowSize {
            rows: u16,
            columns: u16,
            x: u16,
            y: u16,
        }
        unsafe extern "C" {
            fn ioctl(fd: i32, request: usize, size: *mut WindowSize) -> i32;
        }
        #[cfg(target_os = "linux")]
        const REQUEST: usize = 0x5413;
        #[cfg(target_os = "macos")]
        const REQUEST: usize = 0x4008_7468;
        let mut size = WindowSize::default();
        // SAFETY: size has libc winsize layout and stdout remains open.
        if unsafe { ioctl(1, REQUEST, &mut size) } == 0 && size.columns != 0 && size.rows != 0 {
            return (
                isize::try_from(size.columns).unwrap_or(80),
                isize::try_from(size.rows).unwrap_or(24),
            );
        }
    }
    (80, 24)
}

#[cfg(test)]
#[path = "dispatch/process_tests.rs"]
mod tests;
