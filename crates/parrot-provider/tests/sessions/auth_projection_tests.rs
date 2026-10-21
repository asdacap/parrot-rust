use super::projection_tests::projection;

fn public_input(predicate: impl Fn(&serde_json::Value) -> bool) -> String {
    let inputs: serde_json::Value = serde_json::from_str(include_str!("../routing/fixtures/source/auth-cases-v1/inputs.json"))
    .unwrap();
    let collaborators: serde_json::Value = serde_json::from_str(include_str!("../routing/fixtures/source/auth-cases-v1/collaborators.json"))
    .unwrap();
    let cases = inputs["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|case| predicate(case))
        .cloned()
        .collect::<Vec<_>>();
    serde_json::json!({"cases":cases,"collaborators":collaborators}).to_string()
}

#[tokio::test(start_paused = true)]
async fn typed_auth_retries_through_actual_direct_and_session_wrappers() {
    let output: serde_json::Value = serde_json::from_str(
        &projection::project(&public_input(|case| {
            case["failure"] == "auth"
                && case["visible"] == false
                && case["caller_cancelled"] == false
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(output["contract"], "t32-auth-source-v1");
    for case in output["cases"].as_array().unwrap() {
        let result = &case["observation"];
        assert!(result["physical_calls"].as_u64().unwrap() > 1);
        assert_eq!(result["physical_calls"], result["enumerator_disposals"]);
        assert_eq!(result["error"]["type"], "Parrot.Auth.AuthException");
        assert_eq!(result["error"]["message"], "synthetic auth failure");
        assert_eq!(result["http_sends"], 0);
        assert_eq!(result["credential_gets"], 0);
    }
    assert_eq!(output["native_errors"].as_array().unwrap().len(), 2);
    assert!(
        output["native_errors"][0]["error"]["variant"]
            .as_str()
            .unwrap()
            .starts_with("Authentication(Message(")
    );
    assert_eq!(
        output["native_errors"][0]["error"]["source_chain"][0]["message"],
        "synthetic auth failure"
    );
}

#[tokio::test(start_paused = true)]
async fn actual_foundation_stored_keys_preserve_auth_vs_missing_key_behavior() {
    let output: serde_json::Value = serde_json::from_str(
        &projection::project(&public_input(|case| case["factory"] == true))
            .await
            .unwrap(),
    )
    .unwrap();
    for case in output["cases"].as_array().unwrap() {
        let result = &case["observation"];
        assert_eq!(result["http_sends"], 0);
        assert_eq!(result["credential_gets"], result["physical_calls"]);
        assert_eq!(result["enumerator_disposals"], result["physical_calls"]);
        if case["failure"] == "factory_auth" {
            assert!(result["physical_calls"].as_u64().unwrap() > 1);
            assert_eq!(result["error"]["type"], "Parrot.Auth.AuthException");
        } else {
            assert_eq!(result["physical_calls"], 1);
            assert_eq!(result["error"]["type"], "Parrot.Llm.LLMProviderException");
        }
    }
}

#[tokio::test(start_paused = true)]
async fn script_visibility_and_caller_cancellation_stop_retry_admission() {
    let output: serde_json::Value = serde_json::from_str(
        &projection::project(&public_input(|case| {
            case["visible"] == true || case["caller_cancelled"] == true
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    for case in output["cases"].as_array().unwrap() {
        let result = &case["observation"];
        assert_eq!(result["physical_calls"], 1);
        assert_eq!(result["enumerator_disposals"], 1);
        assert_eq!(result["delays_ms"], serde_json::json!([]));
        assert_eq!(result["error"]["type"], "Parrot.Auth.AuthException");
        if case["case_id"].as_str().unwrap().contains("visible") {
            assert_eq!(result["events"][0]["kind"], "TextDelta");
            assert_eq!(result["events"][0]["text"], "synthetic visible output");
        }
    }
}

#[tokio::test(start_paused = true)]
async fn original_constructor_details_survive_direct_and_session_wrappers() {
    let output: serde_json::Value = serde_json::from_str(
        &projection::project(&public_input(|case| {
            matches!(
                case["failure"].as_str(),
                Some("format" | "transport" | "io" | "argument_range")
            )
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    for case in output["cases"].as_array().unwrap() {
        let message = &case["observation"]["error"]["message"];
        let native = output["native_errors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|native| native["case_id"] == case["case_id"])
            .unwrap();
        assert_eq!(message, &native["error"]["message"]);
        match case["failure"].as_str().unwrap() {
            "format" => assert_eq!(message, "synthetic format failure"),
            "transport" => assert_eq!(message, "synthetic transport failure"),
            "io" => assert_eq!(message, "synthetic IO failure"),
            "argument_range" => assert_eq!(
                message,
                "synthetic range failure (Parameter 'source_value')"
            ),
            _ => unreachable!(),
        }
    }
}

#[tokio::test(start_paused = true)]
async fn explicit_public_counterpart_inputs_keep_their_distinct_contract_and_messages() {
    let inputs: serde_json::Value = serde_json::from_str(include_str!("../routing/fixtures/source/auth-counterparts-v1/inputs.json"))
    .unwrap();
    let collaborators: serde_json::Value = serde_json::from_str(include_str!("../routing/fixtures/source/auth-cases-v1/collaborators.json"))
    .unwrap();
    let input = serde_json::json!({"contract":"t32-auth-counterparts-source-v1","cases":inputs["cases"],"collaborators":collaborators}).to_string();
    let output: serde_json::Value =
        serde_json::from_str(&projection::project(&input).await.unwrap()).unwrap();
    assert_eq!(output["contract"], "t32-auth-counterparts-source-v1");
    assert_eq!(output["cases"].as_array().unwrap().len(), 8);
    for (index, case) in output["cases"].as_array().unwrap().iter().enumerate() {
        assert_eq!(
            case["observation"]["error"]["type"],
            inputs["cases"][index]["constructor"]["clr_type"]
        );
        assert_eq!(
            case["observation"]["error"]["message"],
            output["native_errors"][index]["error"]["message"]
        );
        if case["failure"] == "argument_range" {
            assert_eq!(case["observation"]["error"]["message"], "range test");
        }
    }
}

#[tokio::test(start_paused = true)]
async fn wrong_actual_variant_same_failure_label_and_message_changes_selected_type() {
    let input = public_input(|case| case["case_id"] == "auth-direct-auth");
    let original: serde_json::Value =
        serde_json::from_str(&projection::project(&input).await.unwrap()).unwrap();
    let mut mutated: serde_json::Value = serde_json::from_str(&input).unwrap();
    mutated["cases"][0]["constructor"] = serde_json::json!({"clr_type":"System.InvalidOperationException","arguments":["synthetic auth failure"]});
    let changed: serde_json::Value =
        serde_json::from_str(&projection::project(&mutated.to_string()).await.unwrap()).unwrap();
    assert_eq!(
        original["cases"][0]["failure"],
        changed["cases"][0]["failure"]
    );
    assert_eq!(
        original["cases"][0]["observation"]["error"]["message"],
        changed["cases"][0]["observation"]["error"]["message"]
    );
    assert_eq!(
        original["cases"][0]["observation"]["error"]["type"],
        "Parrot.Auth.AuthException"
    );
    assert_eq!(
        changed["cases"][0]["observation"]["error"]["type"],
        "System.InvalidOperationException"
    );
    assert_ne!(
        original["native_errors"][0]["error"]["variant"],
        changed["native_errors"][0]["error"]["variant"]
    );
    assert_ne!(
        original["cases"][0]["observation"]["diagnostic_classification"],
        changed["cases"][0]["observation"]["diagnostic_classification"]
    );
}
