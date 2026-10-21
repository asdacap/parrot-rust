//! Production capability and service composition for local native/web clients.

pub mod filesystem;
pub mod launcher;
pub mod permissions;
pub mod policy;
mod skills;

#[cfg(test)]
mod tests;

use crate::{
    composition::{
        factory::{
            scope::{AgentSourceConfig, AuthoritativeAgentSource},
            user::{
                AgentFactoryParts, AgentFactorySource, CatalogUserSkills, UserServices,
                UserSessionFactory, UserSessionFactoryConfig, durable_permission_factory,
                ordinary_workspace_resolver,
            },
        },
        protocol::{
            self, HostedSession, ParrotService, RpcCode, RpcError, SessionHost, SessionListener,
            models::ModelOperations,
        },
    },
    grpc::{
        address::TransportAddress,
        service::GrpcService,
        transport::{GrpcServer, GrpcTransportClient},
    },
    web::router::WebClient,
};
use filesystem::ProductionFilesystem;
use launcher::ProductionLauncher;
use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation, OperationResult},
        filesystem::{
            AuthorizedReadService, BoundRead, FileMetadata, FileReadError, LinkPolicy,
            ReadConsumer, ReadRequest,
        },
        security::SecurityProvider,
    },
    values::{config::PathPlatform, identifiers::Identifier},
};
use parrot_foundation::{
    config::{Configuration, runtime::SandboxRuleAction},
    paths::{AgentScratchDirectory, ApplicationRoots, UserSessionResources},
    template_catalog::PromptTemplateCatalog,
};
use parrot_provider::routing::{
    aliases::{ModelAliasCatalog, ModelRouter, ModelRouting},
    configuration::{ModelConfigurationCoordinator, definitions_from_config},
    registry::ProviderRegistry,
};
use parrot_runtime::{
    agent_turn::{
        lifecycle::{IncomingActivity, OperationHandle, SendResult},
        services::{SelectionSource, TurnProfile, TurnSelection},
    },
    children::{
        AgentIdentity, ChildDiagnostic, ChildDiagnostics, ChildError, ChildProfile, ChildResult,
        IdGenerator, PreparedScope, ProfileResolver, ScopeFactory, ScopeParts, ScopedSession,
    },
    interactions::{BrokerDiagnostic, InteractionDiagnostics},
    plan_mode::{
        MaterializedModeProfile, ModeRegistry, ModeWorkflowFactory, PlanModeWorkflowFactory,
        RetainedModePolicyMaterializer,
    },
    prompt_context::{
        AgentsPromptProvider, ModelPromptProvider, RuleActionDescription,
        SecurityProfilePromptProvider, SecurityRuleDescription, SessionIdentityPromptProvider,
        SkillCatalog, SkillDiscoveryContext, StaticSystemPromptProvider, SystemPromptProvider,
        TemplateSystemPromptProvider, native_platform_identifier, resolve_skill_roots,
    },
    tool_settlement::SharedSecurity,
};
use parrot_storage::{
    diagnostics::FileDiagnosticLog,
    history_files::SessionCatalog,
    ownership::{
        OwnershipRoots, SessionAdmissions, SessionModelRouter, SessionModelSelection, SessionStore,
        SessionStoreError, SessionWorkspaceResolver, WorkingDirectoryClaim,
        capture_runtime_identity, host_key,
    },
};
use parrot_tools::{
    process::execution::{
        locator::{CliUtilityAvailability, ExecutableLocator},
        runner::SandboxGate,
    },
    task_artifacts::TaskArtifactParserAdapter,
    web_fetch::{AddressPolicy, PrivateAddressPolicy, PublicAddressPolicy, WebFetcher},
};
use permissions::ProductionPermissionAuthority;
use policy::{ProductionPolicy, ProductionPolicyMaterializer};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::Mutex as AsyncMutex;

fn internal(error: impl std::fmt::Display) -> RpcError {
    RpcError::new(RpcCode::Internal, error.to_string())
}

