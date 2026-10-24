//! ROOT0f8ef324 finite active-v2 contract; strict-v1 remains byte-identical and historical.
use parrot_provider::{
    anthropic::ProviderError,
    catalogs::{CatalogError, StandardModelError},
};
use serde_json::{Value, json};
use std::error::Error;

#[tokio::test]
async fn all_twelve_list_origins_follow_the_exact_root_finite_diagnostic_contract() {
    let inputs: Value = serde_json::from_str(include_str!(
        "../routing/fixtures/catalog-origin-v1/inputs.json"
    ))
    .unwrap();
    let source: Value = serde_json::from_str(include_str!(
        "../routing/fixtures/catalog-origin-v1/catalog.expected.json"
    ))
    .unwrap();
    let original: Value = serde_json::from_str(include_str!("fixtures/inputs.json")).unwrap();
    let template = &original["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["input"]["operation"] == "list")
        .unwrap()["input"];
    let cases = inputs["cases"].as_array().unwrap();
    let expected = source["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 12);
    assert_eq!(expected.len(), cases.len());
    for (index, (case, native)) in cases.iter().zip(expected).enumerate() {
        assert_eq!(case["case_id"], native["case_id"]);
        let id = case["case_id"].as_str().unwrap();
        let mut input = template.clone();
        input["provider_id"] = case["provider_id"].clone();
        input["sse"] = case["live_body"].clone();
        input["options"]["base_url"] = case["base_url"].clone();
        input["options"]["api_key"] = case["credential"].clone();
        let actual = super::projection::observe(id, &input).await;
        let native = &native["observation"];
        if native["model_error"].is_null() {
            assert_eq!(actual["error"], Value::Null, "{id}");
            let body: Value = serde_json::from_str(case["live_body"].as_str().unwrap()).unwrap();
            let returned = actual["models"].as_array().unwrap();
            assert_eq!(returned.len(), body["data"].as_array().unwrap().len());
            assert_eq!(returned[0]["Id"], body["data"][0]["id"]);
        } else {
            assert_eq!(actual["models"], Value::Null, "{id}");
            assert_eq!(
                actual["error"]["category"], native["model_error"]["type"],
                "{id}"
            );
            if id == "malformed" {
                assert_eq!(index, 10);
                assert_eq!(case["live_body"], "{broken");
                assert_eq!(
                    actual["error"]["message"],
                    "key must be a string at line 1 column 2"
                );
                assert_ne!(actual["error"]["message"], native["model_error"]["message"]);
            } else {
                assert_eq!(
                    actual["error"]["message"], native["model_error"]["message"],
                    "{id}"
                );
            }
        }
        let requests = actual["requests"].as_array().unwrap();
        assert_eq!(
            requests.len(),
            native["requests"].as_array().unwrap().len(),
            "{id}"
        );
        assert_eq!(requests.len(), 1, "list-model lookup is not retried");
        assert_eq!(requests[0]["method"], native["requests"][0]["method"]);
        assert_eq!(requests[0]["uri"], native["requests"][0]["uri"]);
        assert_eq!(
            actual["api_key_calls"],
            json!(native["credential_gets"].as_array().unwrap().len())
        );
        assert_eq!(actual["stream_disposed"], true, "{id}");
    }
}

#[test]
fn standard_catalog_wrapper_retains_each_original_tag_display_and_source() {
    for original in [
        StandardModelError::Json(CatalogError("same opaque diagnostic".into())),
        StandardModelError::InvalidOperation(CatalogError("same opaque diagnostic".into())),
        StandardModelError::Provider(CatalogError("same opaque diagnostic".into())),
    ] {
        let error = ProviderError::StandardCatalog(original.clone());
        assert_eq!(error.to_string(), original.to_string());
        assert_eq!(error.source().unwrap().downcast_ref(), Some(&original));
        assert_eq!(
            error.source().unwrap().source().unwrap().to_string(),
            "same opaque diagnostic"
        );
    }
}

#[test]
fn legacy_catalog_conversion_remains_the_original_variant_and_source_behavior() {
    let error = ProviderError::from(CatalogError("legacy catalog failure".into()));
    assert!(matches!(&error, ProviderError::Catalog(inner) if inner.0 == "legacy catalog failure"));
    assert_eq!(error.to_string(), "legacy catalog failure");
    assert!(error.source().is_none());
}
