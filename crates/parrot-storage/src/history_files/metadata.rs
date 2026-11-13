use serde::{Deserialize, Serialize};

/// PascalCase session identity, preference, and listing metadata.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionMeta {
    #[serde(rename = "Id", deserialize_with = "deserialize_present_nullable")]
    pub id: Option<String>,
    #[serde(
        rename = "WorkingDirectory",
        deserialize_with = "deserialize_present_nullable"
    )]
    pub working_directory: Option<String>,
    #[serde(
        rename = "RootAgentSessionId",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub root_agent_session_id: Option<String>,
    #[serde(
        rename = "ModeRevision",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub mode_revision: Option<i64>,
    #[serde(
        rename = "AgentLineage",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub agent_lineage: Option<Vec<parrot_domain::values::lineage::AgentLineageRecord>>,
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
    #[serde(
        rename = "ExtraConcurrentTasks",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_task_limit"
    )]
    pub extra_concurrent_tasks: Option<i32>,
    #[serde(rename = "FastMode", default, skip_serializing_if = "Option::is_none")]
    pub fast_mode: Option<bool>,
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
            root_agent_session_id: None,
            mode_revision: None,
            agent_lineage: None,
            root_agent_name: Some(String::new()),
            provider_id: Some(provider_id.into()),
            model: Some(model.into()),
            selector: Some(String::new()),
            mode: Some(String::new()),
            extra_concurrent_tasks: None,
            fast_mode: None,
            last_opened_at: Some(String::new()),
            created_at: Some(String::new()),
        }
    }
}

fn deserialize_task_limit<'de, D>(deserializer: D) -> Result<Option<i32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let limit = Option::<i32>::deserialize(deserializer)?;
    if limit.is_some_and(|limit| limit < 0) {
        return Err(serde::de::Error::custom(
            "ExtraConcurrentTasks must be nonnegative",
        ));
    }
    Ok(limit)
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
    fn task_limit_override_preserves_zero_and_rejects_invalid_values() {
        let metadata = SessionMeta::new("listed", "/workspace", "provider", "model");
        let legacy = serde_json::to_value(&metadata).unwrap();
        assert!(legacy.get("ExtraConcurrentTasks").is_none());
        assert_eq!(
            serde_json::from_value::<SessionMeta>(legacy.clone())
                .unwrap()
                .extra_concurrent_tasks,
            None
        );
        for limit in [0, 3, i32::MAX] {
            let mut saved = metadata.clone();
            saved.extra_concurrent_tasks = Some(limit);
            let encoded = serde_json::to_value(&saved).unwrap();
            assert_eq!(encoded["ExtraConcurrentTasks"], limit);
            assert_eq!(
                serde_json::from_value::<SessionMeta>(encoded).unwrap(),
                saved
            );
        }
        for invalid in [
            json!(-1),
            json!(2147483648_i64),
            json!(1.5),
            json!("2"),
            json!(true),
        ] {
            let mut malformed = legacy.clone();
            malformed["ExtraConcurrentTasks"] = invalid;
            assert!(serde_json::from_value::<SessionMeta>(malformed).is_err());
        }
    }

    #[test]
    fn metadata_bytes_match_captured_payload() {
        let metadata = SessionMeta::new(
            "session-one",
            "/tmp/c/workspace/t19-state/workspace",
            "provider",
            "provider/model",
        );
        let evidence: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/history_files/fixtures/format-cases.json"
        ))
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