/// The same configured policies and real authorized reader used for native
/// prompt attachments, including clients connected to a remote session.
pub struct ProductionReadContext {
    pub reader: Arc<ProductionReader>,
    pub modes: Arc<ModeRegistry<ProductionPolicy>>,
}
impl ProductionReadContext {
    pub fn new(
        configuration: &Configuration,
        workspace: parrot_domain::values::project_workspace::ProjectWorkspace,
    ) -> protocol::Result<Self> {
        let profiles = ProductionProfiles::new(configuration)?;
        Ok(Self {
            reader: Arc::new(ProductionReader(
                ProductionFilesystem::new(workspace).map_err(internal)?,
            )),
            modes: Arc::new(
                ModeRegistry::new(
                    profiles.modes(configuration)?,
                    &configuration.runtime.default_profile,
                )
                .map_err(internal)?,
            ),
        })
    }
}

pub struct ProductionHostConfig {
    pub configuration: Configuration,
    pub templates: Arc<PromptTemplateCatalog>,
    pub roots: ApplicationRoots,
    pub launch_directory: PathBuf,
    pub providers: Arc<ProviderRegistry>,
    pub diagnostics: Arc<FileDiagnosticLog>,
    /// Its actual Runtime is retained by the executable until close completes.
    pub owner_runtime: tokio::runtime::Handle,
}

