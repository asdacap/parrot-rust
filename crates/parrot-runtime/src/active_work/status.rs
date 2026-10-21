//! Source-ordered, independently sampled runtime status sections.

use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation, Clock, OperationError},
    values::{
        accounting::{
            AgentSessionStatisticsSnapshot, AgentUsageKey, AgentUsageSnapshot, AgentUsageTotals,
        },
        context::{ContextCadence, ContextSnapshot},
    },
};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use serde_json::{Map, Value, json};
use std::{error::Error, fmt, future::poll_fn, sync::Arc, task::Poll, time::SystemTime};

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
}
impl StatusObservation {
    pub fn available_text(text: impl Into<String>) -> Self {
        Self {
            available: true,
            text: text.into(),
        }
    }
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
        additional: Option<&dyn StatusProvider>,
        cancellation: &dyn Cancellation,
    ) -> StatusResult<String> {
        let mut providers: Vec<&dyn StatusProvider> =
            self.providers.iter().map(Arc::as_ref).collect();
        for (extra, subject) in [
            (profile, "profile provider"),
            (additional, "additional provider"),
        ] {
            if let Some(extra) = extra {
                validate_key(extra.key(), subject).map_err(OperationError::Failed)?;
                if providers
                    .iter()
                    .any(|provider| provider.key() == extra.key())
                {
                    return Err(OperationError::Failed(duplicate(extra.key())));
                }
                providers.push(extra);
                providers.sort_by(|left, right| ordinal(left.key(), right.key()));
            }
        }
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
                        Ok(section) => Ok(section),
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
                Ok(section)
                    if section.available && !section.text.chars().all(char::is_whitespace) =>
                {
                    sections.push(section.text)
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
            Ok(sections.join("\n\n"))
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
            render(&self.templates, "status.selection", json!({
                "profile": query.profile, "model": query.requested_model,
                "parent_session_id": query.parent_session_id, "parent_session_name": query.parent_session_name,
                "has_parent": !query.parent_session_id.chars().all(char::is_whitespace),
                "has_parent_name": !query.parent_session_name.chars().all(char::is_whitespace),
            }).as_object().unwrap().clone(), cancellation)
        })
    }
}

pub struct GeneratedTimeStatusProvider {
    pub clock: Arc<dyn Clock>,
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
            let time = format_utc(self.clock.utc_now()).map_err(OperationError::Failed)?;
            render(
                &self.templates,
                "status.generated-time",
                Map::from_iter([("time".into(), time.into())]),
                cancellation,
            )
        })
    }
}
// Gregorian civil-date conversion, with the source DateTime range and fixed
// seven fractional digits of DateTimeOffset.ToString("O"). No local timezone.
fn format_utc(time: SystemTime) -> Result<String, StatusError> {
    let ticks = match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(duration) => duration.as_nanos() as i128 / 100,
        Err(error) => -((error.duration().as_nanos() as i128 + 99) / 100),
    };
    let seconds = ticks.div_euclid(10_000_000);
    let fraction = ticks.rem_euclid(10_000_000);
    let days = seconds.div_euclid(86_400);
    let within_day = seconds.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i128::from(month <= 2);
    if !(1..=9999).contains(&year) {
        return Err(StatusError::new("UTC time is outside the DateTime range"));
    }
    Ok(format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{fraction:07}+00:00",
        within_day / 3600,
        within_day / 60 % 60,
        within_day % 60
    ))
}

pub struct ContextStatusProvider {
    pub snapshot: ContextSnapshot,
    pub templates: Arc<PromptTemplateCatalog>,
}
impl StatusProvider for ContextStatusProvider {
    fn key(&self) -> &str {
        "runtime:queues-context"
    }
    fn observe<'a>(
        &'a self,
        _query: &'a StatusQuery,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, StatusResult<StatusObservation>> {
        Box::pin(async move {
            check(cancellation)?;
            let snapshot = &self.snapshot;
            let mut model = json!({
                "available": snapshot.is_available(), "estimated_tokens": format_tokens(snapshot.estimated_tokens),
                "context_limit": format_tokens(i64::from(snapshot.context_limit)),
                "cadence": ContextCadence::NOTIFICATION_INTERVAL.to_string(), "trigger": snapshot.trigger_percent.to_string(),
            }).as_object().unwrap().clone();
            if let Some(usage) = snapshot.usage_percent {
                model.insert("usage".into(), usage.to_string().into());
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
            render(
                &self.templates,
                if snapshot.has_context_limit_override {
                    "status.context-limit"
                } else {
                    "status.context"
                },
                model,
                cancellation,
            )
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
            }).as_object().unwrap().clone(), cancellation)
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
        providers: Vec<Arc<dyn StatusProvider>>,
    ) -> Result<Self, StatusError> {
        let activity = StatusRegistry::new(providers.clone())?;
        let mut full: Vec<Arc<dyn StatusProvider>> = vec![
            Arc::new(GeneratedTimeStatusProvider {
                clock,
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
        self.activity
            .observe(
                query,
                None,
                Some(&ContextStatusProvider {
                    snapshot: context,
                    templates: self.templates.clone(),
                }),
                cancellation,
            )
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
        .observe(query, None, None, cancellation)
        .await
    }
    pub async fn observe_with_context(
        &self,
        query: &StatusQuery,
        profile_prompt: &str,
        context: ContextSnapshot,
        cancellation: &dyn Cancellation,
    ) -> StatusResult<String> {
        self.full
            .observe(
                query,
                Some(&ProfileStatusProvider {
                    key: format!("profile:{}", query.profile),
                    prompt: profile_prompt.to_owned(),
                }),
                Some(&ContextStatusProvider {
                    snapshot: context,
                    templates: self.templates.clone(),
                }),
                cancellation,
            )
            .await
    }
    pub async fn observe_fitted(
        &self,
        query: &StatusQuery,
        profile_prompt: &str,
        mut estimate: impl FnMut(&str) -> Result<ContextSnapshot, StatusError>,
        cancellation: &dyn Cancellation,
    ) -> StatusResult<String> {
        let mut context = estimate("").map_err(OperationError::Failed)?;
        let mut content = self
            .observe_with_context(query, profile_prompt, context.clone(), cancellation)
            .await?;
        for _ in 0..8 {
            let rendered = estimate(&content).map_err(OperationError::Failed)?;
            if rendered == context {
                break;
            }
            context = rendered;
            content = self
                .observe_with_context(query, profile_prompt, context.clone(), cancellation)
                .await?;
        }
        Ok(content)
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
    fn observe_fitted<'a>(
        &'a self,
        estimate: &'a (
                dyn Fn(&str) -> crate::compaction::ContextResult<ContextSnapshot> + Send + Sync
            ),
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, crate::compaction::ContextResult<String>> {
        Box::pin(async move {
            self.runtime
                .observe_fitted(
                    &self.query,
                    &self.profile_prompt,
                    |text| {
                        estimate(text)
                            .map_err(|error| StatusError::caused_by(error.to_string(), error))
                    },
                    cancellation,
                )
                .await
                .map_err(context_error)
        })
    }
}
fn context_error(error: OperationError<StatusError>) -> crate::compaction::ContextError {
    use crate::compaction::ContextError;
    match error {
        OperationError::Cancelled => ContextError::Cancelled,
        OperationError::Failed(mut error) => {
            if let Some(source) = error.source.take() {
                match source.downcast::<ContextError>() {
                    Ok(original) => return *original,
                    Err(source) => error.source = Some(source),
                }
            }
            ContextError::Operation(error.to_string())
        }
    }
}
