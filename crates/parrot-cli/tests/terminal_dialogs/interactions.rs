//! Controlled native transport witnesses for enhanced interaction orchestration.
//! Generic test services do not close the missing local production/security edge.

#[path = "../slash_sessions/native_fixture.rs"]
mod native_fixture;

use std::{
    collections::{BTreeMap, VecDeque},
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use native_fixture::{Fixture, RecordingDialog, Stop};
use parrot_cli::{
    dispatch::dialog::{DialogOption, SlashDialog},
    slash_sessions::{CallbackSessionBinding, RpcSlashSession},
    terminal_dialogs::interactions::{
        CountdownCallback, PendingQuestionLifetime, QuestionCollector, QuestionCompletionOutcome,
        QuestionCountdown, QuestionInteractionOptions, complete_permission, complete_plan,
        complete_question, complete_question_using, complete_question_with_options,
        complete_question_with_outcome,
    },
};
use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation},
    protocol::messages::parrot as wire,
};
use parrot_foundation::config::Configuration;
use protobuf::Message;
use tonic::Status;

fn plan() -> wire::PlanCompleted {
    wire::PlanCompleted {
        dialog: Some(wire::TurnCompleteDialog {
            prompt: "Plan complete: ".into(),
            choices: vec![wire::DialogChoice {
                value: "yes".into(),
                description: "Implement it".into(),
                action: Some(wire::ChoiceAction {
                    mode: "build".into(),
                    prompt: "Implement the approved plan.".into(),
                    ..Default::default()
                })
                .into(),
                ..Default::default()
            }],
            custom_choice: "feedback".into(),
            custom_prompt: "Feedback: ".into(),
            ..Default::default()
        })
        .into(),
        ..Default::default()
    }
}

fn session(fixture: &mut Fixture) -> RpcSlashSession {
    let path = fixture.root.join("config.yaml");
    std::fs::write(&path, "model: provider/current\n").unwrap();
    let config = Configuration::load(
        &path,
        &fixture.root.join("predefined.yaml"),
        &BTreeMap::from([("HOME".into(), fixture.root.to_string_lossy().into_owned())]),
        |_| Ok(()),
    )
    .unwrap();
    RpcSlashSession::new(
        fixture.client(),
        wire::UserSession {
            id: "captured-session".into(),
            mode: "plan".into(),
            ..Default::default()
        },
        Arc::new(Mutex::new(config)),
        true,
        Box::new(CallbackSessionBinding(Arc::new(|_, _| {
            Box::pin(async { Ok(()) })
        }))),
    )
}

#[tokio::test]
async fn plan_picker_changes_mode_before_queued_prompt_on_same_session() {
    let mut fixture = Fixture::start(|method, body| match method {
        "update_session" => {
            let request = wire::UpdateSessionRequest::parse_from_bytes(body).unwrap();
            assert_eq!(request.user_session_id, "captured-session");
            assert_eq!(request.mode, "build");
            Ok(wire::UserSession {
                id: request.user_session_id,
                mode: request.mode,
                ..Default::default()
            }
            .write_to_bytes()
            .unwrap())
        }
        "send_message" => Ok(wire::SendMessageResponse::new().write_to_bytes().unwrap()),
        _ => panic!("unexpected RPC {method}"),
    })
    .await;
    let mut session = session(&mut fixture);
    let mut dialog = RecordingDialog::default();
    dialog.selections.push_back(Some("yes".into()));
    assert!(
        complete_plan(
            fixture.client(),
            &plan(),
            &mut session,
            &mut dialog,
            &Stop::new()
        )
        .await
        .unwrap()
    );
    let calls = fixture.calls.lock().unwrap().clone();
    assert_eq!(
        calls.iter().map(|call| call.method).collect::<Vec<_>>(),
        ["update_session", "send_message"]
    );
    let sent = wire::SendMessageRequest::parse_from_bytes(&calls[1].body).unwrap();
    assert_eq!(sent.user_session_id, "captured-session");
    assert_eq!(sent.text, "Implement the approved plan.");
    assert_eq!(
        sent.delivery.enum_value().unwrap(),
        wire::Delivery::DELIVERY_QUEUE
    );
    assert_eq!(dialog.pickers[0].0, "Plan complete: ");
    assert_eq!(
        dialog.pickers[0].1[1].description,
        "Provide feedback and revise"
    );
    fixture.close().await;
}

#[tokio::test]
async fn plan_custom_feedback_is_trimmed_and_blank_or_cancellation_sends_nothing() {
    let mut fixture = Fixture::start(|method, _| match method {
        "send_message" => Ok(wire::SendMessageResponse::new().write_to_bytes().unwrap()),
        _ => panic!("unexpected RPC {method}"),
    })
    .await;
    let mut session = session(&mut fixture);
    for (feedback, expected) in [
        (Some("  revise this  "), true),
        (Some(" \n "), false),
        (None, false),
    ] {
        let mut dialog = RecordingDialog::default();
        dialog.selections.push_back(Some("feedback".into()));
        dialog.texts.push_back(feedback.map(str::to_owned));
        assert_eq!(
            complete_plan(
                fixture.client(),
                &plan(),
                &mut session,
                &mut dialog,
                &Stop::new()
            )
            .await
            .unwrap(),
            expected
        );
    }
    let calls = fixture.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    let sent = wire::SendMessageRequest::parse_from_bytes(&calls[0].body).unwrap();
    assert_eq!(sent.text, "revise this");
    assert_eq!(sent.user_session_id, "captured-session");
    assert_eq!(
        sent.delivery.enum_value().unwrap(),
        wire::Delivery::DELIVERY_QUEUE
    );
    fixture.close().await;
}

