//! Source-ordered, independently sampled runtime status sections.

use chrono::{Local, TimeZone};
use parrot_core::ContextReminderCheckpoint;
use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation, Clock, OperationError},
    values::{
        accounting::{
            AgentSessionStatisticsSnapshot, AgentUsageKey, AgentUsageSnapshot, AgentUsageTotals,
        },
        context::ContextSnapshot,
    },
};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use serde_json::{Map, Value, json};
use std::{
    error::Error,
    fmt,
    future::poll_fn,
    sync::Arc,
    task::Poll,
    time::{Duration, SystemTime},
};

pub type StatusResult<T> = Result<T, OperationError<StatusError>>;

#[derive(Debug)]
pub struct StatusError {
    message: String,
    source: Option<Box<dyn Error + Send + Sync>>,
}
impl StatusError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            source: None,
        }
    }
    pub fn caused_by(
        message: impl Into<String>,
        source: impl Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }
}
impl fmt::Display for StatusError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}
impl Error for StatusError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn Error + 'static))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusQuery {
    pub session_id: String,
    pub parent_session_id: String,
    pub parent_session_name: String,
    pub profile: String,
    pub requested_model: String,
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StatusObservation {
    pub available: bool,
    pub text: String,
    /// Source-computed identity over the observation's inputs, matching the
    /// status slot hashing contract rather than the rendered text.
    pub hash: String,
    pub is_auto_update: bool,
}
impl StatusObservation {
    pub fn available_text(text: impl Into<String>) -> Self {
        Self {
            available: true,
            text: text.into(),
            hash: String::new(),
            is_auto_update: false,
        }
    }
    /// Attaches the source-computed identity of an observation, which the
    /// steering diff reads rather than the rendered text.
    pub fn with_identity(mut self, hash: impl Into<String>, is_auto_update: bool) -> Self {
        self.hash = hash.into();
        self.is_auto_update = is_auto_update;
        self
    }
}

/// One offered status slot from a registry observation, in registry key order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusSlot {
    pub key: String,
    pub hash: String,
    pub text: String,
    pub is_auto_update: bool,
}

pub trait StatusProvider: Send + Sync {
    fn key(&self) -> &str;
    fn observe<'a>(
        &'a self,
        query: &'a StatusQuery,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, StatusResult<StatusObservation>>;
}

pub struct StatusRegistry {
    providers: Vec<Arc<dyn StatusProvider>>,
}
impl StatusRegistry {
    pub fn new(mut providers: Vec<Arc<dyn StatusProvider>>) -> Result<Self, StatusError> {
        let mut keys = std::collections::HashSet::new();
        for provider in &providers {
            validate_key(provider.key(), "provider")?;
            if !keys.insert(provider.key()) {
                return Err(duplicate(provider.key()));
            }
        }
        providers.sort_by(|left, right| ordinal(left.key(), right.key()));
        Ok(Self { providers })
    }

