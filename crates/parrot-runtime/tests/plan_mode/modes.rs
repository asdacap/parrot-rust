use super::support::*;
use parrot_foundation::paths::AgentScratchDirectory;
use parrot_runtime::plan_mode::*;
use std::sync::{Arc, Mutex};

pub fn registry(raw: Arc<Policy>) -> Arc<ModeRegistry<Policy>> {
    Arc::new(
        ModeRegistry::new(
            profiles()
                .into_iter()
                .map(|(id, config)| {
                    MaterializedModeProfile::new(id, &config, &["disabled".into()], raw.clone())
                        .unwrap()
                })
                .collect(),
            COORDINATOR,
        )
        .unwrap(),
    )
}
pub struct Materializer {
    pub raw: Arc<Policy>,
    pub output: Arc<Policy>,
    pub scratch: AgentScratchDirectory,
    pub calls: Mutex<usize>,
    pub failure: Option<ModeError>,
}
impl RetainedModePolicyMaterializer<Policy> for Materializer {
    fn for_agent(
        &self,
        raw: &Arc<Policy>,
        scratch: &AgentScratchDirectory,
    ) -> ModeResult<Arc<Policy>> {
        *self.calls.lock().unwrap() += 1;
        assert!(Arc::ptr_eq(raw, &self.raw));
        assert_eq!(scratch.root(), self.scratch.root());
        assert_ne!(scratch.root(), scratch.plan_directory());
        self.output.trace.lock().unwrap().push("materialize".into());
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        Ok(self.output.clone())
    }
}

#[test]
fn registry_defaults_flags_case_sensitivity_and_ordinal_order_are_effective_configuration() {
    let profiles = profiles();
    let registry = registry(policy("raw"));
    assert_eq!(registry.default(), COORDINATOR);
    assert_eq!(registry.list(), [BUILD, COORDINATOR, PLAN, QUERY]);
    assert_eq!(registry.resolve("").unwrap().id(), COORDINATOR);
    for unknown in ["missing", "Plan", "worker", " "] {
        assert!(registry.resolve(unknown).is_err());
    }
    for id in registry.list() {
        let mode = registry.resolve(id).unwrap();
        assert_eq!(mode.configuration(), &profiles[id]);
        assert_eq!(mode.disabled_tools(), ["disabled"]);
        assert!(!mode.is_tool_permitted("disabled"));
    }
    for (id, read_only, maximum) in [
        (COORDINATOR, false, 1024),
        (BUILD, false, 1024),
        (PLAN, true, 1024),
        (QUERY, true, 8),
    ] {
        let selected = registry.resolve(id).unwrap();
        assert_eq!(selected.configuration().read_only, read_only);
        assert_eq!(selected.configuration().max_turns, maximum);
    }
    let mut config = profiles[PLAN].clone();
    config.is_user_selectable = true;
    let ids = ["\u{e000}", "\u{10000}", "a", "A"];
    let special = ModeRegistry::new(
        ids.iter()
            .map(|id| {
                MaterializedModeProfile::new((*id).into(), &config, &[], policy("ordinal")).unwrap()
            })
            .collect(),
        "A",
    )
    .unwrap();
    assert_eq!(special.list(), ["A", "a", "\u{10000}", "\u{e000}"]);
    config.allowed_tools = Some(vec!["allowed".into(), "disabled".into()]);
    let profile = MaterializedModeProfile::new(
        "custom".into(),
        &config,
        &["disabled".into()],
        policy("tools"),
    )
    .unwrap();
    assert!(profile.is_tool_permitted("allowed"));
    for tool in ["Allowed", "disabled", "other"] {
        assert!(!profile.is_tool_permitted(tool));
    }
}

