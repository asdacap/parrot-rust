//! Exact post-source changes: monitor 796d57f, composite 3f4782c/d641aa5,
//! and native queue removal.
//! Pinned independent fixtures stay unchanged.

pub fn align_product_text(id: &str, text: &str) -> String {
    if id == "status.runtime" {
        const MINIJINJA_QUEUE: &str = concat!(
            "{% for queue in agent.queues %}\n",
            "{{ agent.indent }}  - queue: {{ queue.name }} ({{ queue.size }} items",
            "{% if queue.description != \"\" %}, description: {{ queue.description }}",
            "{% endif %}){% endfor %}"
        );
        const SCRIBAN_QUEUE: &str = concat!(
            "{{ for queue in agent.queues }}\n",
            "{{ agent.indent }}  - queue: {{ queue.name }} ({{ queue.size }} items",
            "{{ if queue.description != \"\" }}, description: {{ queue.description }}",
            "{{ end }}){{ end }}"
        );
        let occurrences =
            text.matches(MINIJINJA_QUEUE).count() + text.matches(SCRIBAN_QUEUE).count();
        if occurrences == 0 {
            assert!(
                text.is_empty()
                    || text.starts_with("Runtime:")
                    || text.starts_with("- AgentTask graph:"),
                "source text changed: {id}"
            );
            return if text.starts_with("Runtime:") {
                text.lines()
                    .filter(|line| !line.trim_start().starts_with("- queue: "))
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                text.to_owned()
            };
        }
        assert_eq!(occurrences, 1, "source text changed: {id}");
        return text.replace(MINIJINJA_QUEUE, "").replace(SCRIBAN_QUEUE, "");
    }
    let (original, current) = match id {
        "monitor.started" => (
            "a running monitor does not keep this turn open",
            "a running monitor keeps this turn open",
        ),
        "agent-task.composite" => (
            concat!(
                "Prepare common resource or context, then use set_agent_tasks to declare and manage ",
                "this work in your own graph. On retries or restarts, inspect your existing tasks ",
                "and reconcile the supplied definitions yourself."
            ),
            concat!(
                "Reuse supplied context and preparation results. Perform only necessary owner-level ",
                "preparation not assigned elsewhere, then use set_agent_tasks to declare and manage ",
                "the supplied provisional work in your own graph. On retries or restarts, inspect ",
                "your existing tasks and reconcile the supplied definitions yourself.\n",
                "The provided task list is provisional, not fixed; you may change it as needed to ",
                "satisfy the task’s scope and acceptance criteria."
            ),
        ),
        "context.agent-path-environment" => {
            let original = "AGENT_HISTORY_DIR contains this agent's history.jsonl.";
            assert_eq!(
                text.matches(original).count(),
                1,
                "source text changed: {id}"
            );
            return text.replace(
                original,
                concat!(
                    "AGENT_DIR contains this agent's history.jsonl.\n",
                    "Every agent's output is always available at \"$AGENT_DIR/output.txt\", ",
                    "including while it runs. A direct subagent's output is at ",
                    "\"$AGENT_DIR/<subagent-name>/output.txt\"; agent_spawn and agent_status ",
                    "report its exact path.\n",
                    "TMPDIR is the sandbox-mapped /tmp inside commands; ",
                    "PARROT_HOST_TMPDIR is the full host temp directory outside the sandbox."
                ),
            );
        }
        "context.agent-history" => ("AGENT_HISTORY_DIR", "AGENT_DIR"),
        _ => return text.to_owned(),
    };
    assert_eq!(
        text.matches(original).count(),
        1,
        "source text changed: {id}"
    );
    text.replace(original, current)
}