#[tokio::test]
async fn plan_missing_dialog_empty_choices_cancel_and_rpc_status_remain_distinct() {
    let mut fixture =
        Fixture::start(|_, _| Err(Status::permission_denied("controlled denial"))).await;
    let mut session = session(&mut fixture);
    let mut dialog = RecordingDialog::default();
    assert!(
        !complete_plan(
            fixture.client(),
            &wire::PlanCompleted::new(),
            &mut session,
            &mut dialog,
            &Stop::new()
        )
        .await
        .unwrap()
    );
    let empty = wire::PlanCompleted {
        dialog: Some(wire::TurnCompleteDialog::new()).into(),
        ..Default::default()
    };
    assert!(
        !complete_plan(
            fixture.client(),
            &empty,
            &mut session,
            &mut dialog,
            &Stop::new()
        )
        .await
        .unwrap()
    );
    assert!(
        !complete_plan(
            fixture.client(),
            &plan(),
            &mut session,
            &mut dialog,
            &Stop::new()
        )
        .await
        .unwrap()
    );
    assert!(fixture.calls.lock().unwrap().is_empty());
    dialog.selections.push_back(Some("yes".into()));
    let error = complete_plan(
        fixture.client(),
        &plan(),
        &mut session,
        &mut dialog,
        &Stop::new(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(error, parrot_cli::slash_sessions::SlashFailure::Rpc(status) if status.code() == tonic::Code::PermissionDenied && status.message() == "controlled denial")
    );
    fixture.close().await;
}

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("bounded interaction did not settle")
}

fn options() -> QuestionInteractionOptions {
    QuestionInteractionOptions {
        reconciliation_interval: Duration::from_millis(1),
        ..Default::default()
    }
}

fn countdown() -> (CountdownCallback, Arc<Mutex<Vec<Option<i64>>>>) {
    let updates = Arc::new(Mutex::new(Vec::new()));
    let observed = updates.clone();
    (
        Arc::new(move |seconds, _| {
            observed.lock().unwrap().push(seconds);
            Box::pin(async { Ok(()) })
        }),
        updates,
    )
}

#[derive(Default)]
struct QuestionState {
    pending: Vec<wire::PendingQuestion>,
    listing_failure: bool,
    reply_failures: VecDeque<Status>,
    reject_failure: Option<Status>,
}

async fn question_fixture(
    pending: Vec<wire::PendingQuestion>,
) -> (Fixture, Arc<Mutex<QuestionState>>) {
    let state = Arc::new(Mutex::new(QuestionState {
        pending,
        ..Default::default()
    }));
    let observed = state.clone();
    let fixture = Fixture::start(move |method, _| {
        let mut state = observed.lock().unwrap();
        match method {
            "list_pending_questions" if state.listing_failure => {
                Err(Status::unavailable("controlled listing failure"))
            }
            "list_pending_questions" => Ok(wire::ListPendingQuestionsResponse {
                questions: state.pending.clone(),
                ..Default::default()
            }
            .write_to_bytes()
            .unwrap()),
            "reply_question" => match state.reply_failures.pop_front() {
                Some(failure) => Err(failure),
                None => Ok(wire::ReplyQuestionResponse::new().write_to_bytes().unwrap()),
            },
            "reject_question" => match state.reject_failure.take() {
                Some(failure) => Err(failure),
                None => Ok(wire::RejectQuestionResponse::new()
                    .write_to_bytes()
                    .unwrap()),
            },
            _ => panic!("unexpected question RPC {method}"),
        }
    })
    .await;
    (fixture, state)
}

