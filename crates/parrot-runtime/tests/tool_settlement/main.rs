//! T47: retained tools reconciled through actual SQLite/JSONL and the T46 drain.
mod fixture;
use fixture::*;
mod harness;
use harness::*;
mod admission;
mod focused;
mod images;
mod parallel_images;
mod runtime;
mod runtime_ordinary;
mod owned_recovery;
mod settlement;
mod snapshot;

#[tokio::test]
async fn fixture_reopens_real_sqlite_and_agent_history() {
    let fixture = Fixture::new();
    let sequence = fixture.batch(vec![call("fixture", "read")]).await;
    let reopened = Fixture::open(&fixture.path);
    let history = Fixture::history_for(&fixture.path, &reopened);
    let stored = history.conversation().await.unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].sequence, sequence);
    assert_eq!(stored[0].message.tool_calls[0].id, "fixture");
    assert!(fixture.jsonl().contains("fixture"));
}
