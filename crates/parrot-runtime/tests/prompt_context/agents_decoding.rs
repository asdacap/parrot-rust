use super::*;

// These tests drive the injected reader through AgentsPrompt, not a decoder-only
// expected-value adapter. Fixture capabilities do not certify a host backend.
#[tokio::test]
async fn agents_readalltext_bom_detection_and_replacement_are_preserved() {
    let (reader, policy) = fixture();
    let provider = AgentsPromptProvider::new(
        "/work/sub",
        "/config",
        reader.clone(),
        policy,
        templates(""),
    )
    .unwrap();
    let mut prompt = provider.materialize(&identity()).unwrap();
    let (_, router) = route(Vec::new());
    let selection = router.resolve("unused/vendor/model").unwrap();
    for (bytes, expected) in [
        (vec![0xef, 0xbb, 0xbf, b'a'], "a"),
        (vec![0xff, 0xfe, b'b', 0], "b"),
        (vec![0xfe, 0xff, 0, b'c'], "c"),
        (vec![0xff, 0xfe, 0, 0, b'd', 0, 0, 0], "d"),
        (vec![0, 0, 0xfe, 0xff, 0, 0, 0, b'e'], "e"),
        (vec![b'f', 0xff], "f�"),
    ] {
        reader
            .files
            .lock()
            .unwrap()
            .insert("/work/sub/AGENTS.md".into(), bytes);
        prompt.sample_epoch(&Never).await.unwrap();
        assert!(prompt.build(&selection).unwrap().contains(expected));
    }
}
