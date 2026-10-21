use parrot_foundation::config::{
    node::{Kind, Node},
    yaml,
};

fn at<'a>(mut node: &'a Node, path: &[&str]) -> &'a Node {
    for key in path {
        node = node.get(key).unwrap();
    }
    node
}
fn count(node: &Node) -> usize {
    match &node.kind {
        Kind::Mapping(entries) => entries.len(),
        _ => panic!("expected mapping"),
    }
}

#[test]
fn source_assertions() {
    let sources = [
        (
            "prompts",
            Some(
                "system_prompts:\n  base: |-\n    First {literal}\n    Second line\nprompt_templates:\n  ordinary:\n    template: >-\n      Hello\n      {name}\n    allowed_arguments: [name]\n",
            ),
        ),
        (
            "tasks",
            Some(
                "system_prompts:\n  agent-tasks: Task guidance\ntools:\n  task-tool:\n    description: Task tool\nagent_tasks:\n  maximum_attempts: 2\nprompt_templates:\n  agent-task.custom:\n    template: '{result}'\n",
            ),
        ),
        (
            "runtime",
            Some(
                "empty: {}\ntools:\n  sample:\n    parameters:\n      type: object\n      properties:\n        items:\n          type: array\n          items:\n            type: string\n      required: [items]\nordered: [third, first, second]\n",
            ),
        ),
    ];
    let content = yaml::compose(&sources).unwrap();
    assert_eq!(content, yaml::compose(&sources).unwrap());
    let documents = yaml::parse_documents(&content).unwrap();
    assert_eq!(documents.len(), 1);
    let root = &documents[0];
    assert_eq!(count(root), 6);
    assert_eq!(
        at(root, &["system_prompts", "base"]).as_str(),
        Some("First {literal}\nSecond line")
    );
    assert_eq!(
        at(root, &["system_prompts", "agent-tasks"]).as_str(),
        Some("Task guidance")
    );
    assert_eq!(
        at(root, &["tools", "task-tool", "description"]).as_str(),
        Some("Task tool")
    );
    assert_eq!(count(at(root, &["prompt_templates"])), 2);
    assert_eq!(
        at(root, &["prompt_templates", "ordinary", "template"]).as_str(),
        Some("Hello {name}")
    );
    assert_eq!(
        at(root, &["prompt_templates", "agent-task.custom", "template"]).as_str(),
        Some("{result}")
    );
    assert_eq!(count(at(root, &["empty"])), 0);
    assert_eq!(
        at(
            root,
            &[
                "tools",
                "sample",
                "parameters",
                "properties",
                "items",
                "items",
                "type"
            ]
        )
        .as_str(),
        Some("string")
    );
    let Kind::Sequence(ordered) = &at(root, &["ordered"]).kind else {
        panic!("expected sequence")
    };
    assert_eq!(
        ordered
            .iter()
            .map(|node| node.as_str().unwrap())
            .collect::<Vec<_>>(),
        ["third", "first", "second"]
    );
}