/// Owns the production service and the lazy local-client transport. External
/// transports borrow `service()` and must be joined before this owner is closed.
pub struct ProductionHost {
    service: Arc<ParrotService>,
    diagnostics: Arc<FileDiagnosticLog>,
    local_path: PathBuf,
    local: AsyncMutex<Option<LocalTransport>>,
    closing: AtomicBool,
}
struct LocalTransport {
    server: GrpcServer,
    client: GrpcTransportClient,
}
impl ProductionHost {
    pub fn new(config: ProductionHostConfig) -> protocol::Result<Self> {
        let workspace = ordinary_workspace_resolver()(&config.launch_directory)?;
        let skill_directory = config.roots.current().skills_directory();
        skills::install_packaged_skills(&skill_directory).map_err(internal)?;
        let user_home = config.roots.user_home().to_owned();
        let aliases = Arc::new(
            ModelAliasCatalog::new(
                config.providers.clone(),
                definitions_from_config(&config.configuration.models.model_aliases)
                    .map_err(internal)?,
            )
            .map_err(internal)?,
        );
        let routing = Arc::new(ModelRouting::new(aliases, &config.configuration.model));
        let mut initial_routing = (*routing.capture()).clone();
        initial_routing.context_limit = config.configuration.models.context_limit.clone();
        routing.publish(Arc::new(initial_routing));
        let router = Arc::new(ModelRouter::new(config.providers.clone(), routing.clone()));
        let coordinator = Arc::new(ModelConfigurationCoordinator::new(
            config.configuration.clone(),
            routing,
            router.clone(),
        ));
        let models = Arc::new(ModelOperations::new(
            coordinator,
            config.providers,
            router.clone(),
        ));
        let materializer = Arc::new(ProductionPolicyMaterializer::new());
        let profiles = Arc::new(ProductionProfiles::new(&config.configuration)?);
        let modes = Arc::new(
            ModeRegistry::new(
                profiles.modes(&config.configuration)?,
                &config.configuration.runtime.default_profile,
            )
            .map_err(internal)?,
        );
        let gate = Arc::new(SandboxGate::new(
            config.configuration.runtime.sandbox_enabled,
        ));
        let configuration = Arc::new(config.configuration);
        let source = Arc::new(ProductionSource {
            configuration: configuration.clone(),
            router: router.clone(),
            templates: config.templates.clone(),
            materializer: materializer.clone(),
            gate: gate.clone(),
            configuration_directory: config.roots.current().config().to_owned(),
            utilities: CliUtilityAvailability::inspect(
                &configuration.runtime.cli_utilities.expected,
                &configuration.runtime.cli_utilities.optional,
                &ExecutableLocator::capture(),
            ),
        });
        let skills_configuration = Arc::new(Mutex::new(configuration.as_ref().clone()));
        let skill_templates = config.templates.clone();
        let diagnostics = Arc::new(ProductionDiagnostics(config.diagnostics.clone()));
        let timeout = (configuration.runtime.user_input_timeout_ms >= 0)
            .then(|| Duration::from_millis(configuration.runtime.user_input_timeout_ms as u64));
        let factory = Arc::new(UserSessionFactory::new(UserSessionFactoryConfig {
            agent_limits: configuration.runtime.agent_limits,
            router: router.clone(),
            modes,
            workflows: vec![(
                "plan".into(),
                Arc::new(PlanModeWorkflowFactory::new(
                    config.templates.clone(),
                    Arc::new(TaskArtifactParserAdapter),
                )) as Arc<dyn ModeWorkflowFactory<ProductionPolicy>>,
            )],
            materializer,
            source,
            profiles,
            ids: diagnostics.clone(),
            diagnostics: diagnostics.clone(),
            interaction_diagnostics: diagnostics.clone(),
            templates: config.templates,
            workspace: ordinary_workspace_resolver(),
            launch_directory: config.launch_directory.clone(),
            skills: Arc::new(move |workspace| {
                let reader = Arc::new(ProductionReader(
                    ProductionFilesystem::new(workspace.clone()).map_err(internal)?,
                ));
                let capture = skills_configuration.clone();
                let persist = skills_configuration.clone();
                let platform = if cfg!(target_os = "macos") {
                    PathPlatform::MacOs
                } else {
                    PathPlatform::Unix
                };
                let roots = resolve_skill_roots(workspace, &user_home, &skill_directory, platform)
                    .map_err(internal)?;
                let catalog = SkillCatalog::new(
                    reader.clone(),
                    roots,
                    SkillDiscoveryContext {
                        working_directory: workspace.launch_directory().to_owned(),
                        platform,
                    },
                    Arc::new(move || {
                        let config = capture.lock().expect("skill configuration poisoned");
                        (config.runtime.skills.clone(), config.skills_generation)
                    }),
                )
                .with_configuration(Arc::new(move |path, enabled| {
                    persist
                        .lock()
                        .expect("skill configuration poisoned")
                        .set_skill_enabled(path, enabled)
                        .map_err(|error| {
                            FileReadError::new(
                                parrot_domain::contracts::filesystem::FileReadErrorKind::Io,
                                error.to_string(),
                            )
                        })
                }));
                Ok(Arc::new(CatalogUserSkills {
                    catalog: Arc::new(catalog),
                    reader,
                    templates: skill_templates.clone(),
                    working_directory: workspace.launch_directory().to_string_lossy().into_owned(),
                }))
            }),
            permissions: durable_permission_factory(timeout, diagnostics),
            user_input_timeout: timeout,
        }));
        let runtime = capture_runtime_identity(&host_key().map_err(internal)?).map_err(internal)?;
        let current_state = config.roots.current().state().to_owned();
        let admissions = Arc::new(SessionAdmissions::new(
            OwnershipRoots::new(
                current_state.clone(),
                config.roots.legacy().state().to_owned(),
            )
            .map_err(internal)?,
            runtime.clone(),
            configuration.legacy_path_read_fallback,
        ));
        let store = Arc::new(SessionStore::new(
            admissions,
            config.launch_directory,
            configuration.runtime.default_profile.clone(),
            factory.clone(),
            Arc::new(StorageRouter(router)),
            Arc::new(StorageWorkspace),
            config.diagnostics.clone(),
        ));
        let host = Arc::new(NativeSessionHost {
            roots: config.roots,
            workspace,
            service: OnceLock::new(),
            diagnostics: config.diagnostics.clone(),
        });
        let service = Arc::new(ParrotService::new(
            models,
            store,
            SessionCatalog::new(&current_state),
            factory,
            host.clone(),
            Arc::new(WorkingDirectoryClaim::new(&current_state, runtime)),
            gate,
            config.diagnostics.clone(),
            config.owner_runtime,
        ));
        host.service
            .set(Arc::downgrade(&service))
            .map_err(|_| internal("production service was already bound"))?;
        let local_path = current_state.join("control").join(format!(
            "local-{}.sock",
            FileDiagnosticLog::create_instance_id()
        ));
        Ok(Self {
            service,
            diagnostics: config.diagnostics,
            local_path,
            local: AsyncMutex::new(None),
            closing: AtomicBool::new(false),
        })
    }
    pub fn service(&self) -> &Arc<ParrotService> {
        &self.service
    }
    pub async fn open_local_client(
        &self,
        cancellation: &dyn Cancellation,
    ) -> Result<WebClient, tonic::Status> {
        cancellation
            .check()
            .map_err(|_| tonic::Status::cancelled("operation cancelled"))?;
        let mut local = self.local.lock().await;
        if self.closing.load(Ordering::Acquire) {
            return Err(tonic::Status::unavailable("local service is closed"));
        }
        if local.is_none() {
            let server = GrpcServer::start_local(
                GrpcService::new(self.service.clone()),
                &self.local_path,
                self.diagnostics.clone(),
            )
            .await
            .map_err(|e| tonic::Status::internal(e.to_string()))?;
            let client = GrpcTransportClient::connect(
                &TransportAddress::Unix(self.local_path.clone()),
                None,
                None,
                self.diagnostics.clone(),
            )
            .map_err(|e| tonic::Status::internal(e.to_string()))?;
            *local = Some(LocalTransport { server, client });
        }
        local
            .as_mut()
            .expect("local transport initialized")
            .client
            .client()
            .cloned()
            .map_err(|e| tonic::Status::internal(e.to_string()))
    }
    pub async fn close(&self) -> protocol::Result<()> {
        self.closing.store(true, Ordering::Release);
        let mut failure = None;
        let mut local = self.local.lock().await;
        if let Some(local) = local.as_mut() {
            if let Err(error) = local.server.close().await {
                failure = Some(internal(error));
            }
            local.client.close();
        }
        local.take();
        if let Err(error) = self.service.close().await {
            failure.get_or_insert(error);
        }
        failure.map_or(Ok(()), Err)
    }
}

