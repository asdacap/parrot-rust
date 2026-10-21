mod common;
mod configuration;
mod factories;
mod registry;
mod routing;
mod usage;

use serde_json::{Value, json};

/// Input-only observation of actual production provider and routing components.
pub async fn project(input: &str) -> Result<String, String> {
    let input: Value = serde_json::from_str(input).map_err(|e| e.to_string())?;
    let cases = input
        .get("cases")
        .and_then(Value::as_array)
        .ok_or("input cases must be an array")?;
    let composed = common::text(&input, "contract") == "t32-composed-usage8-source-v1";
    let mut observations = Vec::with_capacity(cases.len());
    for case in cases {
        let case_id = common::text(case, "case_id");
        let family = common::text(case, "family");
        eprintln!("{}", json!({"case_id":case_id,"family":family}));
        let observation = match family {
            "registry" => registry::run(case).await,
            "routing" => routing::run(case).await,
            "configuration" => configuration::run(case).await,
            "factory" => factories::run_factory(case).await,
            "builder" => factories::run_builder(case).await,
            "modelsdev" => factories::run_modelsdev(case).await,
            "usage" if composed => usage::run_composed(case).await,
            "usage" => usage::run(case).await,
            unknown => return Err(format!("unknown fixture family {unknown}")),
        }
        .map_err(|e| format!("{case_id}: {e}"))?;
        observations.push(json!({"case_id":case_id,"family":family,"observation":observation}));
    }
    serde_json::to_string(&json!({"source_commit":"5f27a8a92625ee31d8bec094e5c3e09550193781","contract":if composed { "t32-composed-usage8-source-v1" } else { "t32-routing-source-v1" },"cases":observations})).map_err(|e|e.to_string())
}
