use super::*;
use parrot_domain::values::progress::Progress;

#[tokio::test]
async fn retained_progress_catalog_handle_replaces_closes_and_reopens_one_report() {
    let fixture = Fixture::new("", true, 0);
    let progress = fixture.find("progress").unwrap();
    assert_eq!(
        fixture.catalog.factories().last().unwrap().name(),
        "progress"
    );
    assert_eq!(
        progress.create().parallel_safety(&Invocation::new(
            "progress-call",
            "{}",
            fixture.templates.clone(),
        )),
        ParallelSafety::Sequential
    );
    assert!(!progress.create().enabled_after_interruption());
    for (arguments, expected, report) in [
        (
            json!({"steps":2,"total_steps":5,"current_step":"Testing"}),
            "Progress updated: 2/5.",
            Some(Progress {
                steps: 2,
                total_steps: 5,
                current_step: Some("Testing".into()),
            }),
        ),
        (
            json!({"steps":1,"total_steps":3}),
            "Progress updated: 1/3.",
            Some(Progress {
                steps: 1,
                total_steps: 3,
                current_step: None,
            }),
        ),
        (json!({"close":true}), "Progress closed.", None),
        (json!({"close":true}), "Progress is already closed.", None),
        (
            json!({"steps":3,"total_steps":3}),
            "Progress updated: 3/3.",
            Some(Progress {
                steps: 3,
                total_steps: 3,
                current_step: None,
            }),
        ),
    ] {
        assert_eq!(
            fixture.invoke("progress", arguments).await.unwrap().text,
            expected
        );
        assert_eq!(
            fixture.services.state.lock().unwrap().progress.progress,
            report
        );
    }
    let calls = fixture.services.calls();
    let invalid = fixture
        .invoke("progress", json!({"steps":4,"total_steps":3}))
        .await
        .unwrap();
    assert!(invalid.text.starts_with("error:"));
    assert_eq!(fixture.services.calls(), calls);
    fixture
        .invoke("progress", json!({"close":true}))
        .await
        .unwrap();
    fixture.close().await;
}

#[tokio::test]
async fn progress_schema_reaches_real_provider_and_result_overrides_are_used() {
    let fixture = Fixture::new(
        "tools:\n  progress:\n    description: Report configured progress.\nprompt_templates:\n  progress-tool.updated:\n    template: 'UPDATED {steps} of {total_steps}'\n",
        true,
        0,
    );
    let request = Request {
        model: "vendor/model".into(),
        tools: fixture.definitions(|name| name == "progress"),
        ..Request::default()
    };
    let prepared = chat_completions::prepare(&request, "test").unwrap();
    let body: Value = serde_json::from_slice(&prepared.body).unwrap();
    let definition = &body["tools"][0]["function"];
    assert_eq!(definition["name"], "progress");
    assert_eq!(definition["description"], "Report configured progress.");
    assert_eq!(
        definition["parameters"]["properties"]["steps"]["type"],
        "integer"
    );
    assert_eq!(
        definition["parameters"]["properties"]["close"]["type"],
        "boolean"
    );
    assert_eq!(
        fixture
            .invoke("progress", json!({"steps":0,"total_steps":2}))
            .await
            .unwrap()
            .text,
        "UPDATED 0 of 2"
    );
    fixture
        .invoke("progress", json!({"close":true}))
        .await
        .unwrap();
    fixture.close().await;
}