#[test]
fn lazy_mode_profile_retains_raw_policy_and_all_metadata_across_prepare_complete_and_reattach() {
    let fixture = Fixture::new();
    let raw = policy("raw");
    let config = profiles()[PLAN].clone();
    let parser = Arc::new(Parser::new());
    let factory = Arc::new(PlanModeWorkflowFactory::new(templates(), parser));
    let modes = UserSessionModes::new(registry(raw.clone()), vec![(PLAN.into(), factory)]).unwrap();
    let mode = modes.resolve(PLAN).unwrap();
    assert!(mode.profile().prompt().is_err());
    assert!(mode.prepare().is_err());
    assert!(mode.complete().is_err());
    modes.attach(fixture.scratch("main"));
    let profile = mode.profile();
    assert_eq!(profile.id(), PLAN);
    assert_eq!(profile.usage(), config.usage);
    assert_eq!(profile.recursion_limit(), config.recursion_limit);
    assert_eq!(profile.allowed_tools(), config.allowed_tools.as_deref());
    assert_eq!(profile.disabled_tools(), ["disabled"]);
    assert_eq!(profile.max_turns(), config.max_turns);
    assert_eq!(profile.is_user_selectable(), config.is_user_selectable);
    assert_eq!(profile.is_agent_selectable(), config.is_agent_selectable);
    assert!(Arc::ptr_eq(profile.security(), &raw));
    let before = profile.prompt().unwrap();
    // Consume T44's actual composite/epoch contract with the selected mode's
    // lazy text; this local adapter is not the production host composition.
    use parrot_runtime::prompt_context::{CompositeSystemPrompt, SystemPrompt};
    struct ModePrompt(SessionMode<Policy>);
    impl SystemPrompt for ModePrompt {
        fn renew_epoch(&mut self) {}
        fn build(
            &mut self,
            _: &parrot_provider::routing::aliases::ResolvedModelSelection,
        ) -> parrot_runtime::prompt_context::PromptResult<String> {
            self.0.profile().prompt().map_err(|error| {
                parrot_runtime::prompt_context::PromptError::Context(error.to_string())
            })
        }
    }
    let mut composed = CompositeSystemPrompt::new(vec![Box::new(ModePrompt(mode.clone()))]);
    assert_eq!(composed.build(&model()).unwrap(), before);
    composed.renew_epoch();
    mode.prepare().unwrap();
    let after = profile.prompt().unwrap();
    assert_ne!(before, after);
    assert_eq!(composed.build(&model()).unwrap(), after);
    composed.renew_epoch();
    assert_eq!(composed.build(&model()).unwrap(), after);
    assert!(after.contains(fixture.scratch("main").plan_directory().to_str().unwrap()));
    assert!(after.contains(".tasks.json"));
    assert!(matches!(
        mode.complete().unwrap(),
        ModeCompletionOutcome::Repair(_)
    ));
    assert!(Arc::ptr_eq(profile.security(), &raw));
    mode.prepare().unwrap();
    assert_eq!(profile.prompt().unwrap(), after);
    modes.attach(fixture.scratch("main"));
    assert_eq!(mode.complete().unwrap(), ModeCompletionOutcome::None);
    mode.prepare().unwrap();
    assert_ne!(profile.prompt().unwrap(), after);
    let build = modes.resolve(BUILD).unwrap();
    assert_eq!(build.profile().prompt().unwrap(), profiles()[BUILD].prompt);
    build.prepare().unwrap();
    assert_eq!(build.complete().unwrap(), ModeCompletionOutcome::None);
    assert!(Arc::ptr_eq(build.profile().security(), &raw));
}

#[test]
fn workflow_port_preserves_exact_lazy_prompt_reads_prepare_and_repair_lifecycle() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    struct Workflow {
        reads: AtomicUsize,
        prepared: AtomicBool,
    }
    impl ModeWorkflow<Policy> for Workflow {
        fn render_prompt(&self, _: &MaterializedModeProfile<Policy>) -> ModeResult<String> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            Ok(if self.prepared.load(Ordering::SeqCst) {
                "prepared"
            } else {
                "initial"
            }
            .into())
        }
        fn prepare(&self) -> ModeResult<()> {
            self.prepared.store(true, Ordering::SeqCst);
            Ok(())
        }
        fn complete(&self) -> ModeResult<ModeCompletionOutcome> {
            Ok(ModeCompletionOutcome::Repair("repair".into()))
        }
    }
    struct Factory(Arc<Workflow>);
    impl ModeWorkflowFactory<Policy> for Factory {
        fn create(&self, _: &AgentScratchDirectory) -> Arc<dyn ModeWorkflow<Policy>> {
            self.0.clone()
        }
    }
    let fixture = Fixture::new();
    let workflow = Arc::new(Workflow {
        reads: AtomicUsize::new(0),
        prepared: AtomicBool::new(false),
    });
    let raw = policy("raw");
    let modes = UserSessionModes::new(
        registry(raw.clone()),
        vec![(BUILD.into(), Arc::new(Factory(workflow.clone())))],
    )
    .unwrap();
    modes.attach(fixture.scratch("main"));
    let mode = modes.resolve(BUILD).unwrap();
    let profile = mode.profile();
    assert_eq!(workflow.reads.load(Ordering::SeqCst), 0);
    assert_eq!(profile.id(), BUILD);
    assert!(Arc::ptr_eq(profile.security(), &raw));
    assert_eq!(profile.prompt().unwrap(), "initial");
    mode.prepare().unwrap();
    assert!(workflow.prepared.load(Ordering::SeqCst));
    assert_eq!(profile.prompt().unwrap(), "prepared");
    assert_eq!(mode.profile().prompt().unwrap(), "prepared");
    assert_eq!(workflow.reads.load(Ordering::SeqCst), 3);
    assert_eq!(
        mode.complete().unwrap(),
        ModeCompletionOutcome::Repair("repair".into())
    );
}

