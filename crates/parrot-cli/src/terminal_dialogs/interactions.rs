//! Session-captured question, permission and plan interactions for enhanced input.
//!
//! These helpers consume the actual native client and the single slash dialog
//! contract. They do not construct application or filesystem/security services.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation},
    protocol::messages::parrot as wire,
};
use parrot_host::web::router::WebClient;

use crate::{
    dispatch::dialog::{DialogOption, SlashDialog},
    slash_sessions::{SlashFailure, SlashSession, native_call},
};

pub type CountdownCallback = Arc<
    dyn for<'a> Fn(Option<i64>, &'a dyn Cancellation) -> BoxFuture<'a, Result<(), SlashFailure>>
        + Send
        + Sync,
>;

/// Source polling defaults, with an injectable monotonic sample for deterministic
/// countdown characterization. A zero display never closes the server request.
#[derive(Clone)]
pub struct QuestionInteractionOptions {
    pub reconciliation_interval: Duration,
    pub now: Arc<dyn Fn() -> Instant + Send + Sync>,
}

impl Default for QuestionInteractionOptions {
    fn default() -> Self {
        Self {
            reconciliation_interval: Duration::from_millis(250),
            now: Arc::new(Instant::now),
        }
    }
}

pub struct QuestionCountdown {
    remaining_milliseconds: Option<i64>,
    sampled_at: Instant,
}

impl QuestionCountdown {
    pub fn new(now: Instant) -> Self {
        Self {
            remaining_milliseconds: None,
            sampled_at: now,
        }
    }

    pub fn update(&mut self, remaining_milliseconds: Option<i64>, now: Instant) {
        self.remaining_milliseconds = remaining_milliseconds;
        self.sampled_at = now;
    }

    pub fn remaining_seconds(&self, now: Instant) -> Option<i64> {
        self.remaining_milliseconds.map(|milliseconds| {
            let remaining = milliseconds.max(0) as u128;
            let elapsed = now.saturating_duration_since(self.sampled_at).as_millis();
            remaining.saturating_sub(elapsed).div_ceil(1000) as i64
        })
    }
}

struct InteractionCancellation(tokio::sync::watch::Sender<bool>);

impl InteractionCancellation {
    fn new() -> Self {
        Self(tokio::sync::watch::channel(false).0)
    }

    fn cancel(&self) {
        self.0.send_replace(true);
    }
}

impl Cancellation for InteractionCancellation {
    fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }

    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let mut receiver = self.0.subscribe();
            loop {
                if *receiver.borrow_and_update() {
                    return;
                }
                if receiver.changed().await.is_err() {
                    return;
                }
            }
        })
    }
}

struct LinkedCancellation<'a>(&'a dyn Cancellation, &'a dyn Cancellation);

impl Cancellation for LinkedCancellation<'_> {
    fn is_cancelled(&self) -> bool {
        self.0.is_cancelled() || self.1.is_cancelled()
    }

    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            tokio::select! {
                () = self.0.cancelled() => (),
                () = self.1.cancelled() => (),
            }
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Readiness {
    Waiting,
    Ready,
    Stopped,
}

/// A captured session/request lifetime. RPC failures extrapolate the last server
/// timing sample; only absence from a successful listing closes the interaction.
pub struct PendingQuestionLifetime {
    client: WebClient,
    user_session_id: String,
    request_id: String,
    options: QuestionInteractionOptions,
    update_countdown: CountdownCallback,
    ready: tokio::sync::watch::Sender<Readiness>,
    closed: InteractionCancellation,
}

