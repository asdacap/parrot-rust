//! Exact post-source changes: monitor 796d57f and composite 3f4782c/d641aa5.
//! Pinned independent fixtures stay unchanged.

pub fn align_product_text(id: &str, text: &str) -> String {
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
        _ => return text.to_owned(),
    };
    assert_eq!(
        text.matches(original).count(),
        1,
        "source text changed: {id}"
    );
    text.replace(original, current)
}