fn pending(id: &str) -> wire::PendingQuestion {
    wire::PendingQuestion {
        id: id.into(),
        questions: vec![wire::QuestionDefinition {
            header: "Question".into(),
            prompt: "Pick a colour".into(),
            options: vec![wire::QuestionOption {
                label: "Blue".into(),
                description: "A colour".into(),
                ..Default::default()
            }],
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn replies(fixture: &Fixture) -> Vec<wire::ReplyQuestionRequest> {
    fixture
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|call| call.method == "reply_question")
        .map(|call| wire::ReplyQuestionRequest::parse_from_bytes(&call.body).unwrap())
        .collect()
}

fn rejections(fixture: &Fixture) -> Vec<wire::RejectQuestionRequest> {
    fixture
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|call| call.method == "reject_question")
        .map(|call| wire::RejectQuestionRequest::parse_from_bytes(&call.body).unwrap())
        .collect()
}

#[test]
fn countdown_rounds_clamps_refreshes_and_omits_unknown_timing_source_rows() {
    let now = Instant::now();
    assert_eq!(
        QuestionInteractionOptions::default().reconciliation_interval,
        Duration::from_millis(250)
    );
    for (milliseconds, first, after_one_second) in
        [(1001, 2, 1), (60001, 61, 60), (0, 0, 0), (-1, 0, 0)]
    {
        let mut countdown = QuestionCountdown::new(now);
        assert_eq!(countdown.remaining_seconds(now), None);
        countdown.update(Some(milliseconds), now);
        assert_eq!(countdown.remaining_seconds(now), Some(first));
        assert_eq!(
            countdown.remaining_seconds(now + Duration::from_secs(1)),
            Some(after_one_second)
        );
        assert_eq!(
            countdown.remaining_seconds(now + Duration::from_secs(3600)),
            Some(0)
        );
        countdown.update(Some(1500), now);
        assert_eq!(countdown.remaining_seconds(now), Some(2));
        countdown.update(None, now);
        assert_eq!(countdown.remaining_seconds(now), None);
    }
}

#[tokio::test]
async fn lifetime_extrapolates_during_rpc_failure_and_only_server_absence_closes() {
    let mut question = pending("question");
    question.remaining_timeout_ms = Some(1500);
    let (mut fixture, state) = question_fixture(vec![question]).await;
    let (updates, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let callback: CountdownCallback = Arc::new(move |seconds, _| {
        updates.send(seconds).unwrap();
        Box::pin(async { Ok(()) })
    });
    let elapsed = Arc::new(AtomicU64::new(0));
    let clock = elapsed.clone();
    let epoch = Instant::now();
    let lifetime = PendingQuestionLifetime::new(
        fixture.client(),
        "captured-session".into(),
        "question".into(),
        QuestionInteractionOptions {
            reconciliation_interval: Duration::from_millis(1),
            now: Arc::new(move || epoch + Duration::from_millis(clock.load(Ordering::SeqCst))),
        },
        callback,
    );
    let stop = Stop::new();
    bounded(async {
        let running = lifetime.run(&stop);
        let checking = async {
            lifetime.wait_until_ready(&stop).await.unwrap();
            assert_eq!(receiver.recv().await.unwrap(), Some(2));
            state.lock().unwrap().listing_failure = true;
            elapsed.store(1000, Ordering::SeqCst);
            assert_eq!(receiver.recv().await.unwrap(), Some(1));
            elapsed.store(2000, Ordering::SeqCst);
            assert_eq!(receiver.recv().await.unwrap(), Some(0));
            assert!(!lifetime.is_closed());
            assert!(replies(&fixture).is_empty());
            assert!(rejections(&fixture).is_empty());
            {
                let mut state = state.lock().unwrap();
                state.pending[0].remaining_timeout_ms = None;
                state.listing_failure = false;
            }
            assert_eq!(receiver.recv().await.unwrap(), None);
            state.lock().unwrap().pending.clear();
        };
        let (result, ()) = tokio::join!(running, checking);
        result.unwrap();
    })
    .await;
    assert!(lifetime.is_closed());
    assert!(receiver.try_recv().is_err());
    for call in fixture
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|call| call.method == "list_pending_questions")
    {
        assert_eq!(
            wire::ListPendingQuestionsRequest::parse_from_bytes(&call.body)
                .unwrap()
                .user_session_id,
            "captured-session"
        );
    }
    fixture.close().await;
}

#[tokio::test]
async fn fresh_reconciliation_handles_unknown_or_already_closed_requests_source_rows() {
    for present in [false, true] {
        let (mut fixture, _) = question_fixture(if present {
            vec![pending("question")]
        } else {
            vec![]
        })
        .await;
        let (callback, updates) = countdown();
        let lifetime = PendingQuestionLifetime::new(
            fixture.client(),
            "captured-session".into(),
            "question".into(),
            options(),
            callback,
        );
        let stop = Stop::new();
        bounded(async {
            let running = lifetime.run(&stop);
            let checking = async {
                lifetime.wait_until_ready(&stop).await.unwrap();
                assert_eq!(lifetime.is_closed(), !present);
                stop.cancel();
            };
            let (result, ()) = tokio::join!(running, checking);
            result.unwrap();
        })
        .await;
        assert!(updates.lock().unwrap().is_empty());
        fixture.close().await;
    }
}

#[tokio::test]
async fn single_question_uses_captured_session_fresh_countdown_and_clears_after_reply() {
    let mut snapshot = pending("question-request");
    snapshot.remaining_timeout_ms = Some(90000);
    let mut fresh = snapshot.clone();
    fresh.remaining_timeout_ms = Some(4000);
    let (mut fixture, _) = question_fixture(vec![fresh]).await;
    let mut dialog = RecordingDialog::default();
    dialog.selections.push_back(Some("__option_0__".into()));
    let (callback, updates) = countdown();
    bounded(complete_question_with_options(
        fixture.client(),
        "captured-session",
        &snapshot,
        &mut dialog,
        callback,
        options(),
        &Stop::new(),
    ))
    .await
    .unwrap();
    let reply = replies(&fixture);
    assert_eq!(reply.len(), 1);
    assert_eq!(reply[0].user_session_id, "captured-session");
    assert_eq!(reply[0].question_request_id, "question-request");
    assert_eq!(reply[0].answers[0].text, "Blue");
    assert_eq!(dialog.pickers[0].0, "Question\nPick a colour");
    assert_eq!(dialog.pickers[0].1[0].description, "A colour");
    assert_eq!(*updates.lock().unwrap(), [Some(4), None]);
    fixture.close().await;
}

#[tokio::test]
async fn multiple_choice_replies_in_source_option_order_with_or_without_timing() {
    for timed in [false, true] {
        let mut question = pending("question-request");
        question.questions[0].multiple = true;
        question.questions[0].options = ["One", "Two", "Three"]
            .into_iter()
            .map(|label| wire::QuestionOption {
                label: label.into(),
                ..Default::default()
            })
            .collect();
        question.remaining_timeout_ms = timed.then_some(3000);
        let (mut fixture, _) = question_fixture(vec![question.clone()]).await;
        let mut dialog = RecordingDialog::default();
        dialog.selections.extend([
            Some("__option_1__".into()),
            Some("__option_0__".into()),
            Some("__done__".into()),
        ]);
        let (callback, updates) = countdown();
        bounded(complete_question_with_options(
            fixture.client(),
            "captured-session",
            &question,
            &mut dialog,
            callback,
            options(),
            &Stop::new(),
        ))
        .await
        .unwrap();
        assert_eq!(replies(&fixture)[0].answers[0].text, "One, Two");
        assert_eq!(dialog.pickers.len(), 3);
        assert!(
            !dialog.pickers[1]
                .1
                .iter()
                .any(|option| option.id == "__option_1__")
        );
        assert!(
            dialog.pickers[1]
                .1
                .iter()
                .any(|option| option.id == "__done__")
        );
        assert_eq!(updates.lock().unwrap().last(), Some(&None));
        fixture.close().await;
    }
}

#[tokio::test]
async fn custom_question_context_and_optionless_answers_continue_in_order_source_rows() {
    for multiple in [false, true] {
        for optionless in [false, true] {
            let mut question = pending("question-request");
            question.questions[0].header = "Answer context".into();
            question.questions[0].prompt = "Explain your choice".into();
            question.questions[0].custom = true;
            question.questions[0].multiple = multiple;
            if optionless {
                question.questions[0].options.clear();
            }
            question.questions.push(wire::QuestionDefinition {
                prompt: "Next answer".into(),
                custom: true,
                ..Default::default()
            });
            let (mut fixture, _) = question_fixture(vec![question.clone()]).await;
            let mut dialog = RecordingDialog::default();
            if !optionless {
                dialog.selections.push_back(Some("__custom__".into()));
                if multiple {
                    dialog.selections.push_back(Some("__done__".into()));
                }
            }
            dialog.texts.extend([
                Some("  typed answer  ".into()),
                Some("second answer".into()),
            ]);
            let (callback, _) = countdown();
            bounded(complete_question_with_options(
                fixture.client(),
                "captured-session",
                &question,
                &mut dialog,
                callback,
                options(),
                &Stop::new(),
            ))
            .await
            .unwrap();
            let reply = replies(&fixture);
            assert_eq!(
                reply[0]
                    .answers
                    .iter()
                    .map(|answer| answer.text.as_str())
                    .collect::<Vec<_>>(),
                ["typed answer", "second answer"]
            );
            assert!(rejections(&fixture).is_empty());
            if optionless {
                assert!(dialog.pickers.is_empty());
            } else {
                assert_eq!(dialog.pickers[0].0, "Answer context\nExplain your choice");
            }
            fixture.close().await;
        }
    }
}

#[tokio::test]
async fn picker_cancel_and_empty_or_cancelled_custom_answers_reject_without_reply() {
    for custom in [false, true] {
        for answer in [None, Some(" \n ")] {
            let mut question = pending("question-request");
            let mut dialog = RecordingDialog::default();
            if custom {
                question.questions[0].custom = true;
                question.questions[0].options.clear();
                dialog.texts.push_back(answer.map(str::to_owned));
            }
            let (mut fixture, _) = question_fixture(vec![question.clone()]).await;
            let (callback, updates) = countdown();
            bounded(complete_question_with_options(
                fixture.client(),
                "captured-session",
                &question,
                &mut dialog,
                callback,
                options(),
                &Stop::new(),
            ))
            .await
            .unwrap();
            let rejected = rejections(&fixture);
            assert_eq!(rejected.len(), 1);
            assert_eq!(rejected[0].user_session_id, "captured-session");
            assert_eq!(rejected[0].question_request_id, "question-request");
            assert!(replies(&fixture).is_empty());
            assert_eq!(updates.lock().unwrap().last(), Some(&None));
            fixture.close().await;
        }
    }
}

struct GateDialog {
    inner: RecordingDialog,
    block_select: usize,
    block_read: bool,
    selections: usize,
    arrived: tokio::sync::mpsc::UnboundedSender<String>,
    releases: tokio::sync::mpsc::UnboundedReceiver<Option<String>>,
    dropped: Arc<AtomicUsize>,
}

struct DroppedRead(Arc<AtomicUsize>);
impl Drop for DroppedRead {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn gate_dialog(
    block_select: usize,
    block_read: bool,
) -> (
    GateDialog,
    tokio::sync::mpsc::UnboundedReceiver<String>,
    tokio::sync::mpsc::UnboundedSender<Option<String>>,
    Arc<AtomicUsize>,
) {
    let (arrived, receiver) = tokio::sync::mpsc::unbounded_channel();
    let (release, releases) = tokio::sync::mpsc::unbounded_channel();
    let dropped = Arc::new(AtomicUsize::new(0));
    (
        GateDialog {
            inner: RecordingDialog::default(),
            block_select,
            block_read,
            selections: 0,
            arrived,
            releases,
            dropped: dropped.clone(),
        },
        receiver,
        release,
        dropped,
    )
}

impl SlashDialog for GateDialog {
    fn select<'a>(
        &'a mut self,
        title: &'a str,
        options: &'a [DialogOption],
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<DialogOption>, Status>> {
        Box::pin(async move {
            self.selections += 1;
            if self.selections != self.block_select {
                return self.inner.select(title, options, c).await;
            }
            self.inner
                .pickers
                .push((title.to_owned(), options.to_vec()));
            self.arrived.send(title.to_owned()).unwrap();
            let _read = DroppedRead(self.dropped.clone());
            tokio::select! {
                biased;
                () = c.cancelled() => Err(Status::cancelled("controlled dialog cancelled")),
                released = self.releases.recv() => Ok(released.flatten().and_then(|id| options.iter().find(|option| option.id == id).cloned())),
            }
        })
    }
    fn read_text<'a>(
        &'a mut self,
        prompt: &'a str,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<String>, Status>> {
        if !self.block_read {
            return self.inner.read_text(prompt, c);
        }
        Box::pin(async move {
            self.arrived.send(prompt.to_owned()).unwrap();
            let _read = DroppedRead(self.dropped.clone());
            tokio::select! {
                biased;
                () = c.cancelled() => Err(Status::cancelled("controlled dialog cancelled")),
                released = self.releases.recv() => Ok(released.flatten()),
            }
        })
    }
    fn read_secret<'a>(
        &'a mut self,
        prompt: &'a str,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<String>, Status>> {
        self.inner.read_secret(prompt, c)
    }
    fn confirm<'a>(
        &'a mut self,
        lines: &'a [String],
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<bool, Status>> {
        self.inner.confirm(lines, c)
    }
    fn show<'a>(
        &'a mut self,
        lines: &'a [String],
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Status>> {
        self.inner.show(lines, c)
    }
    fn print<'a>(
        &'a mut self,
        lines: &'a [String],
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Status>> {
        self.inner.print(lines, c)
    }
    fn show_error<'a>(
        &'a mut self,
        message: &'a str,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Status>> {
        self.inner.show_error(message, c)
    }
    fn load<'a>(
        &'a mut self,
        activity: &'a str,
        work: BoxFuture<'a, Result<(), Status>>,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Status>> {
        self.inner.load(activity, work, c)
    }
}