    pub async fn observe(
        &self,
        query: &StatusQuery,
        profile: Option<&dyn StatusProvider>,
        additional: &[&dyn StatusProvider],
        cancellation: &dyn Cancellation,
    ) -> StatusResult<String> {
        let slots = self
            .observe_slots(query, profile, additional, cancellation)
            .await?;
        Ok(slots
            .iter()
            .map(|slot| slot.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n"))
    }

    /// Observes every provider once and returns the resulting slots in registry
    /// key order. The join poll semantics match `observe` above.
    pub async fn observe_slots(
        &self,
        query: &StatusQuery,
        profile: Option<&dyn StatusProvider>,
        additional: &[&dyn StatusProvider],
        cancellation: &dyn Cancellation,
    ) -> StatusResult<Vec<StatusSlot>> {
        let mut providers: Vec<&dyn StatusProvider> =
            self.providers.iter().map(Arc::as_ref).collect();
        if let Some(extra) = profile {
            validate_key(extra.key(), "profile provider").map_err(OperationError::Failed)?;
            if providers
                .iter()
                .any(|provider| provider.key() == extra.key())
            {
                return Err(OperationError::Failed(duplicate(extra.key())));
            }
            providers.push(extra);
        }
        for &extra in additional {
            validate_key(extra.key(), "additional provider").map_err(OperationError::Failed)?;
            if providers
                .iter()
                .any(|provider| provider.key() == extra.key())
            {
                return Err(OperationError::Failed(duplicate(extra.key())));
            }
            providers.push(extra);
        }
        providers.sort_by(|left, right| ordinal(left.key(), right.key()));
        // Poll every observation before waiting, and join every result even after
        // an error, matching Task.WhenAll rather than serial or fail-fast sampling.
        let mut observations: Vec<_> = providers
            .iter()
            .map(|provider| {
                Some(Box::pin(async move {
                    match provider.observe(query, cancellation).await {
                        Err(OperationError::Cancelled) if cancellation.is_cancelled() => {
                            Err(OperationError::Cancelled)
                        }
                        Err(error) => Err(OperationError::Failed(StatusError::caused_by(
                            format!("{}: {error}", provider.key()),
                            error,
                        ))),
                        Ok(section) => Ok((provider.key().to_owned(), section)),
                    }
                }))
            })
            .collect();
        let mut results: Vec<_> = (0..observations.len()).map(|_| None).collect();
        poll_fn(|context| {
            let mut pending = false;
            for (observation, result) in observations.iter_mut().zip(&mut results) {
                if let Some(future) = observation {
                    match future.as_mut().poll(context) {
                        Poll::Ready(value) => {
                            *result = Some(value);
                            *observation = None;
                        }
                        Poll::Pending => pending = true,
                    }
                }
            }
            if pending {
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        })
        .await;
        let mut sections = Vec::new();
        let mut cancelled = false;
        for result in results {
            match result.expect("every provider observation was joined") {
                Ok((key, section))
                    if section.available && !section.text.chars().all(char::is_whitespace) =>
                {
                    sections.push(StatusSlot {
                        key,
                        hash: section.hash,
                        text: section.text,
                        is_auto_update: section.is_auto_update,
                    })
                }
                Ok(_) => {}
                Err(OperationError::Cancelled) => cancelled = true,
                // Task.WhenAll faults take precedence over cancelled tasks.
                Err(error) => return Err(error),
            }
        }
        if cancelled {
            Err(OperationError::Cancelled)
        } else {
            Ok(sections)
        }
    }
}
fn duplicate(key: &str) -> StatusError {
    StatusError::new(format!("status: duplicate provider '{key}'"))
}
fn validate_key(key: &str, subject: &str) -> Result<(), StatusError> {
    if !key
        .find(':')
        .is_some_and(|separator| separator > 0 && separator + 1 < key.len())
        || key.chars().any(char::is_whitespace)
    {
        Err(StatusError::new(format!(
            "status: {subject} requires a stable namespaced key"
        )))
    } else {
        Ok(())
    }
}
/// .NET Ordinal compares UTF-16 code units, not UTF-8 bytes/scalar values.
pub(crate) fn ordinal(left: &str, right: &str) -> std::cmp::Ordering {
    left.encode_utf16().cmp(right.encode_utf16())
}
fn check(cancellation: &dyn Cancellation) -> StatusResult<()> {
    cancellation.check().map_err(|_| OperationError::Cancelled)
}
pub(crate) fn render(
    templates: &PromptTemplateCatalog,
    id: &str,
    model: Map<String, Value>,
    cancellation: &dyn Cancellation,
) -> StatusResult<StatusObservation> {
    let arguments: Vec<_> = model.into_iter().collect();
    templates
        .render(id, &arguments, cancellation)
        .map(StatusObservation::available_text)
        .map_err(|error| match error {
            OperationError::Cancelled => OperationError::Cancelled,
            OperationError::Failed(error) => {
                OperationError::Failed(StatusError::caused_by(error.to_string(), error))
            }
        })
}

pub struct ProfileStatusProvider {
    pub key: String,
    pub prompt: String,
}
impl StatusProvider for ProfileStatusProvider {
    fn key(&self) -> &str {
        &self.key
    }
    fn observe<'a>(
        &'a self,
        _query: &'a StatusQuery,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, StatusResult<StatusObservation>> {
        Box::pin(async move {
            check(cancellation)?;
            Ok(if self.prompt.chars().all(char::is_whitespace) {
                StatusObservation::default()
            } else {
                StatusObservation::available_text(&self.prompt)
                    .with_identity(format!("profile-prompt:{}", self.prompt), true)
            })
        })
    }
}

pub struct SelectionStatusProvider {
    pub templates: Arc<PromptTemplateCatalog>,
}
impl StatusProvider for SelectionStatusProvider {
    fn key(&self) -> &str {
        "runtime:selection"
    }
    fn observe<'a>(
        &'a self,
        query: &'a StatusQuery,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, StatusResult<StatusObservation>> {
        Box::pin(async move {
            check(cancellation)?;
            render(
                &self.templates,
                "status.selection",
                json!({
                    "profile": query.profile, "model": query.requested_model,
                    "parent_session_id": query.parent_session_id, "parent_session_name": query.parent_session_name,
                    "has_parent": !query.parent_session_id.chars().all(char::is_whitespace),
                    "has_parent_name": !query.parent_session_name.chars().all(char::is_whitespace),
                })
                .as_object()
                .unwrap()
                .clone(),
                cancellation,
            )
            .map(|observation| {
                observation.with_identity(
                    format!(
                        "selection:{}:{}:{}:{}",
                        query.profile, query.requested_model, query.parent_session_id,
                        query.parent_session_name
                    ),
                    true,
                )
            })
        })
    }
}

pub struct GeneratedTimeStatusProvider {
    pub clock: Arc<dyn Clock>,
    pub started_at: SystemTime,
    pub templates: Arc<PromptTemplateCatalog>,
}
impl StatusProvider for GeneratedTimeStatusProvider {
    fn key(&self) -> &str {
        "runtime:generated-time"
    }
    fn observe<'a>(
        &'a self,
        _query: &'a StatusQuery,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, StatusResult<StatusObservation>> {
        Box::pin(async move {
            check(cancellation)?;
            let now = self.clock.utc_now();
            // The bucket, not the rendered text, is the steering identity: the
            // text changes on every render, while the hash only moves when the
            // clock crosses a five-minute boundary.
            let bucket = epoch_seconds(now).div_euclid(300);
            let time = format_local_minute(now).map_err(OperationError::Failed)?;
            let duration = format_duration(now.duration_since(self.started_at).unwrap_or_default());
            render(
                &self.templates,
                "status.generated-time",
                Map::from_iter([
                    ("time".into(), time.into()),
                    ("duration".into(), duration.into()),
                ]),
                cancellation,
            )
            .map(|observation| observation.with_identity(format!("generated-time:{bucket}"), true))
        })
    }
}
fn epoch_seconds(time: SystemTime) -> i64 {
    let (sign, duration) = match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(duration) => (1, duration),
        Err(error) => (-1, error.duration()),
    };
    sign * i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
}
fn format_local_minute(time: SystemTime) -> Result<String, StatusError> {
    let (sign, duration) = match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(duration) => (1, duration),
        Err(error) => (-1, error.duration()),
    };
    let seconds = sign * i64::try_from(duration.as_secs()).unwrap_or(i64::MAX);
    let local = Local
        .timestamp_opt(seconds, duration.subsec_nanos())
        .single()
        .ok_or_else(|| StatusError::new("local time is out of the supported range"))?;
    Ok(local.format("%Y-%m-%d %H:%M").to_string())
}
fn format_duration(elapsed: Duration) -> String {
    let minutes = elapsed.as_secs() / 60;
    let hours = minutes / 60;
    if hours > 0 {
        format!("{hours}h {:02}m", minutes % 60)
    } else {
        format!("{minutes}m")
    }
}

