use serde_json::Value;

pub fn cases() -> Vec<Value> {
    let root: Value = serde_json::from_str(include_str!("fixtures/source-cases.json")).unwrap();
    root["cases"].as_array().unwrap().clone()
}

pub fn cases_for(method_prefix: &str) -> Vec<Value> {
    cases()
        .into_iter()
        .filter(|case| {
            case["display_name"]
                .as_str()
                .unwrap()
                .starts_with(method_prefix)
        })
        .collect()
}

pub fn arguments(case: &Value) -> Option<Vec<Value>> {
    case.get("original_test_input_json")
        .and_then(|input| input.get("arguments"))
        .and_then(Value::as_array)
        .cloned()
}

pub fn hex_decode(text: &str) -> Vec<u8> {
    assert_eq!(text.len() % 2, 0);
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
        .collect()
}