#[tokio::test]
async fn remotely_settled_picker_or_partial_multi_part_question_discards_answers() {
    for multiple_parts in [false, true] {
        let mut question = pending("question-request");
        if multiple_parts {
            question.questions.push(wire::QuestionDefinition {
                prompt: "Second choice".into(),
                options: question.questions[0].options.clone(),
                ..Default::default()
            });
        }
        let (mut fixture, state) = question_fixture(vec![question.clone()]).await;
        let (mut dialog, mut arrived, _release, dropped) =
            gate_dialog(if multiple_parts { 2 } else { 1 }, false);
        if multiple_parts {
            dialog
                .inner
                .selections
                .push_back(Some("__option_0__".into()));
        }
        let (callback, updates) = countdown();
        let stop = Stop::new();
        bounded(async {
            let interacting = complete_question_with_options(
                fixture.client(),
                "captured-session",
                &question,
                &mut dialog,
                callback,
                options(),
                &stop,
            );
            let settling = async {
                let title = arrived.recv().await.unwrap();
                assert_eq!(
                    title,
                    if multiple_parts {
                        "Second choice"
                    } else {
                        "Question\nPick a colour"
                    }
                );
                state.lock().unwrap().pending.clear();
            };
            let (result, ()) = tokio::join!(interacting, settling);
            result.unwrap();
        })
        .await;
        assert!(replies(&fixture).is_empty());
        assert!(rejections(&fixture).is_empty());
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert_eq!(updates.lock().unwrap().last(), Some(&None));
        fixture.close().await;
    }
}