struct ProductionSource {
    configuration: Arc<Configuration>,
    router: Arc<ModelRouter>,
    templates: Arc<PromptTemplateCatalog>,
    materializer: Arc<ProductionPolicyMaterializer>,
    gate: Arc<SandboxGate>,
    configuration_directory: PathBuf,
    utilities: CliUtilityAvailability,
}

struct InstructionSecurity(ProductionPolicy);
impl SecurityProvider for InstructionSecurity {
    type Snapshot = ProductionPolicy;

    fn capture(&self) -> ProductionPolicy {
        self.0.clone()
    }
}

impl AgentFactorySource<ProductionPolicy> for ProductionSource {
    fn create(
        &self,
        services: Arc<UserServices<ProductionPolicy>>,
    ) -> protocol::Result<AgentFactoryParts<ProductionPolicy>> {
        self.materializer
            .register_session(
                services.resources.root().to_owned(),
                services.resources.workspace().clone(),
            )
            .map_err(internal)?;
        let filesystem =
            ProductionFilesystem::new(services.resources.workspace().clone()).map_err(internal)?;
        let launcher = Arc::new(
            ProductionLauncher::with_device_paths(
                self.gate.clone(),
                services.resources.workspace().launch_directory().to_owned(),
                services.resources.queue_directory(),
                self.configuration
                    .runtime
                    .sandbox_dev_bind
                    .iter()
                    .map(PathBuf::from)
                    .collect(),
            )
            .map_err(internal)?,
        );
        let materializer = self.materializer.clone();
        let templates = self.templates.clone();
        let mut prompts: Vec<Arc<dyn SystemPromptProvider>> = self
            .configuration
            .system_prompts
            .iter()
            .map(|(name, text)| {
                Arc::new(StaticSystemPromptProvider::new(
                    format!("configured:{name}"),
                    text.clone(),
                )) as Arc<dyn SystemPromptProvider>
            })
            .collect();
        // Instructions are host context, not a file-tool invocation under the
        // selected profile. Keep this read-only capture separate from tool grants.
        prompts.push(Arc::new(
            AgentsPromptProvider::new(
                &services
                    .resources
                    .workspace()
                    .launch_directory()
                    .to_string_lossy(),
                &self.configuration_directory.to_string_lossy(),
                Arc::new(ProductionReader(filesystem.clone())),
                Arc::new(InstructionSecurity(
                    ProductionPolicy::compose(true, &[], &[], &[]).map_err(internal)?,
                )),
                templates.clone(),
            )
            .map_err(internal)?,
        ));
        let utilities = |entries: &[String]| {
            if entries.is_empty() {
                "none".to_owned()
            } else {
                entries.join(", ")
            }
        };
        let mut children = self
            .configuration
            .runtime
            .profiles
            .iter()
            .filter(|(_, profile)| profile.is_agent_selectable)
            .collect::<Vec<_>>();
        children.sort_by(|(left, _), (right, _)| left.encode_utf16().cmp(right.encode_utf16()));
        let subagents = children
            .into_iter()
            .map(|(id, profile)| serde_json::json!({"id": id, "usage": profile.usage}))
            .collect::<Vec<_>>();
        let context = [
            (
                "05b-optional-cli-utilities",
                "context.optional-cli-utilities",
                vec![(
                    "utilities".into(),
                    utilities(self.utilities.available_optional()).into(),
                )],
            ),
            (
                "06-cli-utilities",
                "context.expected-cli-utilities",
                vec![(
                    "utilities".into(),
                    utilities(self.utilities.available_expected()).into(),
                )],
            ),
            (
                "07-platform",
                "context.platform",
                vec![("platform".into(), native_platform_identifier().into())],
            ),
            (
                "12-working-directory",
                "system.working-directory",
                vec![(
                    "working_directory".into(),
                    services
                        .resources
                        .workspace()
                        .launch_directory()
                        .to_string_lossy()
                        .into_owned()
                        .into(),
                )],
            ),
            (
                "13-git-repository",
                "context.git-repository",
                vec![(
                    "is_repository".into(),
                    services
                        .resources
                        .workspace()
                        .is_git_repository()
                        .to_string()
                        .into(),
                )],
            ),
            (
                "15-subagents",
                "context.subagents",
                vec![("subagents".into(), subagents.into())],
            ),
        ];
        prompts.extend(context.into_iter().map(|(key, template, arguments)| {
            Arc::new(TemplateSystemPromptProvider::new(
                format!("runtime:system-context:{key}"),
                template,
                arguments,
                templates.clone(),
            )) as Arc<dyn SystemPromptProvider>
        }));
        prompts.push(Arc::new(SessionIdentityPromptProvider::new(
            templates.clone(),
        )));
        let sandbox = self.gate.clone();
        prompts.push(Arc::new(SecurityProfilePromptProvider::new(
            self.configuration
                .runtime
                .sandbox_rules
                .iter()
                .map(|rule| SecurityRuleDescription {
                    path: rule.path.clone(),
                    action: match rule.action {
                        SandboxRuleAction::AllowWrite => RuleActionDescription::AllowWrite,
                        SandboxRuleAction::DenyRead => RuleActionDescription::DenyRead,
                        SandboxRuleAction::AllowRead => RuleActionDescription::AllowRead,
                        SandboxRuleAction::DenyWrite => RuleActionDescription::DenyWrite,
                    },
                })
                .collect(),
            Arc::new(move || sandbox.enabled()),
            templates.clone(),
        )));
        prompts.push(Arc::new(ModelPromptProvider::new(
            self.configuration.models.model_profiles.clone(),
            self.router.clone(),
            templates,
        )));
        let address_policy: Arc<dyn AddressPolicy> =
            if self.configuration.runtime.web_fetch_allow_private {
                Arc::new(PrivateAddressPolicy)
            } else {
                Arc::new(PublicAddressPolicy)
            };
        let actual = AuthoritativeAgentSource::new(AgentSourceConfig {
            configuration: self.configuration.clone(),
            router: self.router.clone(),
            filesystem,
            launcher,
            permission_authority: Arc::new(move |user, identity| {
                ProductionPermissionAuthority::new(user, identity, materializer.clone())
                    .map(Arc::new)
            }),
            web_fetcher: Arc::new(WebFetcher::production(address_policy)),
            prompts,
            clock: Arc::new(parrot_runtime::interactions::TokioClock),
            profiles: self.configuration.models.model_profiles.clone(),
        });
        let parts = actual.create(services.clone())?;
        Ok(AgentFactoryParts {
            factory: Arc::new(MaterializingFactory {
                inner: parts.factory,
                services,
                materializer: self.materializer.clone(),
            }),
            bind: parts.bind,
        })
    }
}
struct MaterializingFactory {
    inner: Arc<dyn ScopeFactory<ProductionPolicy>>,
    services: Arc<UserServices<ProductionPolicy>>,
    materializer: Arc<ProductionPolicyMaterializer>,
}
impl ScopeFactory<ProductionPolicy> for MaterializingFactory {
    fn create(
        &self,
        prepared: PreparedScope<ProductionPolicy>,
    ) -> BoxFuture<'_, ChildResult<ScopeParts<ProductionPolicy>>> {
        Box::pin(async move {
            let names: Vec<_> = prepared
                .identity
                .name_path
                .iter()
                .map(String::as_str)
                .collect();
            let scratch = self
                .services
                .resources
                .agent_scratch(&names)
                .map_err(|e| ChildError::registry(e.to_string()))?;
            let mut selection = prepared.selection.capture().map_err(ChildError::from)?;
            selection.security = self
                .materializer
                .for_agent(&selection.security, &scratch)
                .map_err(|e| ChildError::registry(e.to_string()))?;
            prepared
                .selection
                .update(selection)
                .map_err(ChildError::from)?;
            let mut parts = self.inner.create(prepared).await?;
            parts.session = Arc::new(MaterializingSession {
                inner: parts.session,
                scratch,
                materializer: self.materializer.clone(),
            });
            Ok(parts)
        })
    }
}
struct MaterializingSession {
    inner: Arc<dyn ScopedSession<ProductionPolicy>>,
    scratch: AgentScratchDirectory,
    materializer: Arc<ProductionPolicyMaterializer>,
}
impl ScopedSession<ProductionPolicy> for MaterializingSession {
    fn identity(&self) -> &Arc<AgentIdentity> {
        self.inner.identity()
    }
    fn selection(&self) -> &Arc<dyn SelectionSource<ProductionPolicy>> {
        self.inner.selection()
    }
    fn is_active(&self) -> bool {
        self.inner.is_active()
    }
    fn update_selection(&self, mut selection: TurnSelection<ProductionPolicy>) -> ChildResult<()> {
        selection.security = self
            .materializer
            .for_agent(&selection.security, &self.scratch)
            .map_err(|e| ChildError::registry(e.to_string()))?;
        self.inner.update_selection(selection)
    }
    fn send(
        &self,
        parts: Vec<parrot_storage::events::ConversationPart>,
        id: String,
        delivery: parrot_domain::protocol::messages::parrot::Delivery,
        activity: IncomingActivity,
        cancellation: Arc<dyn Cancellation>,
    ) -> ChildResult<OperationHandle<SendResult>> {
        self.inner.send(parts, id, delivery, activity, cancellation)
    }
    fn close(&self) -> BoxFuture<'_, ChildResult<()>> {
        self.inner.close()
    }
}