#[test]
fn independent_user_flags_and_copied_configuration_do_not_change_the_other_partition_metadata() {
    let raw = policy("raw");
    let mut configs = profiles();
    configs.get_mut(BUILD).unwrap().is_agent_selectable = true;
    configs.get_mut("worker").unwrap().is_user_selectable = true;
    configs.get_mut("review").unwrap().is_user_selectable = false;
    configs.get_mut("review").unwrap().is_agent_selectable = false;
    let mut disabled = vec!["disabled".into()];
    let registry = ModeRegistry::new(
        configs
            .iter()
            .map(|(id, config)| {
                MaterializedModeProfile::new(id.clone(), config, &disabled, raw.clone()).unwrap()
            })
            .collect(),
        BUILD,
    )
    .unwrap();
    disabled.clear();
    configs.get_mut(BUILD).unwrap().allowed_tools = Some(vec!["later mutation".into()]);
    configs.get_mut(BUILD).unwrap().prompt = "later mutation".into();
    assert_eq!(registry.list(), [BUILD, COORDINATOR, PLAN, QUERY, "worker"]);
    assert!(registry.resolve("review").is_err());
    assert_eq!(registry.resolve("worker").unwrap().id(), "worker");
    let build = registry.resolve(BUILD).unwrap();
    assert!(build.configuration().is_agent_selectable);
    assert!(build.configuration().is_user_selectable);
    assert_ne!(build.configuration().prompt, "later mutation");
    assert_ne!(
        build.configuration().allowed_tools,
        configs[BUILD].allowed_tools
    );
    assert_eq!(build.disabled_tools(), ["disabled"]);
    assert!(Arc::ptr_eq(build.security(), &raw));
    // ResolveChild/explore alias remain the child-profile owner, not ModeRegistry.
}

#[test]
fn capture_validates_before_single_materialization_propagates_error_and_forwards_exact_output_arc()
{
    let fixture = Fixture::new();
    let raw = policy("raw");
    let output = policy("materialized");
    let modes = UserSessionModes::new(registry(raw.clone()), vec![]).unwrap();
    let mut materializer = Materializer {
        raw: raw.clone(),
        output: output.clone(),
        scratch: fixture.scratch("actual-stored-agent"),
        calls: Mutex::new(0),
        failure: None,
    };
    assert!(
        modes
            .capture_turn_selection("worker", model(), &materializer)
            .is_err()
    );
    assert!(
        modes
            .capture_turn_selection(PLAN, model(), &materializer)
            .is_err()
    );
    assert_eq!(*materializer.calls.lock().unwrap(), 0);
    modes.attach(materializer.scratch.clone());
    materializer.failure = Some(ModeError::Template("exact materialization error".into()));
    assert!(
        matches!(modes.capture_turn_selection(PLAN, model(), &materializer), Err(ref error) if Some(error) == materializer.failure.as_ref())
    );
    assert_eq!(*materializer.calls.lock().unwrap(), 1);
    materializer.failure = None;
    let selection = modes
        .capture_turn_selection(PLAN, model(), &materializer)
        .unwrap();
    assert_eq!(*materializer.calls.lock().unwrap(), 2);
    assert!(Arc::ptr_eq(&selection.security, &output));
    assert!(!Arc::ptr_eq(&selection.security, &raw));
    assert!(Arc::ptr_eq(
        modes.resolve(PLAN).unwrap().profile().security(),
        &raw
    ));
    assert_eq!(selection.profile.id, PLAN);
    assert_eq!(
        selection.profile.maximum_requests,
        profiles()[PLAN].max_turns
    );
    assert_eq!(
        selection.profile.allowed_tools,
        profiles()[PLAN].allowed_tools
    );
    assert_eq!(selection.profile.disabled_tools, ["disabled"]);
    assert_eq!(
        selection.model.canonical_model.selector(),
        "fixture/vendor/model"
    );
}
