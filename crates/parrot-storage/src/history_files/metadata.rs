use serde::{Deserialize, Serialize};

/// PascalCase `meta.json` listing metadata. Conversation state remains in SQLite.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionMeta {
    #[serde(rename = "Id", deserialize_with = "deserialize_present_nullable")]
    pub id: Option<String>,
    #[serde(
        rename = "WorkingDirectory",
        deserialize_with = "deserialize_present_nullable"
    )]
    pub working_directory: Option<String>,
    #[serde(rename = "RootAgentName", default)]
    pub root_agent_name: Option<String>,
    #[serde(
        rename = "ProviderId",
        deserialize_with = "deserialize_present_nullable"
    )]
    pub provider_id: Option<String>,
    #[serde(rename = "Model", deserialize_with = "deserialize_present_nullable")]
    pub model: Option<String>,
    #[serde(rename = "Selector", default)]
    pub selector: Option<String>,
    #[serde(rename = "Mode", default)]
    pub mode: Option<String>,
    #[serde(rename = "LastOpenedAt", default)]
    pub last_opened_at: Option<String>,
    #[serde(rename = "CreatedAt", default)]
    pub created_at: Option<String>,
}

impl SessionMeta {
    pub fn new(
        id: impl Into<String>,
        working_directory: impl Into<String>,
        provider_id: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            id: Some(id.into()),
            working_directory: Some(working_directory.into()),
            root_agent_name: Some(String::new()),
            provider_id: Some(provider_id.into()),
            model: Some(model.into()),
            selector: Some(String::new()),
            mode: Some(String::new()),
            last_opened_at: Some(String::new()),
            created_at: Some(String::new()),
        }
    }
}

fn deserialize_present_nullable<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn metadata_uses_pascal_case_fields_and_defaults_selector() {
        let metadata = SessionMeta::new("listed", "/workspace", "provider", "model");
        assert_eq!(
            serde_json::to_value(&metadata).unwrap(),
            json!({
                "Id": "listed",
                "WorkingDirectory": "/workspace",
                "RootAgentName": "",
                "ProviderId": "provider",
                "Model": "model",
                "Selector": "",
                "Mode": "",
                "LastOpenedAt": "",
                "CreatedAt": ""
            })
        );
        assert_eq!(
            serde_json::from_str::<SessionMeta>(
                r#"{"Id":"listed","WorkingDirectory":"/workspace","ProviderId":"provider","Model":"model"}"#,
            )
            .unwrap()
            .selector,
            None
        );
    }

    #[test]
    fn required_metadata_properties_must_be_present_but_may_be_null() {
        let complete = serde_json::json!({
            "Id": "listed",
            "WorkingDirectory": "/workspace",
            "ProviderId": "provider",
            "Model": "model"
        });
        assert!(serde_json::from_value::<SessionMeta>(complete.clone()).is_ok());

        for required in ["Id", "WorkingDirectory", "ProviderId", "Model"] {
            let mut missing = complete.clone();
            missing.as_object_mut().unwrap().remove(required);
            assert!(
                serde_json::from_value::<SessionMeta>(missing).is_err(),
                "missing required property {required}"
            );
        }

        let nullable = serde_json::json!({
            "Id": null,
            "WorkingDirectory": null,
            "ProviderId": null,
            "Model": null
        });
        let parsed: SessionMeta = serde_json::from_value(nullable.clone()).unwrap();
        assert_eq!(parsed.id, None);
        assert_eq!(parsed.working_directory, None);
        assert_eq!(parsed.provider_id, None);
        assert_eq!(parsed.model, None);
        let encoded = serde_json::to_value(&parsed).unwrap();
        for required in ["Id", "WorkingDirectory", "ProviderId", "Model"] {
            assert!(encoded[required].is_null());
        }
        assert_eq!(
            serde_json::from_value::<SessionMeta>(encoded).unwrap(),
            parsed
        );
    }

    #[test]
    fn optional_metadata_nulls_round_trip_while_missing_values_use_source_defaults() {
        let explicit_nulls = serde_json::json!({
            "Id": "listed",
            "WorkingDirectory": "/workspace",
            "RootAgentName": null,
            "ProviderId": "provider",
            "Model": "model",
            "Selector": null,
            "Mode": null,
            "LastOpenedAt": null,
            "CreatedAt": null
        });
        let parsed: SessionMeta = serde_json::from_value(explicit_nulls.clone()).unwrap();
        assert_eq!(parsed.root_agent_name, None);
        assert_eq!(parsed.selector, None);
        assert_eq!(parsed.mode, None);
        assert_eq!(parsed.last_opened_at, None);
        assert_eq!(parsed.created_at, None);
        assert_eq!(serde_json::to_value(parsed).unwrap(), explicit_nulls);

        let omitted: SessionMeta = serde_json::from_str(
            r#"{"Id":"listed","WorkingDirectory":"/workspace","ProviderId":"provider","Model":"model"}"#,
        )
        .unwrap();
        // The pinned generated StoreJsonContext supplies null for omitted
        // optional properties, unlike the record's direct-construction defaults.
        assert_eq!(omitted.root_agent_name, None);
        assert_eq!(omitted.selector, None);
        assert_eq!(omitted.mode, None);
        assert_eq!(omitted.last_opened_at, None);
        assert_eq!(omitted.created_at, None);
    }

    #[test]
    fn metadata_bytes_match_the_independent_dotnet_payload() {
        let metadata = SessionMeta::new(
            "session-one",
            "/tmp/c/workspace/t19-state/workspace",
            "provider",
            "provider/model",
        );
        let evidence: serde_json::Value = serde_json::from_str(include_str!("../../tests/history_files/fixtures/format-cases.json"))
        .unwrap();
        let oracle = evidence;
        let expected = oracle["observations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|observation| observation["id"] == "metadata.publish.default-pascalcase")
            .unwrap()["text"]
            .as_str()
            .unwrap();

        assert_eq!(
            super::super::json::serialize_metadata(&metadata).unwrap(),
            expected.as_bytes()
        );
    }
}