struct ProductionProfiles {
    profiles: std::collections::BTreeMap<
        String,
        (
            parrot_foundation::config::runtime::ProfileConfig,
            Arc<ProductionPolicy>,
        ),
    >,
    disabled: Vec<String>,
}
impl ProductionProfiles {
    fn new(configuration: &Configuration) -> protocol::Result<Self> {
        let profiles = configuration
            .runtime
            .profiles
            .iter()
            .map(|(name, config)| {
                Ok((
                    name.clone(),
                    (
                        config.clone(),
                        Arc::new(
                            ProductionPolicy::compose(
                                config.read_only,
                                &config.sandbox_rules,
                                &configuration.runtime.sandbox_rules,
                                &[],
                            )
                            .map_err(internal)?,
                        ),
                    ),
                ))
            })
            .collect::<protocol::Result<_>>()?;
        Ok(Self {
            profiles,
            disabled: configuration
                .runtime
                .disabled_tools
                .iter()
                .cloned()
                .collect(),
        })
    }
    fn modes(
        &self,
        _: &Configuration,
    ) -> protocol::Result<Vec<MaterializedModeProfile<ProductionPolicy>>> {
        self.profiles
            .iter()
            .map(|(id, (config, security))| {
                MaterializedModeProfile::new(id.clone(), config, &self.disabled, security.clone())
                    .map_err(internal)
            })
            .collect()
    }
}
impl ProfileResolver<ProductionPolicy> for ProductionProfiles {
    fn resolve_child(&self, id: &str) -> ChildResult<ChildProfile<ProductionPolicy>> {
        let selected = if id == "explore"
            && self
                .profiles
                .get("explorer")
                .is_some_and(|(config, _)| config.is_agent_selectable)
        {
            "explorer"
        } else {
            id
        };
        let (configuration, security) = self
            .profiles
            .get(selected)
            .filter(|(config, _)| config.is_agent_selectable)
            .ok_or_else(|| ChildError::registry(format!("unknown child profile {id}")))?;
        Ok(ChildProfile {
            profile: TurnProfile {
                id: selected.into(),
                maximum_requests: configuration.max_turns,
                allowed_tools: configuration.allowed_tools.clone(),
                disabled_tools: self.disabled.clone(),
            },
            security: security.clone(),
            recursion_limit: configuration.recursion_limit as usize,
        })
    }
}
struct StorageRouter(Arc<ModelRouter>);
impl SessionModelRouter for StorageRouter {
    fn resolve(&self, selector: &str) -> Result<SessionModelSelection, SessionStoreError> {
        let selected = self
            .0
            .resolve(selector)
            .map_err(SessionStoreError::failure)?;
        Ok(SessionModelSelection {
            provider_id: selected.canonical_model.provider.id().into(),
            canonical_model: selected.canonical_model.selector(),
            selector: selected.requested_selector.value,
        })
    }
}
struct StorageWorkspace;
impl SessionWorkspaceResolver for StorageWorkspace {
    fn resolve(&self, path: &Path) -> Result<PathBuf, SessionStoreError> {
        ordinary_workspace_resolver()(path)
            .map(|workspace| workspace.launch_directory().to_owned())
            .map_err(SessionStoreError::failure)
    }
}
struct ProductionDiagnostics(Arc<FileDiagnosticLog>);
impl ChildDiagnostics for ProductionDiagnostics {
    fn record(&self, value: ChildDiagnostic) {
        use parrot_storage::diagnostics::{DiagnosticEvent, DiagnosticSeverity};
        let mut event = DiagnosticEvent::new(
            "agent",
            value.event,
            if value.error_code.is_some() {
                DiagnosticSeverity::Error
            } else {
                DiagnosticSeverity::Information
            },
        );
        event.agent_session_id = Some(value.session_id);
        event.correlation_id = Some(value.correlation_id);
        event.duration_milliseconds = value
            .duration_milliseconds
            .map(|value| i64::try_from(value).unwrap_or(i64::MAX));
        event.outcome = value.outcome.map(str::to_owned);
        event.error_code = value.error_code.map(str::to_owned);
        self.0.write(&event);
    }
}
impl InteractionDiagnostics for ProductionDiagnostics {
    fn write(&self, value: BrokerDiagnostic) {
        use parrot_storage::diagnostics::{DiagnosticEvent, DiagnosticSeverity};
        let severity = match value.severity {
            parrot_runtime::interactions::DiagnosticSeverity::Information => {
                DiagnosticSeverity::Information
            }
            parrot_runtime::interactions::DiagnosticSeverity::Error => DiagnosticSeverity::Error,
        };
        let mut event = DiagnosticEvent::new(value.category, value.event, severity);
        event.agent_session_id = value.agent_session_id;
        event.correlation_id = Some(value.correlation_id);
        event.duration_milliseconds = value
            .duration_milliseconds
            .map(|value| i64::try_from(value).unwrap_or(i64::MAX));
        event.outcome = value.outcome.map(str::to_owned);
        event.error_code = value.error_code.map(str::to_owned);
        self.0.write(&event);
    }
}
impl IdGenerator for ProductionDiagnostics {
    fn agent_session(&self) -> ChildResult<String> {
        Identifier::agent_session().map_err(|e| ChildError::registry(e.to_string()))
    }
    fn message(&self) -> ChildResult<String> {
        Identifier::message_id().map_err(|e| ChildError::registry(e.to_string()))
    }
}
pub struct ProductionReader(ProductionFilesystem);
impl AuthorizedReadService for ProductionReader {
    type Security = ProductionPolicy;
    fn inspect<'a>(
        &'a self,
        path: &'a Path,
        links: LinkPolicy,
        snapshot: &'a ProductionPolicy,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<FileMetadata, FileReadError>> {
        Box::pin(async move {
            self.0
                .inspect(
                    path,
                    links,
                    &SharedSecurity(Arc::new(snapshot.clone())),
                    cancellation,
                )
                .await
        })
    }
    fn list_directory<'a>(
        &'a self,
        path: &'a Path,
        links: LinkPolicy,
        snapshot: &'a ProductionPolicy,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Vec<FileMetadata>, FileReadError>> {
        Box::pin(async move {
            self.0
                .list_directory(
                    path,
                    links,
                    &SharedSecurity(Arc::new(snapshot.clone())),
                    cancellation,
                )
                .await
        })
    }
    fn read<'a>(
        &'a self,
        request: &'a ReadRequest,
        snapshot: &'a ProductionPolicy,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<BoundRead, FileReadError>> {
        Box::pin(async move {
            self.0
                .read(
                    request,
                    &SharedSecurity(Arc::new(snapshot.clone())),
                    cancellation,
                )
                .await
        })
    }

    fn stream_read<'a>(
        &'a self,
        request: &'a ReadRequest,
        snapshot: &'a ProductionPolicy,
        cancellation: &'a dyn Cancellation,
        consumer: &'a mut dyn ReadConsumer,
    ) -> BoxFuture<'a, OperationResult<std::path::PathBuf, FileReadError>> {
        Box::pin(async move {
            self.0
                .stream_read(
                    request,
                    &SharedSecurity(Arc::new(snapshot.clone())),
                    cancellation,
                    consumer,
                )
                .await
        })
    }
}