impl PendingQuestionLifetime {
    pub fn new(
        client: WebClient,
        user_session_id: String,
        request_id: String,
        options: QuestionInteractionOptions,
        update_countdown: CountdownCallback,
    ) -> Self {
        Self {
            client,
            user_session_id,
            request_id,
            options,
            update_countdown,
            ready: tokio::sync::watch::channel(Readiness::Waiting).0,
            closed: InteractionCancellation::new(),
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed.is_cancelled()
    }

    pub async fn wait_until_ready(
        &self,
        cancellation: &dyn Cancellation,
    ) -> Result<(), SlashFailure> {
        let mut receiver = self.ready.subscribe();
        loop {
            let readiness = *receiver.borrow_and_update();
            match readiness {
                Readiness::Ready => return Ok(()),
                Readiness::Stopped => {
                    return Err(tonic::Status::cancelled("question reconciliation stopped").into());
                }
                Readiness::Waiting => (),
            }
            tokio::select! {
                biased;
                () = cancellation.cancelled() => return Err(tonic::Status::cancelled("operation cancelled").into()),
                result = receiver.changed() => if result.is_err() {
                    return Err(tonic::Status::cancelled("question reconciliation stopped").into());
                },
            }
        }
    }

    pub async fn run(&self, cancellation: &dyn Cancellation) -> Result<(), SlashFailure> {
        let result = self.reconcile(cancellation).await;
        if *self.ready.borrow() == Readiness::Waiting {
            self.ready.send_replace(Readiness::Stopped);
        }
        match result {
            Err(SlashFailure::Rpc(status))
                if cancellation.is_cancelled() && status.code() == tonic::Code::Cancelled =>
            {
                Ok(())
            }
            result => result,
        }
    }

    async fn reconcile(&self, cancellation: &dyn Cancellation) -> Result<(), SlashFailure> {
        let mut client = self.client.clone();
        let mut countdown = QuestionCountdown::new((self.options.now)());
        let mut displayed_seconds = None;
        while !cancellation.is_cancelled() && !self.is_closed() {
            let listed = native_call(
                client.list_pending_questions(wire::ListPendingQuestionsRequest {
                    user_session_id: self.user_session_id.clone(),
                    ..Default::default()
                }),
                cancellation,
            )
            .await;
            match listed {
                Ok(listed) => {
                    let Some(pending) = listed
                        .questions
                        .iter()
                        .find(|question| question.id == self.request_id)
                    else {
                        self.closed.cancel();
                        self.ready.send_replace(Readiness::Ready);
                        return Ok(());
                    };
                    countdown.update(pending.remaining_timeout_ms, (self.options.now)());
                    self.ready.send_replace(Readiness::Ready);
                }
                Err(_) if cancellation.is_cancelled() => return Ok(()),
                Err(SlashFailure::Rpc(_)) => (),
                Err(failure) => return Err(failure),
            }
            let remaining_seconds = countdown.remaining_seconds((self.options.now)());
            if remaining_seconds != displayed_seconds {
                (self.update_countdown)(remaining_seconds, cancellation).await?;
                displayed_seconds = remaining_seconds;
            }
            tokio::select! {
                biased;
                () = cancellation.cancelled() => return Ok(()),
                () = tokio::time::sleep(self.options.reconciliation_interval) => (),
            }
        }
        Ok(())
    }
}

pub async fn complete_question(
    client: WebClient,
    user_session_id: &str,
    pending: &wire::PendingQuestion,
    dialog: &mut dyn SlashDialog,
    update_countdown: CountdownCallback,
    cancellation: &dyn Cancellation,
) -> Result<(), SlashFailure> {
    complete_question_with_options(
        client,
        user_session_id,
        pending,
        dialog,
        update_countdown,
        QuestionInteractionOptions::default(),
        cancellation,
    )
    .await
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QuestionCompletionOutcome {
    Finished,
    RetryInvalidArgument(String),
}

pub type QuestionCollector = Arc<
    dyn for<'a> Fn(
            &'a wire::PendingQuestion,
            &'a mut dyn SlashDialog,
            &'a dyn Cancellation,
        )
            -> BoxFuture<'a, Result<Option<Vec<wire::QuestionAnswer>>, SlashFailure>>
        + Send
        + Sync,
>;

fn enhanced_question_collector() -> QuestionCollector {
    Arc::new(|pending, dialog, cancellation| {
        Box::pin(collect_question_answers(pending, dialog, cancellation))
    })
}

/// Return frontend-owned error detail while sharing Enhanced's answer collector.
/// The caller owns InvalidArgument formatting and any retry of this request.
pub async fn complete_question_with_outcome(
    client: WebClient,
    user_session_id: &str,
    pending: &wire::PendingQuestion,
    dialog: &mut dyn SlashDialog,
    update_countdown: CountdownCallback,
    cancellation: &dyn Cancellation,
) -> Result<QuestionCompletionOutcome, SlashFailure> {
    complete_question_core(
        client,
        user_session_id,
        pending,
        dialog,
        update_countdown,
        (
            QuestionInteractionOptions::default(),
            enhanced_question_collector(),
        ),
        cancellation,
    )
    .await
}

/// Share the same server-authoritative lifetime and reply/reject RPCs with a
/// frontend-owned answer collector. `None` rejects; the caller owns retry/output.
pub async fn complete_question_using(
    client: WebClient,
    user_session_id: &str,
    pending: &wire::PendingQuestion,
    dialog: &mut dyn SlashDialog,
    update_countdown: CountdownCallback,
    collector: QuestionCollector,
    cancellation: &dyn Cancellation,
) -> Result<QuestionCompletionOutcome, SlashFailure> {
    complete_question_core(
        client,
        user_session_id,
        pending,
        dialog,
        update_countdown,
        (QuestionInteractionOptions::default(), collector),
        cancellation,
    )
    .await
}

pub async fn complete_question_with_options(
    client: WebClient,
    user_session_id: &str,
    pending: &wire::PendingQuestion,
    dialog: &mut dyn SlashDialog,
    update_countdown: CountdownCallback,
    options: QuestionInteractionOptions,
    cancellation: &dyn Cancellation,
) -> Result<(), SlashFailure> {
    match complete_question_core(
        client,
        user_session_id,
        pending,
        dialog,
        update_countdown,
        (options, enhanced_question_collector()),
        cancellation,
    )
    .await?
    {
        QuestionCompletionOutcome::Finished => Ok(()),
        QuestionCompletionOutcome::RetryInvalidArgument(detail) => {
            dialog.show_error(&detail, cancellation).await?;
            Ok(())
        }
    }
}

/// The monitor and dialog are polled in this scope and joined before returning;
/// neither a remote closure nor a session-switch cancellation can leak a reply
/// through a later session's client/identity.
async fn complete_question_core(
    mut client: WebClient,
    user_session_id: &str,
    pending: &wire::PendingQuestion,
    dialog: &mut dyn SlashDialog,
    update_countdown: CountdownCallback,
    collection: (QuestionInteractionOptions, QuestionCollector),
    cancellation: &dyn Cancellation,
) -> Result<QuestionCompletionOutcome, SlashFailure> {
    let (options, collector) = collection;
    let lifetime = PendingQuestionLifetime::new(
        client.clone(),
        user_session_id.to_owned(),
        pending.id.clone(),
        options,
        update_countdown.clone(),
    );
    let stopping = InteractionCancellation::new();
    let monitoring_cancellation = LinkedCancellation(cancellation, &stopping);
    let interaction_cancellation = LinkedCancellation(cancellation, &lifetime.closed);
    let monitoring = lifetime.run(&monitoring_cancellation);
    tokio::pin!(monitoring);
    let mut monitor_finished = false;
    let collected = {
        let collecting = async {
            lifetime.wait_until_ready(&interaction_cancellation).await?;
            collector(pending, dialog, &interaction_cancellation).await
        };
        tokio::pin!(collecting);
        tokio::select! {
            biased;
            result = &mut monitoring => {
                monitor_finished = true;
                match result {
                    Err(failure) => Err(failure),
                    Ok(()) if cancellation.is_cancelled() => Err(tonic::Status::cancelled("operation cancelled").into()),
                    Ok(()) => Ok(None),
                }
            }
            result = &mut collecting => result.map(Some),
        }
    };
    stopping.cancel();
    let monitored = if monitor_finished {
        Ok(())
    } else {
        monitoring.await
    };
    // Clear with an uncancelled cleanup token after joining reconciliation and
    // before the reply/reject RPC, including outer cancellation and failures.
    let cleared = (update_countdown)(None, &InteractionCancellation::new()).await;
    async {
        monitored?;
        let collected = collected?;
        cleared?;
        let Some(decision) = collected else {
            return Ok(QuestionCompletionOutcome::Finished);
        };
        if lifetime.is_closed() {
            return Ok(QuestionCompletionOutcome::Finished);
        }
        match decision {
            None => reject_question(client, user_session_id, &pending.id, cancellation)
                .await
                .map(|()| QuestionCompletionOutcome::Finished),
            Some(answers) => {
                match native_call(
                    client.reply_question(wire::ReplyQuestionRequest {
                        user_session_id: user_session_id.to_owned(),
                        question_request_id: pending.id.clone(),
                        answers,
                        ..Default::default()
                    }),
                    cancellation,
                )
                .await
                {
                    Ok(_) => Ok(QuestionCompletionOutcome::Finished),
                    Err(SlashFailure::Rpc(status)) if status.code() == tonic::Code::NotFound => {
                        Ok(QuestionCompletionOutcome::Finished)
                    }
                    Err(SlashFailure::Rpc(status))
                        if status.code() == tonic::Code::InvalidArgument =>
                    {
                        Ok(QuestionCompletionOutcome::RetryInvalidArgument(
                            status.message().to_owned(),
                        ))
                    }
                    Err(failure) => Err(failure),
                }
            }
        }
    }
    .await
}

pub async fn reject_question(
    mut client: WebClient,
    user_session_id: &str,
    question_request_id: &str,
    cancellation: &dyn Cancellation,
) -> Result<(), SlashFailure> {
    match native_call(
        client.reject_question(wire::RejectQuestionRequest {
            user_session_id: user_session_id.to_owned(),
            question_request_id: question_request_id.to_owned(),
            ..Default::default()
        }),
        cancellation,
    )
    .await
    {
        Ok(_) => Ok(()),
        Err(SlashFailure::Rpc(status)) if status.code() == tonic::Code::NotFound => Ok(()),
        Err(failure) => Err(failure),
    }
}

/// Present the captured permission request. This is RPC/dialog orchestration,
/// not a permission materializer or an authorized filesystem implementation.
pub async fn complete_permission(
    mut client: WebClient,
    user_session_id: &str,
    pending: &wire::PendingPermission,
    dialog: &mut dyn SlashDialog,
    cancellation: &dyn Cancellation,
) -> Result<(), SlashFailure> {
    loop {
        let options: Vec<_> = pending
            .choices
            .iter()
            .map(|choice| {
                DialogOption::new(
                    &choice.value,
                    &choice.label,
                    permission_action(choice.action),
                )
            })
            .collect();
        let selected = dialog
            .select(&permission_description(pending), &options, cancellation)
            .await?;
        let mut choice = match selected {
            Some(selected) => pending
                .choices
                .iter()
                .find(|choice| choice.value == selected.id),
            None => plain_reject(pending),
        };
        let Some(selected_choice) = choice else {
            dialog
                .show_error(
                    "The permission request has no plain reject choice.",
                    cancellation,
                )
                .await?;
            return Ok(());
        };
        let mut reason = String::new();
        if selected_choice.requires_reason {
            match dialog.read_text("Reason", cancellation).await? {
                None => {
                    choice = plain_reject(pending);
                    if choice.is_none() {
                        dialog
                            .show_error(
                                "The permission request has no plain reject choice.",
                                cancellation,
                            )
                            .await?;
                        return Ok(());
                    }
                }
                Some(entered) => {
                    reason = entered.trim().to_owned();
                    if reason.is_empty() {
                        dialog
                            .show_error("A reason is required.", cancellation)
                            .await?;
                        continue;
                    }
                }
            }
        }
        let choice = choice.expect("the fallback reject choice was checked");
        match native_call(
            client.reply_permission(wire::ReplyPermissionRequest {
                user_session_id: user_session_id.to_owned(),
                permission_request_id: pending.id.clone(),
                choice_value: choice.value.clone(),
                reason,
                ..Default::default()
            }),
            cancellation,
        )
        .await
        {
            Ok(_) => return Ok(()),
            Err(SlashFailure::Rpc(status)) if status.code() == tonic::Code::NotFound => {
                return Ok(());
            }
            Err(SlashFailure::Rpc(status)) if status.code() == tonic::Code::InvalidArgument => {
                dialog.show_error(status.message(), cancellation).await?;
            }
            Err(failure) => return Err(failure),
        }
    }
}

fn plain_reject(pending: &wire::PendingPermission) -> Option<&wire::PermissionChoice> {
    pending.choices.iter().find(|choice| {
        choice.action.enum_value().ok() == Some(wire::PermissionAction::PERMISSION_ACTION_DENY)
            && !choice.requires_reason
    })
}

fn permission_action(action: protobuf::EnumOrUnknown<wire::PermissionAction>) -> String {
    match action.enum_value() {
        Ok(wire::PermissionAction::PERMISSION_ACTION_ALLOW) => "Allow".into(),
        Ok(wire::PermissionAction::PERMISSION_ACTION_DENY) => "Deny".into(),
        Ok(wire::PermissionAction::PERMISSION_ACTION_UNSPECIFIED) => "Unspecified".into(),
        Err(value) => value.to_string(),
    }
}

fn permission_description(pending: &wire::PendingPermission) -> String {
    let targets = pending
        .targets
        .iter()
        .map(|target| {
            let scope = match target.scope.enum_value() {
                Ok(wire::PermissionTargetScope::PERMISSION_TARGET_SCOPE_WRITE) => "write".into(),
                Ok(wire::PermissionTargetScope::PERMISSION_TARGET_SCOPE_UNSPECIFIED) => {
                    "unspecified".into()
                }
                Err(value) => value.to_string(),
            };
            let kind = match target.kind.enum_value() {
                Ok(wire::PermissionTargetKind::PERMISSION_TARGET_KIND_FILE) => "file".into(),
                Ok(wire::PermissionTargetKind::PERMISSION_TARGET_KIND_DIRECTORY) => {
                    "directory".into()
                }
                Ok(wire::PermissionTargetKind::PERMISSION_TARGET_KIND_UNSPECIFIED) => {
                    "unspecified".into()
                }
                Err(value) => value.to_string(),
            };
            format!("{scope} {kind} {}", target.path)
        })
        .collect::<Vec<_>>()
        .join(", ");
    if pending.reason.is_empty() {
        targets
    } else {
        format!("{targets} — {}", pending.reason)
    }
}

async fn collect_question_answers(
    pending: &wire::PendingQuestion,
    dialog: &mut dyn SlashDialog,
    cancellation: &dyn Cancellation,
) -> Result<Option<Vec<wire::QuestionAnswer>>, SlashFailure> {
    let mut answers = Vec::with_capacity(pending.questions.len());
    for question in &pending.questions {
        const CUSTOM: &str = "__custom__";
        const DONE: &str = "__done__";
        let title = if question.header.trim().is_empty() {
            question.prompt.clone()
        } else {
            format!("{}\n{}", question.header, question.prompt)
        };
        let direct_custom = question.options.is_empty() && question.custom;
        let mut selected_indexes = std::collections::HashSet::new();
        let mut custom_answer = None;
        loop {
            let mut choices: Vec<_> = question
                .options
                .iter()
                .enumerate()
                .filter(|(index, _)| !selected_indexes.contains(index))
                .map(|(index, option)| {
                    DialogOption::new(
                        format!("__option_{index}__"),
                        &option.label,
                        &option.description,
                    )
                })
                .collect();
            if question.custom && custom_answer.is_none() {
                choices.push(DialogOption::new(
                    CUSTOM,
                    "Custom answer",
                    "Write an answer",
                ));
            }
            if question.multiple
                && !direct_custom
                && (!selected_indexes.is_empty() || custom_answer.is_some())
            {
                choices.push(DialogOption::new(DONE, "Done", "Finish selecting"));
            }
            let selected = if direct_custom {
                choices.first().cloned()
            } else {
                dialog.select(&title, &choices, cancellation).await?
            };
            let Some(selected) = selected else {
                return Ok(None);
            };
            if selected.id == DONE {
                break;
            }
            let answer = if selected.id == CUSTOM {
                let Some(answer) = dialog.read_text(&title, cancellation).await? else {
                    return Ok(None);
                };
                let answer = answer.trim();
                if answer.is_empty() {
                    return Ok(None);
                }
                answer.to_owned()
            } else {
                selected.label.clone()
            };
            if !question.multiple || direct_custom {
                custom_answer = Some(answer);
                break;
            }
            if selected.id == CUSTOM {
                custom_answer = Some(answer);
            } else if let Some(index) = question
                .options
                .iter()
                .position(|option| option.label == selected.label)
            {
                // Source records the first equal label, then emits option order.
                selected_indexes.insert(index);
            }
        }
        let text = if !question.multiple || direct_custom {
            custom_answer.unwrap_or_default()
        } else {
            question
                .options
                .iter()
                .enumerate()
                .filter(|(index, _)| selected_indexes.contains(index))
                .map(|(_, option)| option.label.clone())
                .chain(custom_answer)
                .collect::<Vec<_>>()
                .join(", ")
        };
        answers.push(wire::QuestionAnswer {
            text,
            ..Default::default()
        });
    }
    Ok(Some(answers))
}

/// Complete a plan's source dialog. `true` means that a prompt was queued.
pub async fn complete_plan(
    client: WebClient,
    completed: &wire::PlanCompleted,
    session: &mut dyn SlashSession,
    dialog: &mut dyn SlashDialog,
    cancellation: &dyn Cancellation,
) -> Result<bool, SlashFailure> {
    complete_plan_with_before_send(client, completed, session, dialog, |_| Ok(()), cancellation)
        .await
}

/// Notify the owner after any selected mode change and immediately before the
/// actual send, never for a cancelled, empty, or mode-only action.
pub async fn complete_plan_with_before_send(
    mut client: WebClient,
    completed: &wire::PlanCompleted,
    session: &mut dyn SlashSession,
    dialog: &mut dyn SlashDialog,
    before_send: impl FnOnce(&dyn SlashSession) -> Result<(), SlashFailure> + Send,
    cancellation: &dyn Cancellation,
) -> Result<bool, SlashFailure> {
    let Some(definition) = completed.dialog.as_ref() else {
        return Ok(false);
    };
    let mut options: Vec<_> = definition
        .choices
        .iter()
        .map(|choice| DialogOption::new(&choice.value, &choice.value, &choice.description))
        .collect();
    if !definition.custom_choice.is_empty() {
        options.push(DialogOption::new(
            &definition.custom_choice,
            &definition.custom_choice,
            if definition.custom_description.is_empty() {
                "Provide feedback and revise"
            } else {
                &definition.custom_description
            },
        ));
    }
    if options.is_empty() {
        return Ok(false);
    }
    let Some(selected) = dialog
        .select(&definition.prompt, &options, cancellation)
        .await?
    else {
        return Ok(false);
    };
    let prompt = if selected.id == definition.custom_choice {
        let Some(feedback) = dialog
            .read_text(&definition.custom_prompt, cancellation)
            .await?
        else {
            return Ok(false);
        };
        let feedback = feedback.trim();
        if feedback.is_empty() {
            return Ok(false);
        }
        feedback.to_owned()
    } else {
        let Some(choice) = definition
            .choices
            .iter()
            .find(|choice| choice.value == selected.id)
        else {
            return Ok(false);
        };
        let Some(action) = choice.action.as_ref() else {
            return Ok(false);
        };
        if !action.mode.is_empty() {
            session.select_mode(&action.mode, cancellation).await?;
        }
        if action.prompt.is_empty() {
            return Ok(false);
        }
        action.prompt.clone()
    };
    before_send(session)?;
    native_call(
        client.send_message(wire::SendMessageRequest {
            user_session_id: session.id().to_owned(),
            text: prompt,
            delivery: wire::Delivery::DELIVERY_QUEUE.into(),
            ..Default::default()
        }),
        cancellation,
    )
    .await?;
    Ok(true)
}
