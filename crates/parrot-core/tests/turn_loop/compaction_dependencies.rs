use super::*;

pub(super) struct MemoryStatus {
    pub(super) contexts: Mutex<Vec<ContextSnapshot>>,
}

impl MemoryStatus {
    pub(super) fn offering(
        &self,
        context: ContextSnapshot,
    ) -> BoxFuture<'_, Result<StatusOffering, Error>> {
        self.contexts.lock().unwrap().push(context);
        Box::pin(async move {
            let mut offering = StatusOffering::default();
            offering.insert(
                StatusKey::new("status"),
                StatusOffer {
                    hash: String::new(),
                    text: "fitted status".into(),
                    is_auto_update: false,
                },
            );
            Ok(offering)
        })
    }
}

pub(super) struct MemoryCompaction {
    pub(super) replies: VecDeque<Result<Option<Message>, Error>>,
    pub(super) requests: Vec<parrot_core::CompactionRequest>,
    pub(super) results: Vec<Result<Option<Message>, Error>>,
    pub(super) contexts: VecDeque<CompactedContext>,
    pub(super) saved_state: AgentState,
    pub(super) effect_state: Option<AgentState>,
    pub(super) status: MemoryStatus,
}

impl Default for MemoryCompaction {
    fn default() -> Self {
        Self {
            replies: VecDeque::new(),
            requests: Vec::new(),
            results: Vec::new(),
            contexts: VecDeque::new(),
            saved_state: AgentState::default(),
            effect_state: None,
            status: MemoryStatus {
                contexts: Mutex::default(),
            },
        }
    }
}

#[test]
fn conversation_reads_return_owned_data_without_requesting_compaction() {
    fn retain_owned<T: Send + Sync + 'static>(value: T) -> T {
        value
    }
    let state = MemoryState::new([]);
    state.request().messages = vec![Message {
        contents: vec![Content::Text("saved conversation".into())],
        ..Message::default()
    }];
    let compaction = state.compaction.clone();
    let input = retain_owned(finish(state.read_conversation_state()).unwrap());
    drop(state);

    assert_eq!(
        input.assemble_messages(),
        [Message::text(Role::User, "saved conversation")]
    );
    assert_eq!(
        input.history[0].message,
        Message::text(Role::User, "saved conversation")
    );
    assert_eq!(input.history[0].sequence, 1);
    assert_eq!(input.history[0].tool_owner_sequence, None);
    assert_eq!(input.summary, None);
    assert!(compaction.lock().unwrap().requests.is_empty());
}

#[test]
fn core_forwards_the_catalog_to_one_complete_provider_operation() {
    let mut state = MemoryState::new([
        Reply::TooLarge("oversized"),
        Reply::final_response("answer"),
    ]);
    state.compact_with("committed summary");

    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    let compaction = state.compaction.lock().unwrap();
    assert_eq!(compaction.requests.len(), 1);
    assert_eq!(compaction.requests[0].target, None);
    assert_eq!(
        compaction.requests[0]
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["read", "write"]
    );
    assert_eq!(state.provider.requests.lock().unwrap().len(), 2);
}

#[test]
fn opaque_replacement_reaches_the_next_request_without_interpretation() {
    let mut state = MemoryState::new([
        Reply::TooLarge("oversized"),
        Reply::final_response("answer"),
    ]);
    let replacement = Message {
        role: Role::Assistant,
        provider_id: "native-provider".into(),
        provider_content: "opaque continuation bytes".into(),
        ..Message::default()
    };
    let retained = Message {
        role: Role::User,
        contents: vec![Content::Text("retained tail".into())],
        ..Message::default()
    };
    let context = state.request().context.clone();
    state
        .compaction
        .lock()
        .unwrap()
        .replies
        .push_back(Ok(Some(replacement.clone())));
    state.compactions.push_back(CompactedContext {
        messages: vec![replacement.clone(), retained.clone()],
        state: context,
    });

    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    let requests = state.provider.requests.lock().unwrap();
    assert_eq!(&requests[1].messages[..2], [replacement, retained]);
    assert_eq!(state.compaction.lock().unwrap().requests.len(), 1);
}