struct NativeSessionHost {
    roots: ApplicationRoots,
    workspace: parrot_domain::values::project_workspace::ProjectWorkspace,
    service: OnceLock<Weak<ParrotService>>,
    diagnostics: Arc<FileDiagnosticLog>,
}
impl SessionHost for NativeSessionHost {
    fn host(
        &self,
        session: Arc<dyn HostedSession>,
        cancellation: Arc<dyn Cancellation>,
    ) -> BoxFuture<'_, protocol::Result<Arc<dyn SessionListener>>> {
        Box::pin(async move {
            cancellation.check().map_err(|_| RpcError::cancelled())?;
            let id =
                parrot_domain::values::identifiers::UserSessionId::parse(&session.snapshot().id)
                    .map_err(internal)?;
            let selected = self
                .roots
                .discover_session(&id, true, self.workspace.launch_directory())
                .map_err(internal)?
                .ok_or_else(|| internal("selected session root is missing"))?;
            let resources = UserSessionResources::new(selected, id, self.workspace.clone())
                .map_err(internal)?;
            let service = self
                .service
                .get()
                .and_then(Weak::upgrade)
                .ok_or_else(|| internal("production service is closed"))?;
            let server = GrpcServer::start_local(
                GrpcService::new(service),
                resources.socket_path(),
                self.diagnostics.clone(),
            )
            .await
            .map_err(internal)?;
            Ok(Arc::new(NativeSessionListener(AsyncMutex::new(server)))
                as Arc<dyn SessionListener>)
        })
    }
}
struct NativeSessionListener(AsyncMutex<GrpcServer>);
impl SessionListener for NativeSessionListener {
    fn close(&self) -> BoxFuture<'_, protocol::Result<()>> {
        Box::pin(async move { self.0.lock().await.close().await.map_err(internal) })
    }
}