#[tokio::test]
async fn remotely_settled_optionless_custom_answers_do_not_submit_source_rows() {
    for multiple in [false, true] {
        let mut question = pending("question-request");
        question.questions[0].options.clear();
        question.questions[0].custom = true;
        question.questions[0].multiple = multiple;
        let (mut fixture, state) = question_fixture(vec![question.clone()]).await;
        let (mut dialog, mut arrived, _release, dropped) = gate_dialog(usize::MAX, true);
        let (callback, _) = countdown();
        let stop = Stop::new();
        bounded(async {
            let interacting = complete_question_with_options(
                fixture.client(),
                "captured-session",
                &question,
                &mut dialog,
                callback,
                options(),
                &stop,
            );
            let settling = async {
                assert_eq!(arrived.recv().await.unwrap(), "Question\nPick a colour");
                state.lock().unwrap().pending.clear();
            };
            let (result, ()) = tokio::join!(interacting, settling);
            result.unwrap();
        })
        .await;
        assert!(dialog.inner.pickers.is_empty());
        assert!(replies(&fixture).is_empty());
        assert!(rejections(&fixture).is_empty());
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        fixture.close().await;
    }
}

#[tokio::test]
async fn another_request_settlement_does_not_close_active_question_or_change_identity() {
    let active = pending("question-a");
    let (mut fixture, state) = question_fixture(vec![active.clone(), pending("question-b")]).await;
    let (mut dialog, mut arrived, release, _) = gate_dialog(1, false);
    let (callback, _) = countdown();
    let stop = Stop::new();
    bounded(async {
        let interacting = complete_question_with_options(
            fixture.client(),
            "session-a",
            &active,
            &mut dialog,
            callback,
            options(),
            &stop,
        );
        let settling = async {
            arrived.recv().await.unwrap();
            state
                .lock()
                .unwrap()
                .pending
                .retain(|question| question.id == "question-a");
            tokio::time::sleep(Duration::from_millis(10)).await;
            release.send(Some("__option_0__".into())).unwrap();
        };
        let (result, ()) = tokio::join!(interacting, settling);
        result.unwrap();
    })
    .await;
    let sent = replies(&fixture);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].user_session_id, "session-a");
    assert_eq!(sent[0].question_request_id, "question-a");
    assert!(rejections(&fixture).is_empty());
    fixture.close().await;
}

#[tokio::test]
async fn zero_countdown_remains_pending_until_user_answer_and_then_clears() {
    let mut question = pending("question-request");
    question.remaining_timeout_ms = Some(0);
    let (mut fixture, _) = question_fixture(vec![question.clone()]).await;
    let (mut dialog, mut arrived, release, _) = gate_dialog(1, false);
    let (callback, updates) = countdown();
    let stop = Stop::new();
    bounded(async {
        let interacting = complete_question_with_options(
            fixture.client(),
            "captured-session",
            &question,
            &mut dialog,
            callback,
            options(),
            &stop,
        );
        let answering = async {
            arrived.recv().await.unwrap();
            assert!(replies(&fixture).is_empty());
            assert!(rejections(&fixture).is_empty());
            assert_eq!(updates.lock().unwrap().first(), Some(&Some(0)));
            release.send(Some("__option_0__".into())).unwrap();
        };
        let (result, ()) = tokio::join!(interacting, answering);
        result.unwrap();
    })
    .await;
    assert_eq!(replies(&fixture).len(), 1);
    assert_eq!(*updates.lock().unwrap(), [Some(0), None]);
    fixture.close().await;
}

#[tokio::test]
async fn scope_cancellation_joins_monitor_clears_countdown_and_never_replies_or_rejects() {
    let question = pending("question-request");
    let (mut fixture, _) = question_fixture(vec![question.clone()]).await;
    let (mut dialog, mut arrived, _release, dropped) = gate_dialog(1, false);
    let (callback, updates) = countdown();
    let stop = Stop::new();
    bounded(async {
        let interacting = complete_question_with_options(fixture.client(), "old-session", &question, &mut dialog, callback, options(), &stop);
        let switching = async { arrived.recv().await.unwrap(); stop.cancel(); };
        let (result, ()) = tokio::join!(interacting, switching);
        assert!(matches!(result, Err(parrot_cli::slash_sessions::SlashFailure::Rpc(status)) if status.code() == tonic::Code::Cancelled));
    }).await;
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert!(replies(&fixture).is_empty());
    assert!(rejections(&fixture).is_empty());
    assert_eq!(updates.lock().unwrap().last(), Some(&None));
    let listed = fixture.calls.lock().unwrap().len();
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(
        fixture.calls.lock().unwrap().len(),
        listed,
        "reconciliation must not remain detached"
    );
    fixture.close().await;
}

#[tokio::test]
async fn closed_before_ready_never_opens_dialog_and_failed_listing_retries_before_ready() {
    let question = pending("question-request");
    let (mut fixture, state) = question_fixture(vec![]).await;
    let mut dialog = RecordingDialog::default();
    let (callback, updates) = countdown();
    bounded(complete_question_with_options(
        fixture.client(),
        "captured-session",
        &question,
        &mut dialog,
        callback,
        options(),
        &Stop::new(),
    ))
    .await
    .unwrap();
    assert!(dialog.pickers.is_empty());
    assert!(replies(&fixture).is_empty());
    assert!(rejections(&fixture).is_empty());
    assert_eq!(*updates.lock().unwrap(), [None]);
    {
        let mut state = state.lock().unwrap();
        state.pending.push(question.clone());
        state.listing_failure = true;
    }
    dialog.selections.push_back(Some("__option_0__".into()));
    let (callback, _) = countdown();
    let calls = fixture.calls.clone();
    let stop = Stop::new();
    bounded(async {
        let interacting = complete_question_with_options(
            fixture.client(),
            "captured-session",
            &question,
            &mut dialog,
            callback,
            options(),
            &stop,
        );
        let recovering = async {
            while calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| call.method == "list_pending_questions")
                .count()
                < 2
            {
                tokio::task::yield_now().await;
            }
            state.lock().unwrap().listing_failure = false;
        };
        let (result, ()) = tokio::join!(interacting, recovering);
        result.unwrap();
    })
    .await;
    assert_eq!(replies(&fixture).len(), 1);
    fixture.close().await;
}