fn notification_band(snapshot: &ContextSnapshot) -> Option<i32> {
    let interval = ContextReminderCheckpoint::NOTIFICATION_INTERVAL;
    snapshot
        .usage_percent
        .filter(|_| snapshot.context_limit > 0)
        .map(|usage| usage / interval * interval)
        .filter(|band| *band >= 50)
}

fn context_hash(query: &StatusQuery, snapshot: &ContextSnapshot, band: Option<i32>) -> String {
    format!(
        "context:{}:{}:{}:{}",
        query.requested_model,
        snapshot.context_limit,
        snapshot.has_context_limit_override,
        band.map(|band| band.to_string())
            .unwrap_or_else(|| "none".into())
    )
}

fn usage_model(snapshot: &ContextSnapshot) -> Map<String, Value> {
    let mut model = json!({
        "available": snapshot.is_available() && snapshot.usage_percent.is_some(), "estimated_tokens": format_tokens(snapshot.estimated_tokens),
        "context_limit": format_tokens(i64::from(snapshot.context_limit)),
        "cadence": ContextReminderCheckpoint::NOTIFICATION_INTERVAL.to_string(), "trigger": snapshot.trigger_percent.to_string(),
    }).as_object().unwrap().clone();
    if let Some(usage) = snapshot.usage_percent {
        model.insert("usage".into(), usage.to_string().into());
    } else {
        model.insert("usage".into(), "".into());
    }
    if snapshot.has_context_limit_override {
        model.insert(
            "trigger_tokens".into(),
            snapshot
                .trigger_tokens
                .map_or_else(|| "unavailable".into(), |tokens| tokens.to_string())
                .into(),
        );
    }
    model
}

