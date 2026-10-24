use parrot_provider::catalogs::{
    CatalogError, KimiModelDecoder, LiteLlmModelInfoDecoder, Model, ModelCapabilities,
    ModelMetadataFields as F, ModelVariant, ModelsDevCatalogueDecoder, OpenRouterModelDecoder,
    StandardModelDecoder, StandardModelError, filter_chatgpt_external, merge_models,
    supplement_models,
};

fn model(id: &str) -> Model {
    Model::new(id, "p")
}

fn boundary_inputs() -> serde_json::Map<String, serde_json::Value> {
    serde_json::from_str(include_str!("fixtures/catalog-boundary-inputs.json")).unwrap()
}

fn boundary_input(name: &str) -> String {
    boundary_inputs()[name].as_str().unwrap().to_owned()
}

fn assert_legacy_standard_error(json: &str, error: StandardModelError) {
    let legacy = StandardModelDecoder::decode("p", json).unwrap_err();
    let expected = match error {
        StandardModelError::Json(error)
        | StandardModelError::InvalidOperation(error)
        | StandardModelError::Provider(error) => error,
    };
    assert_eq!(legacy, expected, "legacy error changed for {json}");
}

#[test]
fn model_reserves_requested_output_and_five_percent_input_headroom() {
    for (context, output, requested, safe_input) in [
        (180_000, 0, 32_768, Some(140_220)),
        (180_000, -1, 32_768, Some(140_220)),
        (180_000, 65_536, 32_768, Some(140_220)),
        (180_000, 8_192, 8_192, Some(163_626)),
        (1_000, 0, 32_768, Some(0)),
        (1_000, 100, 100, Some(857)),
        (22, 2, 2, Some(19)),
        (23, 3, 3, Some(19)),
        (i32::MAX, 0, 32_768, Some(2_045_191_313)),
        (0, 0, 32_768, None),
        (-1, 8_192, 8_192, None),
    ] {
        let mut model = model("model");
        model.context_window = context;
        model.max_output_tokens = output;
        assert_eq!(model.requested_output_tokens(), requested);
        assert_eq!(model.output_reserved_input_token_limit(), safe_input);
    }
}

