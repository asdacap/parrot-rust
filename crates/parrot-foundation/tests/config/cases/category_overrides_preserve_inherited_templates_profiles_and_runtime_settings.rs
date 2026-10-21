use crate::support::Context;
use parrot_foundation::config::{ConfigError, Configuration};
#[test]
fn source_assertions() {
    let baseline_context = Context::new();
    let baseline = baseline_context.load().unwrap();
    assert!(!baseline_context.path.exists());
    let context = Context::new();
    context.write("prompt_templates:\n  agent-task.stop-canceled:\n    template: Task canceled by owner.\n  agent-session.child-completion:\n    template: '{agent_name}: {result}'\nprofiles:\n  build:\n    usage: Custom foreground usage\nlive_buffer_rows: 37\nagent_tasks:\n  maximum_attempts: 7\n");
    let mut known = baseline.prompt_templates.clone();
    known.get_mut("agent-task.stop-canceled").unwrap().text = "Task canceled by owner.".into();
    known
        .get_mut("agent-session.child-completion")
        .unwrap()
        .text = "{agent_name}: {result}".into();
    let configuration = Configuration::load(
        &context.path,
        &context.predefined,
        &context.environment,
        |spec| {
            let id = spec
                .path
                .strip_prefix("prompt_templates.")
                .unwrap_or(&spec.path);
            if known.get(id) == Some(spec) {
                Ok(())
            } else {
                Err(ConfigError::invalid_data(format!(
                    "descriptor not source-validated: {}",
                    spec.path
                )))
            }
        },
    )
    .unwrap();
    assert_eq!(configuration.system_prompts, baseline.system_prompts);
    assert_eq!(
        configuration.runtime.profiles["build"].read_only,
        baseline.runtime.profiles["build"].read_only
    );
    assert_eq!(
        configuration.runtime.profiles["build"].usage,
        "Custom foreground usage"
    );
    assert_eq!(configuration.runtime.live_buffer_rows, 37);
    assert_eq!(configuration.runtime.agent_tasks.maximum_attempts, 7);
    assert_eq!(
        configuration.runtime.tools.len(),
        baseline.runtime.tools.len()
    );
}
