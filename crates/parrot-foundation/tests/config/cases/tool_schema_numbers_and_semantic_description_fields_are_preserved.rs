use crate::support::ConfigFixture;
use serde_json::Value;
#[test]
fn source_assertions() {
    let fixture = ConfigFixture::new();
    let config=fixture.configured("tools:\n  read:\n    parameters:\n      properties:\n        offset:\n          maximum: 1e100\n          default:\n            description: ''\n").unwrap();
    let json = &config.runtime.tools["read"].parameters_json;
    let schema: Value = serde_json::from_str(json).unwrap();
    assert!(json.contains("\"maximum\":1e100"));
    assert_eq!(schema["properties"]["offset"]["default"]["description"], "");
}
