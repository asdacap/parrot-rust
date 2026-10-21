use super::projector;
use serde_json::{Value, json};

#[tokio::test(start_paused = true)]
async fn frozen_input_catalogue_runs_all_families_without_oracle_access() {
    let input = include_str!("fixtures/source/inputs.json");
    let source: Value = serde_json::from_str(input).unwrap();
    let output: Value = serde_json::from_str(&projector::project(input).await.unwrap()).unwrap();
    let inputs = source["cases"].as_array().unwrap();
    let observations = output["cases"].as_array().unwrap();
    assert_eq!(inputs.len(), 165);
    assert_eq!(observations.len(), inputs.len());
    for (input, output) in inputs.iter().zip(observations) {
        assert_eq!(output["case_id"], input["case_id"]);
        assert_eq!(output["family"], input["family"]);
        assert!(output["observation"].is_object(), "{}", input["case_id"]);
    }
}

#[tokio::test(start_paused = true)]
async fn projector_observes_actual_registry_and_captured_routing() {
    let input = json!({"cases":[{"case_id":"local-routing","family":"routing","providers":[{"id":"p","models":[{"id":"a"},{"id":"b"}]}],"aliases":[{"name":"work","target":"p/a","usage":"local"}],"configured_default":"work","selectors":[""],"replacement":{"aliases":[{"name":"work","target":"p/b","usage":"local"}],"configured_default":"work"}}]});
    let output: Value =
        serde_json::from_str(&projector::project(&input.to_string()).await.unwrap()).unwrap();
    let observation = &output["cases"][0]["observation"];
    assert_eq!(
        observation["before"][0]["result"]["canonical"]["model"]["id"],
        "a"
    );
    assert_eq!(
        observation["after"][0]["result"]["canonical"]["model"]["id"],
        "b"
    );
    assert_eq!(
        observation["captured"][0]["result"]["canonical"]["model"]["id"],
        "a"
    );
}

#[tokio::test(start_paused = true)]
async fn original_composed_usage_observes_actual_ids_and_full_seed_sequence_predicates() {
    let input = include_str!("fixtures/source/composed-usage8-v1/inputs.json");
    let inputs: Value = serde_json::from_str(input).unwrap();
    let output: Value = serde_json::from_str(&projector::project(input).await.unwrap()).unwrap();
    assert_eq!(output["contract"], "t32-composed-usage8-source-v1");
    let cases = output["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 8);
    for (input, case) in inputs["cases"].as_array().unwrap().iter().zip(cases) {
        assert_eq!(case["case_id"], input["case_id"]);
        let observation = &case["observation"];
        assert_eq!(observation["provider_id"], input["provider_id"]);
        assert_eq!(observation["retry_provider_id"], observation["provider_id"]);
        assert_eq!(
            observation["provider_seed_model_ids"],
            json!(["declared-model"])
        );
        assert_eq!(
            observation["seed_model_ids"],
            observation["provider_seed_model_ids"]
        );
        assert_eq!(observation["seed_sequence_equal"], true);
        assert_eq!(observation["construction_request_count"], 0);
        let compatible = input["provider_id"] == "compatible";
        assert_eq!(observation["provider_reporter_present"], !compatible);
        assert_eq!(observation["reporter_present"], !compatible);
        assert_eq!(observation["reporter_same_through_retry"], true);
        if compatible {
            assert!(observation["requests"].as_array().unwrap().is_empty());
            assert!(observation["error"].is_null());
        } else if input["credential"] == "" {
            assert!(observation["requests"].as_array().unwrap().is_empty());
            assert_eq!(
                observation["error"]["type"],
                "Parrot.Llm.LLMProviderException"
            );
        } else {
            assert_eq!(observation["requests"].as_array().unwrap().len(), 1);
            assert!(observation["error"].is_null());
            assert!(observation["usage"].is_object());
        }
    }
}

#[tokio::test]
async fn projector_rejects_unknown_family() {
    assert!(
        projector::project(r#"{"cases":[{"case_id":"bad","family":"unknown"}]}"#)
            .await
            .unwrap_err()
            .contains("unknown fixture family")
    );
}