fn reminder_model(snapshot: &ContextSnapshot, band: i32) -> Map<String, Value> {
    let mut model = json!({
        "percentage": band.to_string(),
        "estimated_tokens": snapshot.estimated_tokens.to_string(),
        "context_limit": snapshot.context_limit.to_string(),
        "notification_interval": ContextReminderCheckpoint::NOTIFICATION_INTERVAL.to_string(),
        "trigger": snapshot.trigger_percent.to_string(),
    })
    .as_object()
    .expect("context reminder model is object")
    .clone();
    if snapshot.has_context_limit_override {
        model.insert(
            "trigger_tokens".into(),
            snapshot
                .trigger_tokens
                .map_or_else(|| "unavailable".into(), |tokens| tokens.to_string())
                .into(),
        );
    }
    model
}

fn render_usage_line(
    snapshot: &ContextSnapshot,
    templates: &Arc<PromptTemplateCatalog>,
    cancellation: &dyn Cancellation,
) -> StatusResult<StatusObservation> {
    render(
        templates,
        if snapshot.has_context_limit_override {
            "status.context-limit"
        } else {
            "status.context"
        },
        usage_model(snapshot),
        cancellation,
    )
}

fn render_reminder(
    snapshot: &ContextSnapshot,
    band: i32,
    templates: &Arc<PromptTemplateCatalog>,
    cancellation: &dyn Cancellation,
) -> StatusResult<StatusObservation> {
    render(
        templates,
        if snapshot.has_context_limit_override {
            "agent-session.context-limit-reminder"
        } else {
            "agent-session.context-reminder"
        },
        reminder_model(snapshot, band),
        cancellation,
    )
}

/// One slot observation covering context usage and, when a notification band
/// is active, the band reminder appended under the usage line. The identity
/// hash records the band, so the steering machinery refreshes the slot once
/// per band and the standing comparison gates repeats across tool followups
/// and restarts. Without an active band the hash records "none" and stays
/// stable across usage changes.
pub struct ContextStatusProvider {
    pub snapshot: ContextSnapshot,
    pub templates: Arc<PromptTemplateCatalog>,
}
impl StatusProvider for ContextStatusProvider {
    fn key(&self) -> &str {
        "runtime:context"
    }
    fn observe<'a>(
        &'a self,
        query: &'a StatusQuery,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, StatusResult<StatusObservation>> {
        Box::pin(async move {
            check(cancellation)?;
            let usage = render_usage_line(&self.snapshot, &self.templates, cancellation)?;
            let hash = context_hash(query, &self.snapshot, notification_band(&self.snapshot));
            match notification_band(&self.snapshot) {
                None => Ok(usage.with_identity(hash, true)),
                Some(band) => {
                    let reminder =
                        render_reminder(&self.snapshot, band, &self.templates, cancellation)?;
                    Ok(StatusObservation {
                        available: usage.available && reminder.available,
                        text: format!("{}\n\n{}", usage.text, reminder.text),
                        hash,
                        is_auto_update: true,
                    })
                }
            }
        })
    }
}

/// Usage-line-only observation for the runtime-tree view, which never renders
/// the band reminder. The stable identity never changes and this slot never
/// triggers steering.
struct ContextUsageStatusProvider {
    snapshot: ContextSnapshot,
    templates: Arc<PromptTemplateCatalog>,
}
impl StatusProvider for ContextUsageStatusProvider {
    fn key(&self) -> &str {
        "runtime:context"
    }
    fn observe<'a>(
        &'a self,
        _query: &'a StatusQuery,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, StatusResult<StatusObservation>> {
        Box::pin(async move {
            check(cancellation)?;
            render_usage_line(&self.snapshot, &self.templates, cancellation)
                .map(|observation| observation.with_identity("context-usage:activity", false))
        })
    }
}

