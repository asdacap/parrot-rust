use super::*;

struct FirstHostOnly(AtomicUsize);
impl AddressPolicy for FirstHostOnly {
    fn allows_host(&self, _: &str) -> bool {
        self.0.fetch_add(1, Ordering::SeqCst) == 0
    }
    fn allows(&self, _: IpAddr) -> bool {
        true
    }
}

#[tokio::test]
async fn cached_host_is_reauthorized_before_every_redirect() {
    let policy = Arc::new(FirstHostOnly(AtomicUsize::new(0)));
    let resolver = Arc::new(FixedResolver {
        hosts: Mutex::default(),
        addresses: vec!["127.0.0.1".parse().unwrap()],
    });
    let transport = Arc::new(ScriptedTransport::new(vec![response(
        302,
        Some("/next"),
        None,
        &[],
        b"",
    )]));
    let fetcher = fetcher(
        policy.clone(),
        resolver.clone(),
        transport.clone(),
        options(),
    );
    assert_eq!(
        failed(
            fetcher
                .fetch(get("http://fixture.invalid/start"), &NeverCancel)
                .await
        ),
        "web fetch local or empty host is forbidden"
    );
    assert_eq!(policy.0.load(Ordering::SeqCst), 2);
    assert_eq!(resolver.hosts.lock().unwrap().len(), 1);
    assert_eq!(transport.sent.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn decoder_failure_propagates_as_io_not_a_web_tool_error() {
    let transport = Arc::new(ScriptedTransport::new(vec![response(
        200,
        None,
        Some("text/plain"),
        &["gzip"],
        b"this is not a gzip stream",
    )]));
    let fetcher = Arc::new(WebFetcher::new(
        Arc::new(PrivateAddressPolicy),
        Arc::new(FixedResolver::default()),
        transport,
        options(),
    ));
    let tool = WebFetchTool::<NoFilesystemAccess, ()>::new(
        fetcher,
        ConfiguredToolDefinition::new("web", "{}"),
    );
    let result = tool
        .invoke(
            Invocation {
                arguments_json: r#"{"url":"http://127.0.0.1/"}"#.to_owned(),
                prompt_templates: templates(),
            },
            InvocationContext::new(NoFilesystemAccess, (), &NeverCancel),
        )
        .await;
    assert!(
        matches!(result, Err(OperationError::Failed(FetchError::Io(error))) if error.kind() == io::ErrorKind::InvalidData)
    );
}

struct PendingCompressedBody(Arc<AtomicBool>);
impl Drop for PendingCompressedBody {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
impl ResponseBody for PendingCompressedBody {
    fn read<'a>(&'a mut self, _: &'a mut [u8]) -> BoxFuture<'a, Result<usize, FetchError>> {
        Box::pin(std::future::pending())
    }
}

#[tokio::test]
async fn cancellation_while_waiting_for_decoded_bytes_drops_owned_response() {
    let dropped = Arc::new(AtomicBool::new(false));
    let transport = Arc::new(ScriptedTransport::new(vec![Response {
        status_code: 200,
        location: None,
        content_type: Some("text/plain".to_owned()),
        content_encodings: vec!["gzip".to_owned()],
        body: Box::new(PendingCompressedBody(dropped.clone())),
    }]));
    let fetcher = WebFetcher::new(
        Arc::new(PrivateAddressPolicy),
        Arc::new(FixedResolver::default()),
        transport,
        options(),
    );
    let cancellation = CancelSignal::default();
    let cancel = async {
        tokio::time::sleep(Duration::from_millis(2)).await;
        cancellation.cancel();
    };
    let (result, ()) = tokio::join!(
        fetcher.fetch(get("http://127.0.0.1/"), &cancellation),
        cancel
    );
    assert!(matches!(result, Err(OperationError::Cancelled)));
    assert!(dropped.load(Ordering::SeqCst));
}

#[tokio::test]
async fn configured_error_template_is_used_but_success_text_is_not_reformatted() {
    let (_, templates) = parrot_foundation::template_catalog::PromptTemplateCatalog::from_content(
        "prompt_templates:\n  tool-result.error:\n    engine: scalar\n    template: 'Fetch failed: {message}'\n    allowed_arguments: [message]\n    required_arguments: [message]\n", &[("HOME".to_owned(), "/tmp/c/home".to_owned()), ("XDG_CONFIG_HOME".to_owned(), "/tmp/c/config".to_owned()), ("XDG_STATE_HOME".to_owned(), "/tmp/c/state".to_owned()), ("XDG_DATA_HOME".to_owned(), "/tmp/c/data".to_owned()), ("WORKDIR".to_owned(), "/tmp/c/workspace".to_owned())].into_iter().collect()).unwrap();
    let transport = Arc::new(ScriptedTransport::new(vec![response(
        200,
        None,
        Some("text/plain"),
        &[],
        b"fetched",
    )]));
    let fetcher = Arc::new(fetcher(
        Arc::new(PrivateAddressPolicy),
        Arc::new(FixedResolver::default()),
        transport,
        options(),
    ));
    let tool = WebFetchTool::<NoFilesystemAccess, ()>::new(
        fetcher,
        ConfiguredToolDefinition::new("web", "{}"),
    );
    let templates = Arc::new(templates);
    let result = tool
        .invoke(
            Invocation {
                arguments_json: "[]".to_owned(),
                prompt_templates: templates.clone(),
            },
            InvocationContext::new(NoFilesystemAccess, (), &NeverCancel),
        )
        .await
        .unwrap();
    assert_eq!(result.text, "Fetch failed: web fetch URL is required");
    let result = tool
        .invoke(
            Invocation {
                arguments_json: r#"{"url":"http://127.0.0.1/"}"#.to_owned(),
                prompt_templates: templates,
            },
            InvocationContext::new(NoFilesystemAccess, (), &NeverCancel),
        )
        .await
        .unwrap();
    assert_eq!(result.text, "fetched");
}
