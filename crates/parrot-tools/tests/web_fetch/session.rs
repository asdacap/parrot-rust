use super::*;

#[tokio::test]
async fn actual_loopback_content_type_decodes_latin1_obs_text_before_validation() {
    let mut wire = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain; foo=\"a".to_vec();
    wire.push(0xe9);
    wire.extend_from_slice(b"b\"\r\nContent-Length: 7\r\nConnection: close\r\n\r\nfetched");
    let (port, serving) = local_server(vec![wire]);
    let fetcher = WebFetcher::new(
        Arc::new(PrivateAddressPolicy),
        Arc::new(FixedResolver::default()),
        Arc::new(parrot_tools::web_fetch::ReqwestTransport),
        options(),
    );
    let result = fetcher
        .fetch(
            get(&format!("http://127.0.0.1:{port}/latin1")),
            &NeverCancel,
        )
        .await
        .unwrap();
    assert_eq!(result.text, "fetched");
    assert_eq!(serving.join().unwrap().len(), 1);
}