pub struct StatisticsStatusProvider {
    pub snapshot: AgentSessionStatisticsSnapshot,
    pub templates: Arc<PromptTemplateCatalog>,
}
impl StatusProvider for StatisticsStatusProvider {
    fn key(&self) -> &str {
        "runtime:statistics"
    }
    fn observe<'a>(
        &'a self,
        query: &'a StatusQuery,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, StatusResult<StatusObservation>> {
        Box::pin(async move {
            check(cancellation)?;
            render(&self.templates, "status.statistics", json!({
                "session_id": query.session_id,
                "views": [usage_view(&self.snapshot.own, false), usage_view(&self.snapshot.cumulative, true)],
                "incomplete_legacy_tool_counts": self.snapshot.has_incomplete_legacy_tool_counts,
            }).as_object().unwrap().clone(), cancellation).map(|observation| {
                observation.with_identity(
                    format!(
                        "statistics:{}:{}:{}:{}:{}",
                        self.snapshot.own.totals.input_tokens,
                        self.snapshot.own.totals.output_tokens,
                        self.snapshot.own.totals.tool_calls,
                        self.snapshot.cumulative.totals.input_tokens,
                        self.snapshot.has_incomplete_legacy_tool_counts
                    ),
                    true,
                )
            })
        })
    }
}
fn usage_view(usage: &AgentUsageSnapshot, cumulative: bool) -> Value {
    let mut models: Vec<_> = usage.models.iter().collect();
    models.sort_by(|(left, _), (right, _)| {
        ordinal(&left.provider, &right.provider)
            .then_with(|| ordinal(&left.model, &right.model))
            .then_with(|| match (&left.effort, &right.effort) {
                (None, None) => std::cmp::Ordering::Equal,
                (None, Some(_)) => std::cmp::Ordering::Less,
                (Some(_), None) => std::cmp::Ordering::Greater,
                (Some(left), Some(right)) => ordinal(left, right),
            })
    });
    let mut view = totals_model(&usage.totals);
    view.insert("cumulative".into(), cumulative.into());
    view.insert("models".into(), models.into_iter().map(|(key, totals)| {
        let mut model = totals_model(totals);
        model.extend(json!({"provider": key.provider, "model": key.model, "effort": key.effort.as_deref().unwrap_or(""), "has_effort": key.effort.is_some(), "legacy": *key == AgentUsageKey::legacy()}).as_object().unwrap().clone());
        Value::Object(model)
    }).collect());
    view.into()
}
fn totals_model(totals: &AgentUsageTotals) -> Map<String, Value> {
    json!({
        "input_tokens": format_tokens(totals.input_tokens),
        "cache_percentage": format!("{:.2}", if totals.input_tokens > 0 { totals.cached_input_tokens as f64 / totals.input_tokens as f64 * 100.0 } else { 0.0 }),
        "output_tokens": format_tokens(totals.output_tokens), "tool_calls": totals.tool_calls.to_string(),
        "input_cost": format!("{:.2}", totals.input_cost), "output_cost": format!("{:.2}", totals.output_cost),
        "total_cost": format!("{:.2}", totals.total_cost()),
    }).as_object().unwrap().clone()
}
pub fn format_tokens(count: i64) -> String {
    let (value, suffix) = if count >= 1_000_000 {
        (count as f64 / 1_000_000.0, "M")
    } else if count >= 1000 {
        (count as f64 / 1000.0, "k")
    } else {
        return count.to_string();
    };
    let rounded = (value * 10.0).round() / 10.0;
    let number = format!("{rounded:.1}");
    format!("{}{suffix}", number.strip_suffix(".0").unwrap_or(&number))
}

/// Captures belong to the calling real session/selection; no last-observation
/// state is retained here. Each call receives its own immutable context/usage.
pub struct RuntimeStatus {
    templates: Arc<PromptTemplateCatalog>,
    activity: StatusRegistry,
    full: StatusRegistry,
}
impl RuntimeStatus {
    /// Binds only immutable query/profile identity, not a cached context snapshot.
    pub fn bind_context(
        self: &Arc<Self>,
        query: StatusQuery,
        profile_prompt: String,
    ) -> Arc<dyn crate::compaction::ContextStatus> {
        Arc::new(BoundContextStatus {
            runtime: self.clone(),
            query,
            profile_prompt,
        })
    }