#[test]
fn models_dev_decodes_metadata_prices_and_reasoning_efforts() {
    let catalogue = ModelsDevCatalogueDecoder::decode(r#"{"openai":{"id":"openai","unknown":true,"models":{"first":{"id":"gpt-example","name":"Example","limit":{"context":400000,"input":272000,"output":128000},"cost":{"input":5,"cache_read":0,"output":30},"tool_call":false,"reasoning":true,"modalities":{"output":["text","image",7]},"reasoning_options":[{"type":"toggle"},{"type":"effort","values":[null,"high","bogus","low","high"]}]}}}}"#).unwrap();
    let m = &catalogue[0].1[0];
    assert_eq!(
        (m.id.as_str(), m.provider_id.as_str(), m.name.as_str()),
        ("gpt-example", "openai", "Example")
    );
    assert_eq!(
        (m.context_window, m.max_input_tokens, m.max_output_tokens),
        (400000, 272000, 128000)
    );
    assert_eq!(
        (m.input_price, m.cached_input_price, m.output_price),
        (0.000005, 0.0, 0.00003)
    );
    assert_eq!(
        m.capabilities,
        ModelCapabilities {
            tools: false,
            reasoning: true,
            output: vec!["text".into(), "image".into()],
            variants: vec![
                ModelVariant::new("high", "high"),
                ModelVariant::new("low", "low")
            ]
        }
    );
    assert!(
        m.fields.contains(
            F::NAME
                .union(F::CONTEXT_WINDOW)
                .union(F::MAX_INPUT_TOKENS)
                .union(F::MAX_OUTPUT_TOKENS)
                .union(F::INPUT_PRICE)
                .union(F::CACHED_INPUT_PRICE)
                .union(F::OUTPUT_PRICE)
                .union(F::TOOLS)
                .union(F::REASONING)
                .union(F::OUTPUT)
                .union(F::VARIANTS)
        )
    );
}

#[test]
fn models_dev_skips_invalid_records_keeps_first_duplicate_and_ignores_invalid_metadata() {
    let catalogue=ModelsDevCatalogueDecoder::decode(r#"{"invalid":[],"first":{"id":"duplicate","models":{"one":{"id":"first","name":"First"},"two":{"id":"first","name":"Second"},"three":{"id":""},"four":[],"five":{"id":"invalid-values","limit":{"context":-1,"input":1.5,"output":99999999999999999999},"cost":{"input":-1,"cache_read":"NaN","output":"Infinity"},"tool_call":"true","reasoning":null,"modalities":{"output":"text"},"reasoning_options":[{"type":"effort","values":"high"}]}}},"second":{"id":"duplicate","models":{"later":{"id":"later"}}}}"#).unwrap();
    assert_eq!(catalogue.len(), 1);
    assert_eq!(catalogue[0].0, "duplicate");
    assert_eq!(catalogue[0].1.len(), 2);
    assert_eq!(catalogue[0].1[0].name, "First");
    let invalid = &catalogue[0].1[1];
    assert_eq!(invalid.fields, F::NONE);
    assert_eq!(invalid.name, "invalid-values");
    assert!(invalid.capabilities.output.is_empty());
}

#[test]
fn models_dev_ignores_unrecognized_large_numbers_and_enumerates_duplicate_members() {
    let catalogue =
        ModelsDevCatalogueDecoder::decode(&boundary_input("duplicate-members")).unwrap();
    assert_eq!(
        catalogue[0]
            .1
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    let large_number = ModelsDevCatalogueDecoder::decode(&boundary_input("unknown-large")).unwrap();
    assert_eq!(large_number[0].1[0].id, "a");
    assert_eq!(large_number[0].1[0].fields, F::NONE);
}

#[test]
fn models_dev_infers_reasoning_only_from_supported_efforts() {
    let catalogue =
        ModelsDevCatalogueDecoder::decode(&boundary_input("unsupported-efforts")).unwrap();
    for model in &catalogue[0].1 {
        assert!(!model.capabilities.reasoning);
        assert!(model.capabilities.variants.is_empty());
    }
}

#[test]
fn models_dev_prices_accept_invariant_numeric_whitespace() {
    let catalogue = ModelsDevCatalogueDecoder::decode(&boundary_input("whitespace-price")).unwrap();
    let model = &catalogue[0].1[0];
    assert_eq!(model.input_price, 0.000005);
    assert!(model.fields.contains(F::INPUT_PRICE));
}

#[test]
fn catalog_boundary_projection_matches_pinned_dotnet_raw_v2_bytes() {
    let inputs = boundary_inputs();
    let mut output = String::from("[");
    for (index, label) in [
        "unsupported-efforts",
        "unknown-large",
        "duplicate-members",
        "whitespace-price",
    ]
    .iter()
    .enumerate()
    {
        if index > 0 {
            output.push(',');
        }
        let json = inputs[*label].as_str().unwrap();
        let groups = ModelsDevCatalogueDecoder::decode(json).unwrap();
        output.push_str(&format!(
            "{{\"label\":{},\"models\":[",
            serde_json::to_string(label).unwrap()
        ));
        for (model_index, model) in groups[0].1.iter().enumerate() {
            if model_index > 0 {
                output.push(',');
            }
            let variants = model
                .capabilities
                .variants
                .iter()
                .map(|variant| serde_json::to_string(&variant.name).unwrap())
                .collect::<Vec<_>>()
                .join(",");
            let input_price_micros = (model.input_price * 1_000_000.0).round() as u64;
            output.push_str(&format!(
                "{{\"id\":{},\"input_price_micros\":{},\"reasoning\":{},\"variants\":[{}],\"fields\":{}}}",
                serde_json::to_string(&model.id).unwrap(),
                input_price_micros,
                model.capabilities.reasoning,
                variants,
                model.fields.bits(),
            ));
        }
        output.push_str("]}");
    }
    output.push(']');
    assert_eq!(
        output,
        include_str!("fixtures/catalog-boundary-raw-v2.expected.json").trim()
    );
}

fn project_provider_model(model: &Model) -> serde_json::Value {
    let capabilities = &model.capabilities;
    serde_json::json!({
        "id": model.id,
        "provider_id": model.provider_id,
        "name": model.name,
        "context_window": model.context_window,
        "max_input_tokens": model.max_input_tokens,
        "max_output_tokens": model.max_output_tokens,
        "input_price_units": (model.input_price * 1_000_000_000.0).round() as u64,
        "cached_input_price_units": (model.cached_input_price * 1_000_000_000.0).round() as u64,
        "output_price_units": (model.output_price * 1_000_000_000.0).round() as u64,
        "capabilities": {
            "tools": capabilities.tools,
            "reasoning": capabilities.reasoning,
            "output": capabilities.output,
            "variants": capabilities.variants.iter().map(|variant| serde_json::json!({
                "name": variant.name,
                "reasoning_effort": variant.reasoning_effort,
            })).collect::<Vec<_>>(),
        },
        "fields": model.fields.bits(),
    })
}

#[test]
fn canonical_provider_and_merge_projections_match_pinned_dotnet_raw_v2_bytes() {
    let inputs: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/catalog-provider-inputs.json")).unwrap();
    let models_dev_json = inputs["models-dev"].as_str().unwrap();
    let models_dev = ModelsDevCatalogueDecoder::decode(models_dev_json).unwrap();
    let lite_llm =
        LiteLlmModelInfoDecoder::decode("p", inputs["litellm"].as_str().unwrap()).unwrap();
    let open_router =
        OpenRouterModelDecoder::decode("p", inputs["openrouter"].as_str().unwrap()).unwrap();
    let chatgpt_input = inputs["chatgpt_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| Model::new(id.as_str().unwrap(), "openai"))
        .collect::<Vec<_>>();
    let chatgpt = filter_chatgpt_external(&chatgpt_input, "chatgpt");

    let mut configured = model("served");
    configured.name = "Configured".into();
    configured.context_window = 500;
    configured.max_output_tokens = 100;
    configured.input_price = 0.01;
    configured.cached_input_price = 0.005;
    configured.output_price = 0.02;
    configured.capabilities = ModelCapabilities {
        tools: true,
        reasoning: true,
        output: vec!["image".into()],
        variants: vec![ModelVariant::new("high", "high")],
    };
    configured.fields = F::NAME
        .union(F::CONTEXT_WINDOW)
        .union(F::MAX_OUTPUT_TOKENS)
        .union(F::INPUT_PRICE)
        .union(F::CACHED_INPUT_PRICE)
        .union(F::OUTPUT_PRICE)
        .union(F::TOOLS)
        .union(F::REASONING)
        .union(F::OUTPUT)
        .union(F::VARIANTS);
    let mut fetched = model("served");
    fetched.name = "Endpoint".into();
    fetched.context_window = 0;
    fetched.input_price = 0.0;
    fetched.cached_input_price = 0.0;
    fetched.capabilities = ModelCapabilities {
        tools: false,
        reasoning: false,
        output: vec![],
        variants: vec![],
    };
    fetched.fields = F::NAME
        .union(F::CONTEXT_WINDOW)
        .union(F::INPUT_PRICE)
        .union(F::CACHED_INPUT_PRICE)
        .union(F::TOOLS)
        .union(F::REASONING)
        .union(F::OUTPUT)
        .union(F::VARIANTS);
    let merged = merge_models(Some(&[fetched]), &[configured], &[], &[]);

    let projection = serde_json::json!([
        {"label":"models-dev", "models":models_dev[0].1.iter().map(project_provider_model).collect::<Vec<_>>()},
        {"label":"litellm", "models":lite_llm.iter().map(project_provider_model).collect::<Vec<_>>()},
        {"label":"openrouter", "models":open_router.iter().map(project_provider_model).collect::<Vec<_>>()},
        {"label":"chatgpt-filter", "models":chatgpt.iter().map(project_provider_model).collect::<Vec<_>>()},
        {"label":"merge", "models":merged.iter().map(project_provider_model).collect::<Vec<_>>()},
    ]);
    assert_eq!(
        serde_json::to_string(&projection).unwrap(),
        include_str!("fixtures/catalog-provider-projections.expected.json").trim()
    );
}

#[test]
fn merge_prioritizes_explicit_endpoint_false_and_empty_fields() {
    let mut configured = model("served");
    configured.capabilities = ModelCapabilities {
        tools: true,
        reasoning: true,
        output: vec!["image".into()],
        variants: vec![ModelVariant::new("high", "high")],
    };
    configured.fields = F::TOOLS
        .union(F::REASONING)
        .union(F::OUTPUT)
        .union(F::VARIANTS);
    let mut fetched = model("served");
    fetched.capabilities = ModelCapabilities {
        tools: false,
        reasoning: false,
        output: vec![],
        variants: vec![],
    };
    fetched.fields = F::TOOLS
        .union(F::REASONING)
        .union(F::OUTPUT)
        .union(F::VARIANTS);
    let merged = merge_models(Some(&[fetched]), &[configured], &[], &[]);
    assert!(!merged[0].capabilities.tools);
    assert!(!merged[0].capabilities.reasoning);
    assert!(merged[0].capabilities.output.is_empty());
    assert!(merged[0].capabilities.variants.is_empty());
}

#[test]
fn models_dev_rejects_non_object_roots() {
    for json in ["[]", "null", "not-json"] {
        assert!(ModelsDevCatalogueDecoder::decode(json).is_err(), "{json}");
    }
}

#[test]
fn standard_decoder_dedupes_and_defaults_name() {
    let models = StandardModelDecoder::decode(
        "p",
        r#"{"data":[{"id":"m1","context_window":100},{"id":"m1"},{"id":""}]}"#,
    )
    .unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].name, "m1");
    assert_eq!(models[0].context_window, 100);
    assert!(models[0].fields.contains(F::CONTEXT_WINDOW));
}

#[test]
fn standard_decoder_typed_errors_match_archived_t32_source_contrasts() {
    let inputs: serde_json::Value = serde_json::from_str(include_str!(
        "../routing/fixtures/catalog-origin-v1/inputs.json"
    ))
    .unwrap();
    let source: serde_json::Value = serde_json::from_str(include_str!(
        "../routing/fixtures/catalog-origin-v1/catalog.expected.json"
    ))
    .unwrap();
    let source_cases = source["cases"].as_array().unwrap();

    for case in inputs["cases"].as_array().unwrap() {
        let case_id = case["case_id"].as_str().unwrap();
        let body = case["live_body"].as_str().unwrap();
        let source_case = source_cases
            .iter()
            .find(|source_case| source_case["case_id"] == case_id)
            .unwrap();
        let source_error = &source_case["observation"]["model_error"];
        match source_error["type"].as_str() {
            Some("System.InvalidOperationException") => {
                let error = StandardModelDecoder::decode_typed("p", body).unwrap_err();
                let expected = StandardModelError::InvalidOperation(CatalogError(
                    source_error["message"].as_str().unwrap().to_owned(),
                ));
                assert_eq!(error, expected, "{case_id}");
                assert_legacy_standard_error(body, error);
            }
            Some("Parrot.Llm.LLMProviderException") => {
                let error = StandardModelDecoder::decode_typed("p", body).unwrap_err();
                let expected = StandardModelError::Provider(CatalogError(
                    source_error["message"].as_str().unwrap().to_owned(),
                ));
                assert_eq!(error, expected, "{case_id}");
                assert_legacy_standard_error(body, error);
            }
            Some("System.Text.Json.JsonReaderException") => {
                let error = StandardModelDecoder::decode_typed("p", body).unwrap_err();
                assert!(
                    matches!(error, StandardModelError::Json(_)),
                    "{case_id}: {error}"
                );
                assert_legacy_standard_error(body, error);
            }
            None => {
                assert!(source_error.is_null(), "{case_id}");
                let models = StandardModelDecoder::decode_typed("p", body).unwrap();
                assert_eq!(models[0].id, "live-model", "{case_id}");
            }
            Some(source_type) => panic!("unexpected source outcome {source_type} for {case_id}"),
        }
    }
}

#[test]
fn standard_decoder_reads_rich_metadata_without_claiming_invalid_fields() {
    let models=StandardModelDecoder::decode("p",r#"{"data":[{"id":"rich","name":"Rich","max_input_tokens":0,"max_output_tokens":42,"input_cost_per_token":0,"cache_read_input_token_cost":0.00025,"output_cost_per_token":0.002,"supports_function_calling":false,"supports_reasoning":true,"supported_output_modalities":[],"reasoning_effort_levels":["low","high"],"default_reasoning_effort":"high"},{"id":"invalid","max_input_tokens":-1,"input_cost_per_token":-1,"supports_reasoning":"yes","supported_output_modalities":"text","supported_reasoning_efforts":"high"}]}"#).unwrap();
    let rich = &models[0];
    let invalid = &models[1];
    assert_eq!(rich.name, "Rich");
    assert_eq!(rich.context_window, 0);
    assert_eq!(rich.max_input_tokens, 0);
    assert_eq!(rich.max_output_tokens, 42);
    assert_eq!(
        (rich.input_price, rich.cached_input_price, rich.output_price),
        (0.0, 0.00025, 0.002)
    );
    assert!(!rich.capabilities.tools);
    assert!(rich.capabilities.reasoning);
    assert!(rich.capabilities.output.is_empty());
    assert_eq!(
        rich.capabilities
            .variants
            .iter()
            .map(|v| v.name.as_str())
            .collect::<Vec<_>>(),
        ["high", "low"]
    );
    assert_eq!(
        rich.fields,
        F::NAME
            .union(F::MAX_OUTPUT_TOKENS)
            .union(F::MAX_INPUT_TOKENS)
            .union(F::INPUT_PRICE)
            .union(F::CACHED_INPUT_PRICE)
            .union(F::OUTPUT_PRICE)
            .union(F::TOOLS)
            .union(F::REASONING)
            .union(F::OUTPUT)
            .union(F::VARIANTS)
    );
    assert_eq!(invalid.max_input_tokens, 0);
    assert!(invalid.fields.is_empty());
}

#[test]
fn litellm_tolerates_malformed_entries_and_conservatively_combines_deployments() {
    let models=LiteLlmModelInfoDecoder::decode("p",r#"{"data":[null,"bad",7,[],{"model_name":"served","model_info":{"name":"Served","max_input_tokens":128,"max_output_tokens":32,"input_cost_per_token":0.001,"cache_read_input_token_cost":0.0001,"output_cost_per_token":0.002,"supports_function_calling":true,"supports_reasoning":true,"supported_output_modalities":["text","image"],"supported_reasoning_efforts":["none","medium","xhigh","low","bogus","","low",null],"default_reasoning_effort":"medium"}},{"model_name":"served","model_info":{"max_input_tokens":64,"max_output_tokens":64,"input_cost_per_token":0.003,"cache_read_input_token_cost":0.0001,"output_cost_per_token":0.004,"supports_function_calling":false,"supports_reasoning":true,"supported_output_modalities":["text"],"reasoning_effort_levels":["minimal","low","medium","high","max","bogus"]}},{"model_name":"filtered","model_info":{"reasoning_effort_levels":["bogus","",null,"bogus"]}},{"model_name":"missing-info"},{"model_info":{"max_input_tokens":1}}]}"#).unwrap();
    assert_eq!(models.len(), 2);
    let m = &models[0];
    assert_eq!(m.id, "served");
    assert_eq!(m.name, "served");
    assert!(!m.fields.contains(F::NAME));
    assert_eq!(
        (m.context_window, m.max_input_tokens, m.max_output_tokens),
        (64, 64, 32)
    );
    assert!(!m.fields.contains(F::INPUT_PRICE));
    assert_eq!(m.input_price, 0.0);
    assert!(m.fields.contains(F::CACHED_INPUT_PRICE));
    assert_eq!(m.cached_input_price, 0.0001);
    assert!(!m.fields.contains(F::OUTPUT_PRICE));
    assert_eq!(m.output_price, 0.0);
    assert!(!m.capabilities.tools);
    assert!(m.capabilities.reasoning);
    assert_eq!(m.capabilities.output, ["text"]);
    assert_eq!(
        m.capabilities
            .variants
            .iter()
            .map(|v| v.name.as_str())
            .collect::<Vec<_>>(),
        ["medium", "low"]
    );
    assert!(models[1].fields.contains(F::VARIANTS));
    assert!(models[1].capabilities.variants.is_empty());
}

#[test]
fn litellm_combines_configured_and_normalized_metadata_before_deployments() {
    let models=LiteLlmModelInfoDecoder::decode("p",r#"{"data":[{"model_name":"qwen38-27b","litellm_params":{"model_info":{"max_input_tokens":262144,"max_output_tokens":128,"supports_function_calling":true,"supports_reasoning":true,"reasoning_effort_levels":["low","medium","xhigh"]}},"model_info":{"max_input_tokens":null,"max_output_tokens":64,"supports_function_calling":false,"supports_reasoning":null}},{"model_name":"nested-only","litellm_params":{"model_info":{"max_input_tokens":1024,"supports_reasoning":true,"reasoning_effort_levels":["low"]}}},{"model_name":"normalized-only","litellm_params":{"model_info":[]},"model_info":{"max_input_tokens":2048,"supports_reasoning":false}},{"model_name":"malformed-normalized","litellm_params":{"model_info":{"max_input_tokens":4096}},"model_info":"bad"},{"model_name":"duplicate","litellm_params":{"model_info":{"max_input_tokens":8192,"supports_reasoning":true,"reasoning_effort_levels":["low","medium","xhigh"]}},"model_info":{"max_output_tokens":256}},{"model_name":"duplicate","litellm_params":{"model_info":{"max_input_tokens":4096,"supports_reasoning":true,"reasoning_effort_levels":["medium","xhigh"]}},"model_info":{"max_output_tokens":128}},{"model_name":"missing"},{"model_name":"malformed","litellm_params":{"model_info":null},"model_info":[]}] }"#).unwrap();
    assert_eq!(
        models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        [
            "qwen38-27b",
            "nested-only",
            "normalized-only",
            "malformed-normalized",
            "duplicate"
        ]
    );
    let q = &models[0];
    assert_eq!(
        (q.context_window, q.max_input_tokens, q.max_output_tokens),
        (262144, 262144, 64)
    );
    assert!(!q.capabilities.tools);
    assert!(q.capabilities.reasoning);
    assert_eq!(
        q.capabilities
            .variants
            .iter()
            .map(|v| v.name.as_str())
            .collect::<Vec<_>>(),
        ["low", "medium", "xhigh"]
    );
    assert_eq!(
        (models[1].context_window, models[1].max_input_tokens),
        (1024, 1024)
    );
    assert_eq!(
        (models[2].context_window, models[2].max_input_tokens),
        (2048, 2048)
    );
    assert!(!models[2].capabilities.reasoning);
    assert_eq!(
        (models[3].context_window, models[3].max_input_tokens),
        (4096, 4096)
    );
    let duplicate = &models[4];
    assert_eq!(
        (
            duplicate.context_window,
            duplicate.max_input_tokens,
            duplicate.max_output_tokens
        ),
        (4096, 4096, 128)
    );
    assert_eq!(
        duplicate
            .capabilities
            .variants
            .iter()
            .map(|v| v.name.as_str())
            .collect::<Vec<_>>(),
        ["medium", "xhigh"]
    );
}

#[test]
fn litellm_needs_supplement_when_any_supplemental_field_is_missing() {
    assert!(LiteLlmModelInfoDecoder::needs_supplement(&[model(
        "incomplete"
    )]));
    let mut complete = model("complete");
    complete.fields = F::SUPPLEMENTAL;
    assert!(!LiteLlmModelInfoDecoder::needs_supplement(&[complete]));
    assert!(!LiteLlmModelInfoDecoder::needs_supplement(&[]));
}

#[test]
fn supplement_preserves_primary_membership_and_field_precedence() {
    let primary=StandardModelDecoder::decode("p",r#"{"data":[{"id":"served","max_output_tokens":0,"supports_function_calling":false},{"id":"primary-only","supports_reasoning":false}]}"#).unwrap();
    let supplemental=LiteLlmModelInfoDecoder::decode("p",r#"{"data":[{"model_name":"served","model_info":{"max_input_tokens":512,"max_output_tokens":64,"supports_function_calling":true,"supports_reasoning":true,"reasoning_effort_levels":["low","high"]}},{"model_name":"info-only","model_info":{"max_input_tokens":999}},{"model_name":"served","model_info":{"max_input_tokens":1024}}]}"#).unwrap();
    let configured = Model {
        id: "served".into(),
        provider_id: "p".into(),
        name: String::new(),
        context_window: 256,
        max_output_tokens: 0,
        max_input_tokens: 0,
        input_price: 0.01,
        cached_input_price: 0.0,
        output_price: 0.0,
        capabilities: ModelCapabilities {
            tools: true,
            reasoning: false,
            output: vec!["image".into()],
            variants: vec![],
        },
        fields: F::CONTEXT_WINDOW
            .union(F::INPUT_PRICE)
            .union(F::TOOLS)
            .union(F::REASONING)
            .union(F::OUTPUT),
    };
    let supplemented = supplement_models(&primary, &supplemental);
    let merged = merge_models(Some(&supplemented), &[configured], &[], &[]);
    assert_eq!(
        merged.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["primary-only", "served"]
    );
    let served = &merged[1];
    assert_eq!(served.context_window, 512);
    assert_eq!(served.max_input_tokens, 512);
    assert_eq!(served.max_output_tokens, 0);
    assert_eq!(served.input_price, 0.01);
    assert!(!served.capabilities.tools);
    assert!(served.capabilities.reasoning);
    assert_eq!(served.capabilities.output, ["image"]);
    assert_eq!(
        served
            .capabilities
            .variants
            .iter()
            .map(|v| v.name.as_str())
            .collect::<Vec<_>>(),
        ["low", "high"]
    );
}

#[test]
fn openrouter_decodes_pricing_context_and_default_effort_order() {
    let models=OpenRouterModelDecoder::decode("p",r#"{"data":[{"id":"x/y","context_length":9,"pricing":{"prompt":"0.001","input_cache_read":"0.00025","completion":"0.002"},"top_provider":{"max_completion_tokens":42},"reasoning":{"supported_efforts":["low","high"],"default_effort":"high"}}]}"#).unwrap();
    let m = &models[0];
    assert_eq!(
        (m.id.as_str(), m.context_window, m.max_output_tokens),
        ("x/y", 9, 42)
    );
    assert_eq!(
        (m.input_price, m.cached_input_price, m.output_price),
        (0.001, 0.00025, 0.002)
    );
    assert!(m.capabilities.reasoning);
    assert_eq!(
        m.capabilities
            .variants
            .iter()
            .map(|v| v.name.as_str())
            .collect::<Vec<_>>(),
        ["high", "low"]
    );
}

#[test]
fn endpoint_explicit_zero_prices_override_configured_values() {
    let mut configured = model("m");
    configured.input_price = 0.01;
    configured.cached_input_price = 0.005;
    configured.output_price = 0.02;
    configured.fields = F::INPUT_PRICE
        .union(F::CACHED_INPUT_PRICE)
        .union(F::OUTPUT_PRICE);
    let fetched = OpenRouterModelDecoder::decode(
        "p",
        r#"{"data":[{"id":"m","pricing":{"prompt":"0","input_cache_read":"0","completion":"0"}}]}"#,
    )
    .unwrap();
    let merged = merge_models(Some(&fetched), &[configured], &[], &[]);
    assert_eq!(
        (
            merged[0].input_price,
            merged[0].cached_input_price,
            merged[0].output_price
        ),
        (0.0, 0.0, 0.0)
    );
}

#[test]
fn invalid_endpoint_prices_do_not_override_configuration() {
    for invalid in ["-0.1", "NaN", "Infinity"] {
        let mut configured = model("m");
        configured.input_price = 0.01;
        configured.cached_input_price = 0.005;
        configured.fields = F::INPUT_PRICE.union(F::CACHED_INPUT_PRICE);
        let json = format!(r#"{{"data":[{{"id":"m","pricing":{{"prompt":"{invalid}"}}}}]}}"#);
        let fetched = OpenRouterModelDecoder::decode("p", &json).unwrap();
        let merged = merge_models(Some(&fetched), &[configured], &[], &[]);
        assert_eq!(merged[0].input_price, 0.01, "{invalid}");
        assert_eq!(merged[0].cached_input_price, 0.005, "{invalid}");
    }
}

#[test]
fn explicit_endpoint_non_reasoning_suppresses_configured_variants() {
    let mut configured = model("m");
    configured.capabilities = ModelCapabilities {
        tools: false,
        reasoning: true,
        output: vec!["text".into()],
        variants: vec![ModelVariant::new("high", "high")],
    };
    configured.fields = F::REASONING.union(F::VARIANTS);
    let fetched =
        StandardModelDecoder::decode("p", r#"{"data":[{"id":"m","supports_reasoning":false}]}"#)
            .unwrap();
    let merged = merge_models(Some(&fetched), &[configured], &[], &[]);
    assert!(!merged[0].capabilities.reasoning);
    assert!(merged[0].capabilities.variants.is_empty());
    assert!(merged[0].fields.contains(F::VARIANTS));
}

#[test]
fn empty_openrouter_reasoning_container_does_not_clear_configuration() {
    let mut configured = model("m");
    configured.capabilities = ModelCapabilities {
        tools: false,
        reasoning: true,
        output: vec!["text".into()],
        variants: vec![ModelVariant::new("high", "high")],
    };
    configured.fields = F::REASONING.union(F::VARIANTS);
    let fetched =
        OpenRouterModelDecoder::decode("p", r#"{"data":[{"id":"m","reasoning":{}}]}"#).unwrap();
    let merged = merge_models(Some(&fetched), &[configured], &[], &[]);
    assert!(merged[0].capabilities.reasoning);
    assert_eq!(merged[0].capabilities.variants.len(), 1);
}

#[test]
fn merge_prioritizes_endpoint_fields_and_fills_only_omitted_fields_from_configuration() {
    let mut configured = model("served");
    configured.name = "Configured".into();
    configured.context_window = 500;
    configured.max_output_tokens = 100;
    configured.input_price = 0.01;
    configured.cached_input_price = 0.005;
    configured.output_price = 0.02;
    configured.capabilities = ModelCapabilities {
        tools: true,
        reasoning: true,
        output: vec!["image".into()],
        variants: vec![ModelVariant::new("high", "high")],
    };
    configured.fields = F::NAME
        .union(F::CONTEXT_WINDOW)
        .union(F::MAX_OUTPUT_TOKENS)
        .union(F::INPUT_PRICE)
        .union(F::CACHED_INPUT_PRICE)
        .union(F::OUTPUT_PRICE)
        .union(F::TOOLS)
        .union(F::REASONING)
        .union(F::OUTPUT)
        .union(F::VARIANTS);
    let mut fetched = model("served");
    fetched.name = "Endpoint".into();
    fetched.context_window = 0;
    fetched.input_price = 0.0;
    fetched.cached_input_price = 0.0;
    fetched.capabilities = ModelCapabilities {
        tools: false,
        reasoning: false,
        output: vec![],
        variants: vec![],
    };
    fetched.fields = F::NAME
        .union(F::CONTEXT_WINDOW)
        .union(F::INPUT_PRICE)
        .union(F::CACHED_INPUT_PRICE)
        .union(F::TOOLS)
        .union(F::REASONING)
        .union(F::OUTPUT)
        .union(F::VARIANTS);

    let merged = merge_models(Some(&[fetched]), &[configured], &[], &[]);
    let merged = &merged[0];
    assert_eq!(merged.name, "Endpoint");
    assert_eq!(merged.context_window, 0);
    assert_eq!(merged.max_output_tokens, 100);
    assert_eq!(merged.input_price, 0.0);
    assert_eq!(merged.cached_input_price, 0.0);
    assert_eq!(merged.output_price, 0.02);
    assert!(!merged.capabilities.tools);
    assert!(!merged.capabilities.reasoning);
    assert!(merged.capabilities.output.is_empty());
    assert!(merged.capabilities.variants.is_empty());
}

#[test]
fn openrouter_endpoint_fields_are_supplemental_when_not_explicit() {
    let configured = Model {
        id: "served".into(),
        provider_id: "p".into(),
        name: "Configured".into(),
        context_window: 500,
        max_output_tokens: 100,
        max_input_tokens: 0,
        input_price: 0.01,
        cached_input_price: 0.005,
        output_price: 0.02,
        capabilities: ModelCapabilities {
            tools: true,
            reasoning: true,
            output: vec!["image".into()],
            variants: vec![ModelVariant::new("high", "high")],
        },
        fields: F::NAME
            .union(F::CONTEXT_WINDOW)
            .union(F::MAX_OUTPUT_TOKENS)
            .union(F::INPUT_PRICE)
            .union(F::CACHED_INPUT_PRICE)
            .union(F::OUTPUT_PRICE)
            .union(F::TOOLS)
            .union(F::REASONING)
            .union(F::OUTPUT)
            .union(F::VARIANTS),
    };
    let fetched=OpenRouterModelDecoder::decode("p",r#"{"data":[{"id":"served","name":"Endpoint","context_length":0,"pricing":{"prompt":"0","input_cache_read":"0"},"reasoning":{"supported_efforts":[]}}]}"#).unwrap();
    let merged = merge_models(Some(&fetched), &[configured], &[], &[]);
    let m = &merged[0];
    assert_eq!(m.name, "Endpoint");
    assert_eq!(m.context_window, 0);
    assert_eq!(m.max_output_tokens, 100);
    assert_eq!(
        (m.input_price, m.cached_input_price, m.output_price),
        (0.0, 0.0, 0.02)
    );
    assert!(m.capabilities.tools);
    assert!(!m.capabilities.reasoning);
    assert_eq!(m.capabilities.output, ["image"]);
    assert!(m.capabilities.variants.is_empty());
}

#[test]
fn declared_then_defaults_fill_omitted_endpoint_fields_and_defaults_leave_membership_on_refresh() {
    let mut declared = model("served");
    declared.name = "Declared".into();
    declared.fields = F::NAME;
    let another_declared = model("declared");
    let mut default = model("served");
    default.context_window = 500;
    default.fields = F::CONTEXT_WINDOW;
    let fetched = vec![model("served"), model("fresh")];
    let merged = merge_models(
        Some(&fetched),
        &[declared, another_declared],
        &[default],
        &[],
    );
    assert_eq!(
        merged.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["declared", "fresh", "served"]
    );
    assert_eq!(merged[2].name, "Declared");
    assert_eq!(merged[2].context_window, 500);
}

#[test]
fn external_membership_and_metadata_precedence_are_preserved() {
    let mut external = model("shared");
    external.name = "External".into();
    external.context_window = 100;
    external.max_input_tokens = 80;
    external.max_output_tokens = 60;
    external.input_price = 0.01;
    external.cached_input_price = 0.001;
    external.output_price = 0.02;
    external.capabilities = ModelCapabilities {
        tools: true,
        reasoning: true,
        output: vec!["text".into()],
        variants: vec![ModelVariant::new("low", "low")],
    };
    external.fields = all_metadata_fields();
    let mut defaults = model("shared");
    defaults.name = "Default".into();
    defaults.context_window = 200;
    defaults.max_input_tokens = 160;
    defaults.max_output_tokens = 120;
    defaults.input_price = 0.02;
    defaults.cached_input_price = 0.002;
    defaults.output_price = 0.04;
    defaults.capabilities = ModelCapabilities::default();
    defaults.fields = all_metadata_fields();
    let mut declared = model("shared");
    declared.name = "Declared".into();
    declared.context_window = 300;
    declared.max_input_tokens = 240;
    declared.max_output_tokens = 180;
    declared.input_price = 0.03;
    declared.cached_input_price = 0.003;
    declared.output_price = 0.06;
    declared.capabilities = ModelCapabilities {
        tools: true,
        reasoning: true,
        output: vec!["image".into()],
        variants: vec![ModelVariant::new("high", "high")],
    };
    declared.fields = all_metadata_fields();
    let mut fetched = model("shared");
    fetched.name = "Live".into();
    fetched.fields = all_metadata_fields();
    let seeded = merge_models(
        None,
        &[declared.clone()],
        [defaults.clone(), model("default-only")].as_slice(),
        &[external.clone(), model("external-only")],
    );
    let defaulted = merge_models(None, &[], &[defaults], &[external]);
    let refreshed = merge_models(
        Some(&[fetched.clone(), model("live-only"), fetched]),
        &[declared],
        &[model("default-only")],
        &[model("external-only")],
    );
    let shared = &seeded[2];
    assert_eq!(
        (
            shared.name.as_str(),
            shared.context_window,
            shared.max_input_tokens
        ),
        ("Declared", 300, 240)
    );
    assert_eq!(
        (
            defaulted[0].name.as_str(),
            defaulted[0].context_window,
            defaulted[0].max_input_tokens
        ),
        ("Default", 200, 160)
    );
    assert_eq!(
        refreshed.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["external-only", "live-only", "shared"]
    );
    let live = &refreshed[2];
    assert_eq!(live.name, "Live");
    assert_eq!(
        (
            live.context_window,
            live.max_input_tokens,
            live.max_output_tokens
        ),
        (0, 0, 0)
    );
    assert_eq!(
        (live.input_price, live.cached_input_price, live.output_price),
        (0.0, 0.0, 0.0)
    );
    assert!(!live.capabilities.tools);
    assert!(!live.capabilities.reasoning);
    assert!(live.capabilities.output.is_empty());
    assert!(live.capabilities.variants.is_empty());
}

#[test]
fn empty_external_refresh_preserves_declared_but_drops_absent_defaults() {
    let defaults = [model("default")];
    let declared = [model("declared")];
    let seeded = merge_models(None, &declared, &defaults, &[]);
    let refreshed = merge_models(Some(&[model("live")]), &declared, &defaults, &[]);
    assert_eq!(
        seeded.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["declared", "default"]
    );
    assert_eq!(
        refreshed.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["declared", "live"]
    );
}

#[test]
fn synthetic_standard_capabilities_do_not_override_configuration() {
    let mut configured = model("m");
    configured.capabilities = ModelCapabilities {
        tools: false,
        reasoning: true,
        output: vec!["image".into()],
        variants: vec![ModelVariant::new("high", "high")],
    };
    configured.fields = F::TOOLS
        .union(F::REASONING)
        .union(F::OUTPUT)
        .union(F::VARIANTS);
    let fetched = StandardModelDecoder::decode("p", r#"{"data":[{"id":"m"}]}"#).unwrap();
    let merged = merge_models(Some(&fetched), &[configured], &[], &[]);
    assert!(!merged[0].capabilities.tools);
    assert!(merged[0].capabilities.reasoning);
    assert_eq!(merged[0].capabilities.output, ["image"]);
    assert_eq!(merged[0].capabilities.variants.len(), 1);
}

#[test]
fn configured_variant_order_is_preserved() {
    let mut defaults = model("m");
    defaults.capabilities.variants = ["high", "low", "medium"]
        .into_iter()
        .map(|v| ModelVariant::new(v, v))
        .collect();
    let merged = merge_models(None, &[], &[defaults], &[]);
    assert_eq!(
        merged[0]
            .capabilities
            .variants
            .iter()
            .map(|v| v.name.as_str())
            .collect::<Vec<_>>(),
        ["high", "low", "medium"]
    );
}

#[test]
fn kimi_maps_display_name_and_supported_efforts() {
    let models =
        KimiModelDecoder::decode("kimi", include_str!("fixtures/kimi-catalog-v1.json")).unwrap();
    let m = &models[0];
    assert_eq!(
        (m.id.as_str(), m.name.as_str(), m.context_window),
        ("moonshot-v1-128k", "Moonshot v1 128K", 131072)
    );
    assert!(m.capabilities.reasoning);
    assert_eq!(
        m.capabilities
            .variants
            .iter()
            .map(|v| v.name.as_str())
            .collect::<Vec<_>>(),
        ["high", "medium", "low"]
    );
    assert_eq!(models[1].context_window, -1);
    assert!(models[1].capabilities.reasoning);
    assert!(models[1].capabilities.variants.is_empty());
    let expected: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/kimi-catalog-v1.expected.json")).unwrap();
    for (index, model) in models.iter().enumerate() {
        let observed = serde_json::json!({
            "id": &model.id,
            "provider_id": &model.provider_id,
            "name": &model.name,
            "context_window": model.context_window,
            "capabilities": {
                "tools": model.capabilities.tools,
                "reasoning": model.capabilities.reasoning,
                "output": &model.capabilities.output,
                "variants": model.capabilities.variants.iter().map(|variant| serde_json::json!({
                    "name": &variant.name,
                    "reasoning_effort": &variant.reasoning_effort,
                })).collect::<Vec<_>>(),
            },
            "fields": model.fields.bits(),
        });
        assert_eq!(observed, expected[index]);
    }
}

#[test]
fn kimi_reports_variants_field_even_when_effort_support_is_false() {
    let models = KimiModelDecoder::decode(
        "kimi",
        r#"{"data":[{"id":"m","think_efforts":{"support":false,"valid_efforts":["low"]}}]}"#,
    )
    .unwrap();
    assert!(models[0].fields.contains(F::VARIANTS));
    assert!(models[0].capabilities.variants.is_empty());
    assert!(!models[0].capabilities.reasoning);
}

#[test]
fn chatgpt_external_filter_uses_explicit_allow_deny_and_version_rules() {
    let cases = [
        ("gpt-5.5", true),
        ("gpt-5.3-codex-spark", true),
        ("gpt-5.4", true),
        ("gpt-5.4-mini", true),
        ("gpt-6-astra", true),
        ("gpt-6", true),
        ("gpt-6.0-astra", true),
        ("gpt-7", true),
        ("gpt-10", true),
        ("gpt-5.5-astra", true),
        ("gpt-5.9", true),
        ("gpt-5.10", true),
        ("gpt-5.10-astra", true),
        ("gpt-5.40", true),
        ("gpt-6garbage", true),
        ("gpt-6.", true),
        ("gpt-6.1.2", true),
        ("gpt-5", false),
        ("gpt-5.4-astra", false),
        ("gpt-5.04-astra", false),
        ("gpt-4.1", false),
        ("gpt-4.99", false),
        ("gpt-5.5-pro", false),
        ("gpt-5.6", false),
        ("not-a-gpt-model", false),
    ];
    for (id, expected) in cases {
        let filtered = filter_chatgpt_external(&[model(id)], "chatgpt");
        assert_eq!(filtered.len() == 1, expected, "{id}");
        if expected {
            assert_eq!(filtered[0].provider_id, "chatgpt");
        }
    }
}

#[test]
fn chatgpt_seed_and_fetched_catalogues_keep_endpoint_config_and_external_precedence() {
    let mut current_external = model("gpt-5.5-astra");
    current_external.provider_id = "openai".into();
    current_external.context_window = 1_050_000;
    current_external.max_input_tokens = 922_000;
    current_external.max_output_tokens = 64_000;
    current_external.input_price = 0.000005;
    current_external.cached_input_price = 0.0000005;
    current_external.output_price = 0.00003;
    current_external.fields = F::CONTEXT_WINDOW
        .union(F::MAX_INPUT_TOKENS)
        .union(F::MAX_OUTPUT_TOKENS)
        .union(F::INPUT_PRICE)
        .union(F::CACHED_INPUT_PRICE)
        .union(F::OUTPUT_PRICE);
    let mut future_external = model("gpt-6-astra");
    future_external.provider_id = "openai".into();
    future_external.input_price = 0.00001;
    future_external.fields = F::INPUT_PRICE;
    let mut excluded_external = model("gpt-4.1");
    excluded_external.provider_id = "openai".into();
    excluded_external.name = "External old model".into();
    excluded_external.fields = F::NAME;
    let external = filter_chatgpt_external(
        &[current_external, future_external, excluded_external],
        "chatgpt",
    );
    let mut declared = model("gpt-5.5-astra");
    declared.provider_id = "chatgpt".into();
    declared.input_price = 0.000007;
    declared.fields = F::INPUT_PRICE;
    let mut default = model("gpt-5.5-astra");
    default.provider_id = "chatgpt".into();
    default.output_price = 0.00004;
    default.fields = F::OUTPUT_PRICE;

    let seeded = merge_models(None, &[declared.clone()], &[default.clone()], &external);
    assert_eq!(
        seeded.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["gpt-5.5-astra", "gpt-6-astra"]
    );
    let seeded_codex = &seeded[0];
    assert_eq!(
        (
            seeded_codex.context_window,
            seeded_codex.max_input_tokens,
            seeded_codex.max_output_tokens
        ),
        (1_050_000, 922_000, 64_000)
    );
    assert_eq!(
        (
            seeded_codex.input_price,
            seeded_codex.cached_input_price,
            seeded_codex.output_price
        ),
        (0.000007, 0.0000005, 0.00004)
    );

    let mut live_old = model("gpt-4.1");
    live_old.provider_id = "chatgpt".into();
    live_old.name = "Live old model".into();
    live_old.context_window = 100_000;
    live_old.fields = F::NAME.union(F::CONTEXT_WINDOW);
    let mut live_codex = model("gpt-5.5-astra");
    live_codex.provider_id = "chatgpt".into();
    live_codex.name = "Live 5.5".into();
    live_codex.context_window = 900_000;
    live_codex.fields = F::NAME.union(F::CONTEXT_WINDOW);
    let refreshed = merge_models(
        Some(&[live_old, live_codex]),
        &[declared],
        &[default],
        &external,
    );
    assert_eq!(
        refreshed.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["gpt-4.1", "gpt-5.5-astra", "gpt-6-astra"]
    );
    let codex = &refreshed[1];
    assert_eq!(codex.name, "Live 5.5");
    assert_eq!(codex.context_window, 900_000);
    assert_eq!(
        (codex.max_input_tokens, codex.max_output_tokens),
        (922_000, 64_000)
    );
    assert_eq!(
        (
            codex.input_price,
            codex.cached_input_price,
            codex.output_price
        ),
        (0.000007, 0.0000005, 0.00004)
    );
    assert_eq!(refreshed[2].input_price, 0.00001);
}

#[test]
fn models_dev_openai_catalogue_projects_only_allowed_models_to_chatgpt() {
    let source = ModelsDevCatalogueDecoder::decode(r#"{"openai":{"id":"openai","models":{"old":{"id":"gpt-4.1"},"legacy":{"id":"gpt-5.4"},"future":{"id":"gpt-5.5-astra"}}}}"#).unwrap();
    let openai = &source[0].1;
    let chatgpt_external = filter_chatgpt_external(openai, "chatgpt");
    let mut endpoint_model = model("gpt-4.1");
    endpoint_model.provider_id = "chatgpt".into();
    let endpoint = [endpoint_model];
    let chatgpt = merge_models(Some(&endpoint), &[], &[], &chatgpt_external);
    assert_eq!(
        openai.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["gpt-4.1", "gpt-5.4", "gpt-5.5-astra"]
    );
    assert_eq!(
        chatgpt.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["gpt-4.1", "gpt-5.4", "gpt-5.5-astra"]
    );
    assert!(chatgpt.iter().all(|m| m.provider_id == "chatgpt"));
}

#[test]
fn decoders_reject_missing_catalogue_envelopes_and_preserve_no_usable_error() {
    assert_eq!(
        StandardModelDecoder::decode("p", "{}").unwrap_err().0,
        "provider: models response contains no usable models"
    );
    assert_eq!(
        OpenRouterModelDecoder::decode("p", "{}").unwrap_err().0,
        "provider: models response contains no usable models"
    );
    assert_eq!(
        KimiModelDecoder::decode("p", "{}").unwrap_err().0,
        "provider: models response contains no usable models"
    );
    assert_eq!(
        LiteLlmModelInfoDecoder::decode("p", "{}").unwrap_err().0,
        "provider: model info response does not contain a data array"
    );
}

fn all_metadata_fields() -> F {
    F::NAME
        .union(F::CONTEXT_WINDOW)
        .union(F::MAX_INPUT_TOKENS)
        .union(F::MAX_OUTPUT_TOKENS)
        .union(F::INPUT_PRICE)
        .union(F::CACHED_INPUT_PRICE)
        .union(F::OUTPUT_PRICE)
        .union(F::TOOLS)
        .union(F::REASONING)
        .union(F::OUTPUT)
        .union(F::VARIANTS)
}