#[tokio::test]
async fn question_rpc_not_found_is_benign_invalid_argument_shown_and_other_status_preserved() {
    for code in [
        tonic::Code::NotFound,
        tonic::Code::InvalidArgument,
        tonic::Code::PermissionDenied,
    ] {
        let question = pending("question-request");
        let (mut fixture, state) = question_fixture(vec![question.clone()]).await;
        state
            .lock()
            .unwrap()
            .reply_failures
            .push_back(Status::new(code, "controlled reply failure"));
        let mut dialog = RecordingDialog::default();
        dialog.selections.push_back(Some("__option_0__".into()));
        let (callback, updates) = countdown();
        let result = bounded(complete_question_with_options(
            fixture.client(),
            "captured-session",
            &question,
            &mut dialog,
            callback,
            options(),
            &Stop::new(),
        ))
        .await;
        if code == tonic::Code::PermissionDenied {
            assert!(
                matches!(result, Err(parrot_cli::slash_sessions::SlashFailure::Rpc(status)) if status.code() == code)
            );
        } else {
            result.unwrap();
        }
        assert_eq!(
            dialog.errors,
            if code == tonic::Code::InvalidArgument {
                vec!["controlled reply failure"]
            } else {
                vec![]
            }
        );
        assert_eq!(updates.lock().unwrap().last(), Some(&None));
        fixture.close().await;
    }
}

#[tokio::test]
async fn outcome_allows_caller_retry_of_same_question_while_legacy_helper_stays_enhanced() {
    let mut question = pending("question-request");
    question.questions[0].options.push(wire::QuestionOption {
        label: "Green".into(),
        ..Default::default()
    });
    question.remaining_timeout_ms = Some(4000);
    let (mut fixture, state) = question_fixture(vec![question.clone()]).await;
    state
        .lock()
        .unwrap()
        .reply_failures
        .push_back(Status::invalid_argument("choose another answer"));
    let mut dialog = RecordingDialog::default();
    dialog
        .selections
        .extend([Some("__option_0__".into()), Some("__option_1__".into())]);
    let (callback, updates) = countdown();
    let stop = Stop::new();
    let first = bounded(complete_question_with_outcome(
        fixture.client(),
        "captured-session",
        &question,
        &mut dialog,
        callback.clone(),
        &stop,
    ))
    .await
    .unwrap();
    assert_eq!(
        first,
        QuestionCompletionOutcome::RetryInvalidArgument("choose another answer".into())
    );
    assert!(dialog.errors.is_empty());
    assert_eq!(
        replies(&fixture).len(),
        1,
        "the helper must not own the caller's retry loop"
    );
    let second = bounded(complete_question_with_outcome(
        fixture.client(),
        "captured-session",
        &question,
        &mut dialog,
        callback,
        &stop,
    ))
    .await
    .unwrap();
    assert_eq!(second, QuestionCompletionOutcome::Finished);
    assert!(dialog.errors.is_empty());
    let sent = replies(&fixture);
    assert_eq!(sent.len(), 2);
    for reply in &sent {
        assert_eq!(reply.user_session_id, "captured-session");
        assert_eq!(reply.question_request_id, "question-request");
        assert_eq!(reply.answers.len(), 1);
    }
    assert_eq!(sent[0].answers[0].text, "Blue");
    assert_eq!(sent[1].answers[0].text, "Green");
    assert_eq!(*updates.lock().unwrap(), [Some(4), None, Some(4), None]);
    assert!(rejections(&fixture).is_empty());
    fixture.close().await;

    let (mut legacy_fixture, state) = question_fixture(vec![question.clone()]).await;
    state
        .lock()
        .unwrap()
        .reply_failures
        .push_back(Status::invalid_argument("choose another answer"));
    let mut legacy_dialog = RecordingDialog::default();
    legacy_dialog
        .selections
        .push_back(Some("__option_0__".into()));
    let (callback, updates) = countdown();
    bounded(complete_question(
        legacy_fixture.client(),
        "captured-session",
        &question,
        &mut legacy_dialog,
        callback,
        &stop,
    ))
    .await
    .unwrap();
    assert_eq!(legacy_dialog.errors, ["choose another answer"]);
    assert_eq!(
        replies(&legacy_fixture).len(),
        1,
        "Enhanced returns after displaying the error"
    );
    assert_eq!(*updates.lock().unwrap(), [Some(4), None]);
    legacy_fixture.close().await;
}

