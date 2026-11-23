//! Codemode end-to-end: one turn's codemode call runs a script that batches
//! two nested reads, and the model-visible record holds only the script's own
//! result plus a nested-call summary, never per-call nested entries.

use super::*;
use parrot_runtime::agent_turn::lifecycle::ExecutionStatus;

#[tokio::test]
async fn codemode_call_batches_two_reads_and_nested_text_stays_out_of_the_model_record() {
    let fixture = Fixture::new();
    let mut read_alpha = RecordingTool::new(&fixture, "read_alpha", ParallelSafety::ParallelSafe);
    read_alpha.output = ToolOutput::text("ALPHA-SECRET-BODY");
    let mut read_beta = RecordingTool::new(&fixture, "read_beta", ParallelSafety::ParallelSafe);
    read_beta.output = ToolOutput::text("BETA-SECRET-BODY");
    let read_alpha = Arc::new(read_alpha);
    let read_beta = Arc::new(read_beta);

    let siblings = vec![
        parrot_codemode::DeclaredTool {
            name: "read_alpha".into(),
            description: "Read the alpha file".into(),
            parameters_json: serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
            }),
            output_schema: None,
        },
        parrot_codemode::DeclaredTool {
            name: "read_beta".into(),
            description: "Read the beta file".into(),
            parameters_json: serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
            }),
            output_schema: None,
        },
    ];
    let codemode_tool: Arc<parrot_codemode::CodemodeTool<Security>> = Arc::new(
        parrot_codemode::CodemodeTool::new(Arc::new(parrot_codemode::CodemodeStore::new())),
    );
    codemode_tool.inject(parrot_codemode::RunnerInjection {
        runner: Arc::new(parrot_codemode::SiblingToolRunner::new(vec![
            ToolHandle::Ordinary(Arc::clone(&read_alpha)
                as Arc<
                    dyn parrot_domain::contracts::tools::Tool<
                            Definition = ToolMetadata,
                            Invocation = ToolInvocation,
                            Selection = TurnSelection<Security>,
                            Security = SharedSecurity<Security>,
                            Output = ToolOutput,
                            Error = ToolFailure,
                        >,
                >),
            ToolHandle::Ordinary(Arc::clone(&read_beta)
                as Arc<
                    dyn parrot_domain::contracts::tools::Tool<
                            Definition = ToolMetadata,
                            Invocation = ToolInvocation,
                            Selection = TurnSelection<Security>,
                            Security = SharedSecurity<Security>,
                            Output = ToolOutput,
                            Error = ToolFailure,
                        >,
                >),
        ])),
        siblings,
        inline_budget: None,
    });

    let script = "const [a, b] = await Promise.all([tools.read_alpha({path: 'alpha'}), tools.read_beta({path: 'beta'})]); text('joined ' + a.length + '+' + b.length);";
    let mut codemode_call = call("codemode-1", "codemode");
    codemode_call.arguments_json = serde_json::json!({ "code": script }).to_string();

    let mut first = Step::complete("the model requests the script");
    first.completion.as_mut().unwrap().tool_calls = vec![codemode_call];
    let provider = Scripted::new(vec![
        first,
        Step::complete("answer using the script result"),
    ]);
    let (session, _factory) = runtime::runtime(
        &fixture,
        &provider,
        vec![
            ToolHandle::Ordinary(Arc::clone(&read_alpha) as Arc<_>),
            ToolHandle::Ordinary(Arc::clone(&read_beta) as Arc<_>),
            ToolHandle::Ordinary(Arc::clone(&codemode_tool) as Arc<_>),
        ],
    )
    .await;
    let sent = runtime::send(&session).await;
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );

    // The runner ran both nested reads exactly once each.
    assert_eq!(read_alpha.calls.lock().unwrap().len(), 1);
    assert_eq!(read_beta.calls.lock().unwrap().len(), 1);

    let requests = provider.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);

    // The model-visible tool message is the script's result: the joined
    // summary the script printed, plus the nested-call summary lines. The
    // nested tools' own text never appears, and neither does a nested
    // per-call tool message.
    let tool_messages: Vec<_> = requests[1]
        .messages
        .iter()
        .filter(|message| !message.tool_call_id.is_empty())
        .collect();
    assert_eq!(
        tool_messages.len(),
        1,
        "exactly one tool message (the codemode call), got {:?}",
        tool_messages
            .iter()
            .map(|message| message.tool_call_id.clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(tool_messages[0].tool_call_id, "codemode-1");
    let content = tool_messages[0].content();
    assert!(content.contains("Script completed"), "{}", content);
    assert!(content.contains("joined 17+16"), "{}", content);
    assert!(content.contains("c1 read_alpha ok"), "{}", content);
    assert!(content.contains("c2 read_beta ok"), "{}", content);
    assert!(
        !content.contains("ALPHA-SECRET-BODY"),
        "nested read_alpha text leaked into the model record: {}",
        content
    );
    assert!(
        !content.contains("BETA-SECRET-BODY"),
        "nested read_beta text leaked into the model record: {}",
        content
    );

    // The event record carries one started/finished pair for codemode and
    // none for the nested reads.
    let events = fixture.events().await;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_tool_started())
            .map(|event| event.tool_started().tool_name.clone())
            .collect::<Vec<_>>(),
        ["codemode"]
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_tool_finished())
            .map(|event| event.tool_finished().tool_name.clone())
            .collect::<Vec<_>>(),
        ["codemode"]
    );
    assert_eq!(read_alpha.completed.lock().unwrap().len(), 1);
    assert_eq!(read_beta.completed.lock().unwrap().len(), 1);
    bounded(session.close()).await.unwrap();
}
