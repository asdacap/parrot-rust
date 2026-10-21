use std::collections::BTreeMap;

use parrot_domain::values::tools::{
    ConfiguredToolDefinition, ImageBudgetError, ToolCycleImageBudget, ToolDefinitionCatalog,
};

#[test]
fn catalog_preserves_the_captured_read_schema_as_opaque_json() {
    let parameters = r#"{"type":"object","properties":{"path":{"type":"integer","description":"Workspace-relative or authorized absolute file or directory path.","minimum":7},"offset":{"type":"integer","minimum":1,"description":"One-based first line to read."},"limit":{"type":"integer","minimum":1,"description":"Maximum number of lines to read."}},"required":["path"],"additionalProperties":false}"#;
    let definitions = BTreeMap::from([(
        "read".to_owned(),
        ConfiguredToolDefinition::new("Read synthetic content.", parameters),
    )]);
    let catalog = ToolDefinitionCatalog::new(&definitions);
    let definition = catalog.describe("read").unwrap();
    assert_eq!(definition.description, "Read synthetic content.");
    assert_eq!(definition.parameters_json.as_bytes(), parameters.as_bytes());
    assert_eq!(catalog.definitions(), &definitions);
}

#[test]
fn catalog_copies_input_and_uses_exact_case_sensitive_names() {
    let parameters = r#"{"type":"object","properties":{"description":{"type":"string","description":"Semantic value."}},"required":["description"],"additionalProperties":false}"#;
    let mut definitions = BTreeMap::from([(
        "work".to_owned(),
        ConfiguredToolDefinition::new("Does work.", parameters),
    )]);
    let catalog = ToolDefinitionCatalog::new(&definitions);
    definitions.get_mut("work").unwrap().description = "Changed".to_owned();
    definitions.clear();
    assert_eq!(catalog.describe("work").unwrap().description, "Does work.");
    assert_eq!(
        catalog.describe("work").unwrap().parameters_json,
        parameters
    );
    for name in ["missing", "WORK", "Work", "", "work "] {
        assert_eq!(
            catalog.describe(name).unwrap_err().to_string(),
            format!("tools.{name} is not defined")
        );
    }
}

#[test]
fn schema_null_absence_order_and_spelling_are_not_normalized() {
    for parameters in [
        r#"{}"#,
        r#"{"default":null}"#,
        r#"{ "Properties": { "z": null, "a": {} }, "description": "\\u0041" }"#,
    ] {
        let definitions = BTreeMap::from([(
            "opaque".to_owned(),
            ConfiguredToolDefinition::new("Opaque.", parameters),
        )]);
        let catalog = ToolDefinitionCatalog::new(&definitions);
        assert_eq!(
            catalog.describe("opaque").unwrap().parameters_json,
            parameters
        );
    }
}

#[test]
fn restored_usage_and_exceeded_state_are_independent_and_sticky() {
    for restore_exceeded in [false, true] {
        let budget = ToolCycleImageBudget::new(10);
        budget.restore_accepted(6).unwrap();
        if restore_exceeded {
            budget.restore_exceeded();
        }
        assert_eq!(budget.try_accept(4), Ok(!restore_exceeded));
        assert_eq!(budget.try_accept(5), Ok(false));
        assert_eq!(budget.try_accept(1), Ok(false));
        assert_eq!(budget.try_accept(0), Ok(false));
        assert!(budget.is_exceeded());
        let independent = ToolCycleImageBudget::new(10);
        assert_eq!(independent.try_accept(10), Ok(true));
        assert!(!independent.is_exceeded());
    }
}

#[test]
fn zero_exact_limit_over_limit_and_negative_input() {
    let zero = ToolCycleImageBudget::new(0);
    assert_eq!(zero.try_accept(0), Ok(true));
    assert_eq!(zero.try_accept(1), Ok(false));
    assert_eq!(zero.try_accept(0), Ok(false));
    let budget = ToolCycleImageBudget::new(10);
    assert_eq!(
        budget.try_accept(-1),
        Err(ImageBudgetError::NegativeByteLength)
    );
    assert_eq!(
        budget.restore_accepted(-1),
        Err(ImageBudgetError::NegativeByteLength)
    );
    assert!(!budget.is_exceeded());
    assert_eq!(budget.try_accept(10), Ok(true));
    assert_eq!(budget.try_accept(0), Ok(true));
    assert_eq!(budget.try_accept(1), Ok(false));
    assert_eq!(
        budget.try_accept(-1),
        Err(ImageBudgetError::NegativeByteLength)
    );
    assert!(budget.is_exceeded());
    assert_eq!(ToolCycleImageBudget::new(-1).try_accept(0), Ok(false));
}

#[test]
fn restore_can_exceed_limit_but_checked_overflow_does_not_change_state() {
    let budget = ToolCycleImageBudget::new(10);
    budget.restore_accepted(i64::MAX).unwrap();
    assert!(!budget.is_exceeded());
    assert_eq!(
        budget.restore_accepted(1),
        Err(ImageBudgetError::AcceptedBytesOverflow)
    );
    assert_eq!(budget.restore_accepted(0), Ok(()));
    assert_eq!(budget.try_accept(0), Ok(false));
    assert!(budget.is_exceeded());
    let budget = ToolCycleImageBudget::new(10);
    assert_eq!(budget.try_accept(i64::MAX), Ok(false));
}

#[test]
fn negative_maximum_preserves_source_unchecked_remaining_arithmetic() {
    let budget = ToolCycleImageBudget::new(i32::MIN);
    budget.restore_accepted(i64::MAX).unwrap();
    assert_eq!(budget.try_accept(1), Ok(true));
    assert!(!budget.is_exceeded());
}

#[test]
fn shared_parallel_acceptance_is_atomic() {
    let budget = ToolCycleImageBudget::new(10);
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..20)
            .map(|_| scope.spawn(|| budget.try_accept(1).unwrap()))
            .collect();
        let accepted = handles
            .into_iter()
            .map(|handle| usize::from(handle.join().unwrap()))
            .sum::<usize>();
        assert_eq!(accepted, 10);
    });
    assert!(budget.is_exceeded());
}

#[test]
fn failure_uses_the_exact_prompt_key_and_argument_without_translation() {
    for limit in [0, 10, -1, i32::MAX] {
        let budget = ToolCycleImageBudget::new(limit);
        let result: Result<String, ()> = budget.describe_failure(|key, arguments| {
            assert_eq!(key, "tool-result.image-budget-exceeded");
            assert_eq!(arguments, &[("limit_bytes", limit.to_string())]);
            Ok("catalog rendered text".to_owned())
        });
        assert_eq!(result, Ok("catalog rendered text".to_owned()));
        assert_eq!(
            budget.describe_failure(|_, _| Err("missing prompt")),
            Err("missing prompt")
        );
    }
}