#[tokio::test]
async fn supplied_collector_preserves_raw_answers_and_owns_retry_without_error_side_effect() {
    let question = pending("question-request");
    let (mut fixture, state) = question_fixture(vec![question.clone()]).await;
    state
        .lock()
        .unwrap()
        .reply_failures
        .push_back(Status::invalid_argument("raw answer rejected"));
    let collected = Arc::new(AtomicUsize::new(0));
    let observed = collected.clone();
    let collector: QuestionCollector = Arc::new(move |pending, dialog, cancellation| {
        observed.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            assert_eq!(pending.id, "question-request");
            let answer = dialog
                .read_text("answer (or /cancel)", cancellation)
                .await?;
            Ok(answer.map(|text| {
                vec![wire::QuestionAnswer {
                    text,
                    ..Default::default()
                }]
            }))
        })
    });
    let mut dialog = RecordingDialog::default();
    dialog.texts.extend([
        Some("  raw blue / slash  ".into()),
        Some(" raw green\ncontinued  ".into()),
    ]);
    let (callback, updates) = countdown();
    let stop = Stop::new();
    let first = bounded(complete_question_using(
        fixture.client(),
        "captured-session",
        &question,
        &mut dialog,
        callback.clone(),
        collector.clone(),
        &stop,
    ))
    .await
    .unwrap();
    assert_eq!(
        first,
        QuestionCompletionOutcome::RetryInvalidArgument("raw answer rejected".into())
    );
    assert!(
        dialog.errors.is_empty(),
        "the frontend owns formatting of returned status detail"
    );
    assert_eq!(collected.load(Ordering::SeqCst), 1);
    assert_eq!(replies(&fixture).len(), 1);
    let second = bounded(complete_question_using(
        fixture.client(),
        "captured-session",
        &question,
        &mut dialog,
        callback,
        collector,
        &stop,
    ))
    .await
    .unwrap();
    assert_eq!(second, QuestionCompletionOutcome::Finished);
    assert_eq!(collected.load(Ordering::SeqCst), 2);
    let sent = replies(&fixture);
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].answers[0].text, "  raw blue / slash  ");
    assert_eq!(sent[1].answers[0].text, " raw green\ncontinued  ");
    for reply in sent {
        assert_eq!(reply.user_session_id, "captured-session");
        assert_eq!(reply.question_request_id, "question-request");
    }
    assert!(
        dialog.pickers.is_empty(),
        "the supplied collector must not invoke Enhanced selection"
    );
    assert!(dialog.errors.is_empty());
    assert!(rejections(&fixture).is_empty());
    assert_eq!(*updates.lock().unwrap(), [None, None]);
    fixture.close().await;
}

#[tokio::test]
async fn supplied_collector_starts_only_after_readiness_and_none_rejects_same_request() {
    let question = pending("question-request");
    let (mut fixture, state) = question_fixture(vec![question.clone()]).await;
    let collected = Arc::new(AtomicUsize::new(0));
    let observed = collected.clone();
    let collector: QuestionCollector = Arc::new(move |_, _, _| {
        observed.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(None) })
    });
    let mut dialog = RecordingDialog::default();
    let (callback, _) = countdown();
    assert_eq!(
        bounded(complete_question_using(
            fixture.client(),
            "captured-session",
            &question,
            &mut dialog,
            callback,
            collector.clone(),
            &Stop::new(),
        ))
        .await
        .unwrap(),
        QuestionCompletionOutcome::Finished
    );
    assert_eq!(collected.load(Ordering::SeqCst), 1);
    let rejected = rejections(&fixture);
    assert_eq!(rejected.len(), 1);
    assert_eq!(rejected[0].user_session_id, "captured-session");
    assert_eq!(rejected[0].question_request_id, "question-request");
    assert!(replies(&fixture).is_empty());
    state.lock().unwrap().pending.clear();
    let (callback, _) = countdown();
    assert_eq!(
        bounded(complete_question_using(
            fixture.client(),
            "captured-session",
            &question,
            &mut dialog,
            callback,
            collector,
            &Stop::new(),
        ))
        .await
        .unwrap(),
        QuestionCompletionOutcome::Finished
    );
    assert_eq!(
        collected.load(Ordering::SeqCst),
        1,
        "already closed requests must not invoke the collector"
    );
    assert_eq!(rejections(&fixture).len(), 1);
    fixture.close().await;
}

#[tokio::test]
async fn rejection_not_found_is_benign_and_prior_cancellation_sends_nothing() {
    let question = pending("question-request");
    let (mut fixture, state) = question_fixture(vec![question.clone()]).await;
    state.lock().unwrap().reject_failure = Some(Status::not_found("already closed"));
    let mut dialog = RecordingDialog::default();
    let (callback, _) = countdown();
    bounded(complete_question_with_options(
        fixture.client(),
        "captured-session",
        &question,
        &mut dialog,
        callback,
        options(),
        &Stop::new(),
    ))
    .await
    .unwrap();
    assert_eq!(rejections(&fixture).len(), 1);
    assert!(dialog.errors.is_empty());
    let before = fixture.calls.lock().unwrap().len();
    let stop = Stop::new();
    stop.cancel();
    let (callback, updates) = countdown();
    let result = bounded(complete_question_with_options(
        fixture.client(),
        "another-session",
        &question,
        &mut dialog,
        callback,
        options(),
        &stop,
    ))
    .await;
    assert!(
        matches!(result, Err(parrot_cli::slash_sessions::SlashFailure::Rpc(status)) if status.code() == tonic::Code::Cancelled)
    );
    assert_eq!(fixture.calls.lock().unwrap().len(), before);
    assert_eq!(*updates.lock().unwrap(), [None]);
    fixture.close().await;
}

#[tokio::test]
async fn countdown_callback_failure_is_joined_and_cleanup_runs_without_cancelled_token() {
    let mut question = pending("question-request");
    question.remaining_timeout_ms = Some(1500);
    let (mut fixture, _) = question_fixture(vec![question.clone()]).await;
    let mut dialog = RecordingDialog::default();
    let updates = Arc::new(Mutex::new(Vec::new()));
    let observed = updates.clone();
    let callback: CountdownCallback = Arc::new(move |seconds, c| {
        assert!(!c.is_cancelled());
        observed.lock().unwrap().push(seconds);
        Box::pin(async move {
            if seconds.is_some() {
                Err(Status::internal("controlled countdown failure").into())
            } else {
                Ok(())
            }
        })
    });
    let result = bounded(complete_question_with_options(
        fixture.client(),
        "captured-session",
        &question,
        &mut dialog,
        callback,
        options(),
        &Stop::new(),
    ))
    .await;
    assert!(
        matches!(result, Err(parrot_cli::slash_sessions::SlashFailure::Rpc(status)) if status.code() == tonic::Code::Internal && status.message() == "controlled countdown failure")
    );
    assert_eq!(*updates.lock().unwrap(), [Some(2), None]);
    assert!(dialog.pickers.is_empty());
    assert!(replies(&fixture).is_empty());
    assert!(rejections(&fixture).is_empty());
    fixture.close().await;
}

