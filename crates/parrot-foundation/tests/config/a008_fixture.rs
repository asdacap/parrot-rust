//! Test-only A008 descriptor admission, not template compilation or rendering.
use parrot_foundation::config::{ConfigError, Result, templates::TemplateSpec};
use serde_json::Value;
use std::collections::BTreeMap;

pub fn inventory() -> Result<Vec<Value>> {
    let source: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/scriban/ordering-oracle.json"
    ))
    .map_err(|error| ConfigError::invalid_data(error.to_string()))?;
    let mapping: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/scriban/a008/bundled-migration-map.json"
    ))
    .map_err(|error| ConfigError::invalid_data(error.to_string()))?;
    let mut rows = source["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["id"] == "catalog-defaults")
        .unwrap()["output"]["value"]
        .as_array()
        .unwrap()
        .clone();
    let entries = mapping["entries"].as_array().unwrap();
    if entries.len() != 10 || rows.len() != 107 {
        return Err(ConfigError::invalid_data(
            "A008 fixture inventory count differs".into(),
        ));
    }
    for entry in entries {
        let row = rows
            .iter_mut()
            .find(|row| row["id"] == entry["id"])
            .ok_or_else(|| ConfigError::invalid_data("A008 fixture descriptor missing".into()))?;
        if *row != entry["old"]
            || entry["new"]["allowed"] != row["allowed"]
            || entry["new"]["required"] != row["required"]
            || entry["new"]["id"] != row["id"]
            || entry["new"]["engine"] != "minijinja"
        {
            return Err(ConfigError::invalid_data(
                "A008 fixture mapping differs from original inventory".into(),
            ));
        }
        row["engine"] = entry["new"]["engine"].clone();
        row["text"] = entry["new"]["text"].clone();
        if *row != entry["new"] {
            return Err(ConfigError::invalid_data(
                "A008 fixture mapping changes unapproved fields".into(),
            ));
        }
    }
    Ok(rows)
}

pub fn specs() -> Result<BTreeMap<String, TemplateSpec>> {
    let mut result = BTreeMap::new();
    for row in inventory()? {
        let names = |key: &str| -> Vec<String> {
            row[key]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| value.as_str().unwrap().to_owned())
                .collect()
        };
        let id = row["id"].as_str().unwrap();
        result.insert(
            id.to_owned(),
            TemplateSpec {
                path: format!("prompt_templates.{id}"),
                text: row["text"].as_str().unwrap().to_owned(),
                engine: row["engine"].as_str().unwrap().to_owned(),
                allowed: names("allowed"),
                required: names("required"),
            },
        );
    }
    Ok(result)
}