    pub fn new(
        templates: Arc<PromptTemplateCatalog>,
        clock: Arc<dyn Clock>,
        started_at: SystemTime,
        providers: Vec<Arc<dyn StatusProvider>>,
    ) -> Result<Self, StatusError> {
        let activity = StatusRegistry::new(providers.clone())?;
        let mut full: Vec<Arc<dyn StatusProvider>> = vec![
            Arc::new(GeneratedTimeStatusProvider {
                clock,
                started_at,
                templates: templates.clone(),
            }),
            Arc::new(SelectionStatusProvider {
                templates: templates.clone(),
            }),
        ];
        full.extend(providers);
        Ok(Self {
            templates,
            activity,
            full: StatusRegistry::new(full)?,
        })
    }
    pub async fn observe_runtime(
        &self,
        query: &StatusQuery,
        context: ContextSnapshot,
        cancellation: &dyn Cancellation,
    ) -> StatusResult<String> {
        let context_provider = ContextUsageStatusProvider {
            snapshot: context,
            templates: self.templates.clone(),
        };
        self.activity
            .observe(query, None, &[&context_provider], cancellation)
            .await
    }
    pub async fn observe_statistics(
        &self,
        query: &StatusQuery,
        statistics: AgentSessionStatisticsSnapshot,
        cancellation: &dyn Cancellation,
    ) -> StatusResult<String> {
        StatusRegistry::new(vec![Arc::new(StatisticsStatusProvider {
            snapshot: statistics,
            templates: self.templates.clone(),
        })])
        .map_err(OperationError::Failed)?
        .observe(query, None, &[], cancellation)
        .await
    }
    pub async fn observe_slots(
        &self,
        query: &StatusQuery,
        profile_prompt: &str,
        context: &ContextSnapshot,
        cancellation: &dyn Cancellation,
    ) -> StatusResult<Vec<StatusSlot>> {
        let context_provider = ContextStatusProvider {
            snapshot: context.clone(),
            templates: self.templates.clone(),
        };
        self.full
            .observe_slots(
                query,
                Some(&ProfileStatusProvider {
                    key: format!("profile:{}", query.profile),
                    prompt: profile_prompt.to_owned(),
                }),
                &[&context_provider],
                cancellation,
            )
            .await
    }
    pub async fn observe_with_context(
        &self,
        query: &StatusQuery,
        profile_prompt: &str,
        context: ContextSnapshot,
        cancellation: &dyn Cancellation,
    ) -> StatusResult<String> {
        let profile = ProfileStatusProvider {
            key: format!("profile:{}", query.profile),
            prompt: profile_prompt.to_owned(),
        };
        let context_provider = ContextStatusProvider {
            snapshot: context,
            templates: self.templates.clone(),
        };
        self.full
            .observe(query, Some(&profile), &[&context_provider], cancellation)
            .await
    }
}

struct BoundContextStatus {
    runtime: Arc<RuntimeStatus>,
    query: StatusQuery,
    profile_prompt: String,
}
impl crate::compaction::ContextStatus for BoundContextStatus {
    fn observe<'a>(
        &'a self,
        context: &'a ContextSnapshot,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, crate::compaction::ContextResult<String>> {
        Box::pin(async move {
            self.runtime
                .observe_with_context(
                    &self.query,
                    &self.profile_prompt,
                    context.clone(),
                    cancellation,
                )
                .await
                .map_err(context_error)
        })
    }

    fn slots<'a>(
        &'a self,
        context: &'a ContextSnapshot,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, crate::compaction::ContextResult<parrot_core::StatusOffering>> {
        Box::pin(async move {
            self.runtime
                .observe_slots(&self.query, &self.profile_prompt, context, cancellation)
                .await
                .map_err(context_error)
                .map(|slots| {
                    slots
                        .into_iter()
                        .map(|slot| {
                            (
                                parrot_core::StatusKey::new(slot.key),
                                parrot_core::StatusOffer {
                                    hash: slot.hash,
                                    text: slot.text,
                                    is_auto_update: slot.is_auto_update,
                                },
                            )
                        })
                        .collect()
                })
        })
    }
}
fn context_error(error: OperationError<StatusError>) -> crate::compaction::ContextError {
    use crate::compaction::ContextError;
    match error {
        OperationError::Cancelled => ContextError::Cancelled,
        OperationError::Failed(error) => ContextError::Operation(error.to_string()),
    }
}