fn permission() -> wire::PendingPermission {
    wire::PendingPermission {
        id: "permission-request".into(),
        reason: "Write generated output".into(),
        targets: vec![wire::PermissionTarget {
            scope: wire::PermissionTargetScope::PERMISSION_TARGET_SCOPE_WRITE.into(),
            kind: wire::PermissionTargetKind::PERMISSION_TARGET_KIND_FILE.into(),
            path: "/synthetic/output".into(),
            ..Default::default()
        }],
        choices: vec![
            wire::PermissionChoice {
                value: "allow".into(),
                label: "Allow".into(),
                action: wire::PermissionAction::PERMISSION_ACTION_ALLOW.into(),
                requires_reason: true,
                ..Default::default()
            },
            wire::PermissionChoice {
                value: "deny".into(),
                label: "Reject".into(),
                action: wire::PermissionAction::PERMISSION_ACTION_DENY.into(),
                ..Default::default()
            },
        ],
        ..Default::default()
    }
}

#[tokio::test]
async fn permission_reason_is_required_trimmed_and_reply_uses_captured_identity() {
    let mut fixture = Fixture::start(|method, _| match method {
        "reply_permission" => Ok(wire::ReplyPermissionResponse::new()
            .write_to_bytes()
            .unwrap()),
        _ => panic!("unexpected RPC {method}"),
    })
    .await;
    let mut dialog = RecordingDialog::default();
    dialog
        .selections
        .extend([Some("allow".into()), Some("allow".into())]);
    dialog
        .texts
        .extend([Some(" \n ".into()), Some("  scoped approval  ".into())]);
    bounded(complete_permission(
        fixture.client(),
        "captured-session",
        &permission(),
        &mut dialog,
        &Stop::new(),
    ))
    .await
    .unwrap();
    assert_eq!(dialog.errors, ["A reason is required."]);
    assert_eq!(dialog.pickers.len(), 2);
    assert_eq!(
        dialog.pickers[0].0,
        "write file /synthetic/output — Write generated output"
    );
    assert_eq!(
        dialog.pickers[0]
            .1
            .iter()
            .map(|option| option.description.as_str())
            .collect::<Vec<_>>(),
        ["Allow", "Deny"]
    );
    let calls = fixture.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    let reply = wire::ReplyPermissionRequest::parse_from_bytes(&calls[0].body).unwrap();
    assert_eq!(reply.user_session_id, "captured-session");
    assert_eq!(reply.permission_request_id, "permission-request");
    assert_eq!(reply.choice_value, "allow");
    assert_eq!(reply.reason, "scoped approval");
    fixture.close().await;
}

#[tokio::test]
async fn permission_picker_or_reason_cancellation_chooses_only_plain_reject() {
    for reason_cancelled in [false, true] {
        let mut fixture = Fixture::start(|method, _| match method {
            "reply_permission" => Ok(wire::ReplyPermissionResponse::new()
                .write_to_bytes()
                .unwrap()),
            _ => panic!("unexpected RPC {method}"),
        })
        .await;
        let mut dialog = RecordingDialog::default();
        if reason_cancelled {
            dialog.selections.push_back(Some("allow".into()));
        }
        bounded(complete_permission(
            fixture.client(),
            "captured-session",
            &permission(),
            &mut dialog,
            &Stop::new(),
        ))
        .await
        .unwrap();
        let calls = fixture.calls.lock().unwrap().clone();
        let reply = wire::ReplyPermissionRequest::parse_from_bytes(&calls[0].body).unwrap();
        assert_eq!(reply.choice_value, "deny");
        assert_eq!(reply.reason, "");
        assert_eq!(reply.user_session_id, "captured-session");
        assert!(dialog.errors.is_empty());
        fixture.close().await;
    }
}

#[tokio::test]
async fn permission_without_plain_reject_shows_error_and_never_invents_choice() {
    for reason_cancelled in [false, true] {
        let mut fixture =
            Fixture::start(|method, _| panic!("must not reply without a reject choice: {method}"))
                .await;
        let mut pending = permission();
        pending.choices.truncate(1);
        let mut dialog = RecordingDialog::default();
        if reason_cancelled {
            dialog.selections.push_back(Some("allow".into()));
        }
        bounded(complete_permission(
            fixture.client(),
            "captured-session",
            &pending,
            &mut dialog,
            &Stop::new(),
        ))
        .await
        .unwrap();
        assert_eq!(
            dialog.errors,
            ["The permission request has no plain reject choice."]
        );
        assert!(fixture.calls.lock().unwrap().is_empty());
        fixture.close().await;
    }
}

#[tokio::test]
async fn permission_invalid_argument_reopens_picker_not_found_settles_other_status_propagates() {
    for code in [
        tonic::Code::InvalidArgument,
        tonic::Code::NotFound,
        tonic::Code::PermissionDenied,
    ] {
        let attempt = Arc::new(AtomicUsize::new(0));
        let observed = attempt.clone();
        let mut fixture = Fixture::start(move |method, _| {
            assert_eq!(method, "reply_permission");
            if observed.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(Status::new(code, "controlled permission failure"))
            } else {
                Ok(wire::ReplyPermissionResponse::new()
                    .write_to_bytes()
                    .unwrap())
            }
        })
        .await;
        let mut dialog = RecordingDialog::default();
        dialog
            .selections
            .extend([Some("deny".into()), Some("deny".into())]);
        let result = bounded(complete_permission(
            fixture.client(),
            "captured-session",
            &permission(),
            &mut dialog,
            &Stop::new(),
        ))
        .await;
        if code == tonic::Code::PermissionDenied {
            assert!(
                matches!(result, Err(parrot_cli::slash_sessions::SlashFailure::Rpc(status)) if status.code() == code)
            );
        } else {
            result.unwrap();
        }
        assert_eq!(
            attempt.load(Ordering::SeqCst),
            if code == tonic::Code::InvalidArgument {
                2
            } else {
                1
            }
        );
        assert_eq!(
            dialog.errors,
            if code == tonic::Code::InvalidArgument {
                vec!["controlled permission failure"]
            } else {
                vec![]
            }
        );
        fixture.close().await;
    }
}
