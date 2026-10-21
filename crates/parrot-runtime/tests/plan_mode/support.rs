use parrot_domain::{
    contracts::{
        agent_tasks::TaskArtifactParser,
        paths::{ResolvedRead, RootOrigin, SelectedSessionRoot},
        security::AccessSnapshot,
    },
    values::{
        agent_tasks::{ExecutionStatus, LeafResponse, Task, TaskArtifact, TaskPayload},
        identifiers::UserSessionId,
    },
};
use parrot_foundation::{
    config::runtime::ProfileConfig,
    paths::{AgentScratchDirectory, UserSessionResources},
    template_catalog::PromptTemplateCatalog,
};
use parrot_runtime::plan_mode::*;
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    fmt, io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

static NEXT: AtomicU64 = AtomicU64::new(0);
pub struct Fixture {
    pub root: PathBuf,
    pub resources: UserSessionResources<PathBuf>,
}
impl Fixture {
    pub fn new() -> Self {
        Self::with_origin(RootOrigin::Legacy)
    }
    pub fn with_origin(origin: RootOrigin) -> Self {
        let root = std::env::temp_dir().join(format!(
            "parrot-t53-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join("workspace")).unwrap();
        let selected = SelectedSessionRoot::new(root.join("selected/session"), origin).unwrap();
        let resources = UserSessionResources::new(
            selected,
            UserSessionId::parse("session").unwrap(),
            root.join("workspace"),
        )
        .unwrap();
        Self { root, resources }
    }
    pub fn scratch(&self, name: &str) -> AgentScratchDirectory {
        self.resources.agent_scratch(&[name]).unwrap()
    }
    pub fn workflow(&self) -> (PlanModeWorkflow<Parser>, Arc<Parser>) {
        let parser = Arc::new(Parser::new());
        let workflow = PlanModeWorkflow::new(
            Arc::new(PlanArtifactStore::new(self.scratch("main"))),
            templates(),
            parser.clone(),
        );
        (workflow, parser)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

pub fn templates() -> Arc<PromptTemplateCatalog> {
    Arc::new(
        PromptTemplateCatalog::from_content(
            "",
            &BTreeMap::from([("HOME".into(), "/synthetic-t53-home".into())]),
        )
        .unwrap()
        .1,
    )
}
pub fn profiles() -> BTreeMap<String, ProfileConfig> {
    PromptTemplateCatalog::from_content(
        "",
        &BTreeMap::from([("HOME".into(), "/synthetic-t53-home".into())]),
    )
    .unwrap()
    .0
    .runtime
    .profiles
}

pub struct Tokens(pub Mutex<VecDeque<[u8; 16]>>);
impl Tokens {
    pub fn fixed(value: u8) -> Arc<Self> {
        Arc::new(Self(Mutex::new(VecDeque::from([[value; 16]]))))
    }
}
impl PlanTokenSource for Tokens {
    fn next_token(&self) -> io::Result<[u8; 16]> {
        self.0
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| io::Error::other("token fixture exhausted"))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParseFailure(pub String);
impl fmt::Display for ParseFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ParseFailure {}

/// Scripted domain port, not another JSON parser or T49 validation algorithm.
pub struct Parser {
    pub inputs: Mutex<Vec<String>>,
    answers: Mutex<HashMap<String, Result<TaskArtifact, ParseFailure>>>,
}
impl Parser {
    pub fn new() -> Self {
        Self {
            inputs: Mutex::new(Vec::new()),
            answers: Mutex::new(HashMap::new()),
        }
    }
    pub fn answer(&self, input: &str, answer: Result<TaskArtifact, ParseFailure>) {
        self.answers.lock().unwrap().insert(input.into(), answer);
    }
}
impl TaskArtifactParser for Parser {
    type Error = ParseFailure;
    fn parse_artifact(&self, input: &str) -> Result<TaskArtifact, Self::Error> {
        self.inputs.lock().unwrap().push(input.into());
        self.answers
            .lock()
            .unwrap()
            .get(input)
            .cloned()
            .unwrap_or_else(|| panic!("unexpected parser input: {input}"))
    }
    fn parse_task_set(&self, _: &str) -> Result<Vec<Task>, Self::Error> {
        panic!("plan uses artifact parser only")
    }
    fn validate_merged_graph(&self, _: &[Task], _: &str) -> Result<(), Self::Error> {
        panic!("plan must not update a graph")
    }
    fn parse_leaf_response(&self, _: &str) -> Result<LeafResponse, Self::Error> {
        panic!("plan must not execute a leaf")
    }
}

pub fn task(name: &str, payload: TaskPayload) -> Task {
    Task {
        name: name.into(),
        dependencies: vec![],
        description: name.into(),
        payload,
        acceptance_criteria: "Pass".into(),
        model: None,
        hidden: false,
        state: ExecutionStatus::Pending,
        result: None,
        failure: None,
    }
}
pub const VALID: &str = "{\"schema_version\":1,\"tasks\":[{\"name\":\"implement\",\"description\":\"Implement the plan\",\"payload\":\"Implement it\",\"acceptance_criteria\":\"Tests pass\"}]}";
pub fn valid_artifact() -> TaskArtifact {
    TaskArtifact {
        schema_version: 1,
        tasks: vec![task(
            "implement",
            TaskPayload::Instruction("Implement it".into()),
        )],
    }
}
pub fn write_valid(workflow: &PlanModeWorkflow<Parser>, parser: &Parser) {
    let paths = workflow.artifacts().paths().unwrap();
    std::fs::write(paths.plan(), "# Plan").unwrap();
    std::fs::write(paths.tasks(), VALID).unwrap();
    parser.answer(VALID, Ok(valid_artifact()));
}

/// Test policy only: it records checks and rejects anything not explicitly in
/// this immutable fixture. This does not implement P05 policy/materialization.
#[derive(Debug)]
pub struct Policy {
    pub name: String,
    pub writes: Vec<PathBuf>,
    pub trace: Arc<Mutex<Vec<String>>>,
}
impl AccessSnapshot for Policy {
    type Error = String;
    fn check_read(&self, _: &ResolvedRead) -> Result<(), Self::Error> {
        Ok(())
    }
    fn check_write(&self, target: &Path) -> Result<(), Self::Error> {
        self.trace
            .lock()
            .unwrap()
            .push(format!("check:{}", target.display()));
        if self
            .writes
            .iter()
            .any(|allowed| target.starts_with(allowed))
        {
            Ok(())
        } else {
            Err(format!("{} denied {}", self.name, target.display()))
        }
    }
}
pub fn model() -> parrot_provider::routing::aliases::ResolvedModelSelection {
    use parrot_provider::{
        catalogs::Model,
        routing::{
            aliases::{ModelAliasCatalog, ModelRouter, ModelRouting},
            registry::ProviderRegistry,
        },
    };
    let registry = Arc::new(
        ProviderRegistry::new(
            vec![Arc::new(UnusedProvider)],
            BTreeMap::from([(
                "fixture".into(),
                vec![Model::new("vendor/model", "fixture")],
            )]),
        )
        .unwrap(),
    );
    let aliases = Arc::new(ModelAliasCatalog::new(registry.clone(), vec![]).unwrap());
    let routing = Arc::new(ModelRouting::new(aliases, "fixture/vendor/model"));
    ModelRouter::new(registry, routing).resolve("").unwrap()
}
struct UnusedProvider;
impl parrot_provider::sessions::Provider for UnusedProvider {
    fn id(&self) -> &str {
        "fixture"
    }
    fn call(
        &self,
        _: parrot_provider::sessions::Request,
    ) -> Result<Box<dyn parrot_provider::sessions::EventStream>, parrot_provider::sessions::Error>
    {
        panic!("no provider calls")
    }
    fn has_credential<'a>(
        &'a self,
        _: &'a dyn parrot_domain::contracts::async_ops::Cancellation,
    ) -> parrot_domain::contracts::async_ops::BoxFuture<
        'a,
        Result<bool, parrot_provider::sessions::Error>,
    > {
        panic!("no credentials")
    }
    fn seed_models(&self) -> Vec<parrot_provider::catalogs::Model> {
        vec![]
    }
    fn list_models<'a>(
        &'a self,
        _: &'a dyn parrot_domain::contracts::async_ops::Cancellation,
    ) -> parrot_domain::contracts::async_ops::BoxFuture<
        'a,
        Result<Vec<parrot_provider::catalogs::Model>, parrot_provider::sessions::Error>,
    > {
        panic!("no catalog fetch")
    }
    fn calculate_image_tokens(
        &self,
        _: &parrot_provider::catalogs::Model,
        _: &parrot_provider::sessions::Content,
    ) -> i64 {
        panic!("no image")
    }
    fn open_session(
        &self,
    ) -> Result<Arc<dyn parrot_provider::sessions::Session>, parrot_provider::sessions::Error> {
        panic!("no provider session")
    }
}

pub fn policy(name: &str) -> Arc<Policy> {
    Arc::new(Policy {
        name: name.into(),
        writes: vec![],
        trace: Arc::new(Mutex::new(Vec::new())),
    })
}
