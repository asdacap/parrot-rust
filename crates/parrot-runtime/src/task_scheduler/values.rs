use parrot_domain::values::agent_tasks::{ExecutionStatus, Task, TaskPayload};

pub fn status_name(status: ExecutionStatus) -> &'static str {
    match status {
        ExecutionStatus::Pending => "pending",
        ExecutionStatus::Running => "running",
        ExecutionStatus::Succeeded => "succeeded",
        ExecutionStatus::Failed => "failed",
        ExecutionStatus::Canceled => "canceled",
    }
}

pub fn has_same_definition(left: &Task, right: &Task) -> bool {
    left.name == right.name
        && left.dependencies == right.dependencies
        && left.description == right.description
        && same_payload_definition(&left.payload, &right.payload)
        && left.acceptance_criteria == right.acceptance_criteria
        && left.model == right.model
}
fn same_payload_definition(left: &TaskPayload, right: &TaskPayload) -> bool {
    match (left, right) {
        (TaskPayload::Instruction(left), TaskPayload::Instruction(right)) => left == right,
        (TaskPayload::Tasks(left), TaskPayload::Tasks(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| has_same_definition(left, right))
        }
        _ => false,
    }
}
pub fn has_same_visibility(left: &Task, right: &Task) -> bool {
    left.hidden == right.hidden
        && match (&left.payload, &right.payload) {
            (TaskPayload::Instruction(_), TaskPayload::Instruction(_)) => true,
            (TaskPayload::Tasks(left), TaskPayload::Tasks(right)) => {
                left.len() == right.len()
                    && left
                        .iter()
                        .zip(right)
                        .all(|(left, right)| has_same_visibility(left, right))
            }
            _ => false,
        }
}
pub fn apply_visibility_changes(current: &Task, previous: &Task, incoming: &Task) -> Task {
    let mut updated = current.clone();
    if previous.hidden != incoming.hidden {
        updated.hidden = incoming.hidden;
    }
    if let (
        TaskPayload::Tasks(current),
        TaskPayload::Tasks(previous),
        TaskPayload::Tasks(incoming),
    ) = (&current.payload, &previous.payload, &incoming.payload)
    {
        updated.payload = TaskPayload::Tasks(
            current
                .iter()
                .map(|task| {
                    previous
                        .iter()
                        .zip(incoming)
                        .find(|(_, after)| after.name == task.name)
                        .filter(|(_, after)| has_same_definition(task, after))
                        .map_or_else(
                            || task.clone(),
                            |(before, after)| apply_visibility_changes(task, before, after),
                        )
                })
                .collect(),
        );
    }
    updated
}

/// The source serializes an ordered object with Utf8JsonWriter's default encoder.
/// This is only task prompt serialization; parsing belongs to the injected parser.
pub(crate) fn serialize_tasks(tasks: &[Task]) -> String {
    let mut output = String::from("[");
    for (index, task) in tasks.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        output.push_str("{\"name\":");
        quote(&mut output, &task.name);
        output.push_str(",\"dependencies\":[");
        for (index, dependency) in task.dependencies.iter().enumerate() {
            if index > 0 {
                output.push(',');
            }
            quote(&mut output, dependency);
        }
        output.push_str("],\"description\":");
        quote(&mut output, &task.description);
        output.push_str(",\"payload\":");
        match &task.payload {
            TaskPayload::Instruction(instruction) => quote(&mut output, instruction),
            TaskPayload::Tasks(tasks) => output.push_str(&serialize_tasks(tasks)),
        }
        output.push_str(",\"acceptance_criteria\":");
        quote(&mut output, &task.acceptance_criteria);
        if let Some(model) = &task.model {
            output.push_str(",\"model\":");
            quote(&mut output, model);
        }
        output.push_str(if task.hidden {
            ",\"hidden\":true"
        } else {
            ",\"hidden\":false"
        });
        output.push_str(",\"state\":");
        quote(&mut output, status_name(task.state));
        if let Some(result) = &task.result {
            output.push_str(",\"result\":");
            quote(&mut output, result);
        }
        if let Some(failure) = &task.failure {
            output.push_str(",\"failure\":");
            quote(&mut output, failure);
        }
        output.push('}');
    }
    output.push(']');
    output
}
fn quote(output: &mut String, text: &str) {
    use std::fmt::Write;
    output.push('"');
    for unit in text.encode_utf16() {
        match unit {
            8 => output.push_str("\\b"),
            9 => output.push_str("\\t"),
            10 => output.push_str("\\n"),
            12 => output.push_str("\\f"),
            13 => output.push_str("\\r"),
            92 => output.push_str("\\\\"),
            // JavaScriptEncoder.Default escapes HTML-sensitive ASCII and all
            // non-Basic-Latin UTF-16 units, including surrogate pairs.
            0x20..=0x7e if ![0x22, 0x26, 0x27, 0x2b, 0x3c, 0x3e, 0x60].contains(&unit) => {
                output.push(char::from_u32(u32::from(unit)).expect("ASCII"))
            }
            _ => {
                write!(output, "\\u{unit:04X}").expect("writing to String");
            }
        }
    }
    output.push('"');
}
