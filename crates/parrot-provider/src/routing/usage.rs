//! Account usage capabilities selected by provider routing.
use crate::{
    http_stream::{self, Endpoint, HttpTransport},
    sessions::{
        Error,
        decorators::{SubscriptionUsage, UsageCredits, UsageReporter, UsageWindow},
        provider::{ApiKeySource, CompatibleOptions},
    },
};
use chrono::{DateTime, Datelike, FixedOffset, NaiveDate, NaiveDateTime, Timelike, Utc};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use serde_json::value::RawValue;
use std::{collections::HashMap, sync::Arc};

type Object = HashMap<String, Box<RawValue>>;

struct BearerUsage {
    endpoint: Endpoint,
    keys: Arc<dyn ApiKeySource>,
    provider_id: String,
    transport: Arc<dyn HttpTransport>,
    user_agent: Option<String>,
}
impl BearerUsage {
    fn new(
        options: &CompatibleOptions,
        transport: Arc<dyn HttpTransport>,
        path: &str,
        user_agent: Option<String>,
    ) -> Result<Self, Error> {
        Ok(Self {
            endpoint: http_stream::endpoint_url(
                &options.base_url,
                path,
                options.allow_insecure_localhost,
                options.allow_insecure_remote,
            )?,
            keys: options.api_key_source.clone(),
            provider_id: options.id.clone(),
            transport,
            user_agent,
        })
    }

    async fn get(&self, cancellation: &dyn Cancellation) -> Result<String, Error> {
        let key = self.keys.api_key(cancellation).await?;
        let mut headers =
            http_stream::bearer_headers(&self.provider_id, &key).map_err(|error| match error {
                http_stream::HttpError::Provider(message) => Error::Provider(message),
                other => Error::Http(other),
            })?;
        if let Some(agent) = &self.user_agent {
            headers.push(("User-Agent".into(), agent.clone()));
        }
        Ok(http_stream::get(
            self.transport.as_ref(),
            self.endpoint.clone(),
            headers,
            http_stream::REQUEST_TIMEOUT,
            http_stream::MAX_ERROR_BYTES,
            cancellation,
        )
        .await?)
    }
}

pub struct KimiUsageReporter(BearerUsage);
impl KimiUsageReporter {
    pub fn new(
        options: &CompatibleOptions,
        transport: Arc<dyn HttpTransport>,
    ) -> Result<Self, Error> {
        Ok(Self(BearerUsage::new(
            options,
            transport,
            "users/me/balance",
            None,
        )?))
    }
}
impl UsageReporter for KimiUsageReporter {
    fn usage<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<SubscriptionUsage, Error>> {
        Box::pin(async move {
            let root = object(&self.0.get(cancellation).await?)?;
            let data = root
                .get("data")
                .filter(|raw| raw.get().starts_with('{'))
                .ok_or_else(|| Error::Provider("provider: usage response has no balance".into()))?;
            let data = object(data.get())?;
            Ok(SubscriptionUsage {
                credits: Some(UsageCredits {
                    has_credits: number(&data, "available_balance") > 0.0,
                    // GetRawText().Trim('"') intentionally retains numeric and
                    // escaped-string lexemes rather than decoding/reformatting.
                    balance: data
                        .get("available_balance")
                        .map_or_else(|| "0".into(), |raw| raw.get().trim_matches('"').into()),
                }),
                ..Default::default()
            })
        })
    }
}

pub struct OpenCodeGoUsageReporter(BearerUsage);
impl OpenCodeGoUsageReporter {
    pub fn new(
        options: &CompatibleOptions,
        transport: Arc<dyn HttpTransport>,
    ) -> Result<Self, Error> {
        Ok(Self(BearerUsage::new(
            options,
            transport,
            "usage",
            Some(format!("parrot/{}", env!("CARGO_PKG_VERSION"))),
        )?))
    }
}
impl UsageReporter for OpenCodeGoUsageReporter {
    fn usage<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<SubscriptionUsage, Error>> {
        Box::pin(async move {
            let root = object(&self.0.get(cancellation).await?)?;
            let usage = root
                .get("usage")
                .filter(|raw| raw.get().starts_with('{'))
                .map(|raw| object(raw.get()))
                .transpose()?;
            let (mut primary, mut secondary) = windows(
                usage.as_ref().unwrap_or(&root),
                ["rolling", "weekly", "monthly"],
            )?;
            if primary.is_none() {
                (primary, secondary) =
                    windows(&root, ["rollingUsage", "weeklyUsage", "monthlyUsage"])?;
            }
            let credits = if root
                .get("useBalance")
                .is_some_and(|raw| raw.get() == "true")
            {
                primary.as_ref().map(|window| {
                    let remaining = 100.0 - window.used_percent;
                    let remaining = if remaining < 0.0 { 0.0 } else { remaining };
                    let display = if remaining == f64::INFINITY {
                        "Infinity".into()
                    } else {
                        format!("{:.0}", remaining.round())
                    };
                    UsageCredits {
                        has_credits: true,
                        balance: format!("{display}% remaining"),
                    }
                })
            } else {
                None
            };
            Ok(SubscriptionUsage {
                primary_window: primary,
                secondary_window: secondary,
                credits,
                ..Default::default()
            })
        })
    }
}

fn object(raw: &str) -> Result<Object, Error> {
    let raw: Box<RawValue> = serde_json::from_str(raw).map_err(|e| Error::Encode(e.to_string()))?;
    if !raw.get().starts_with('{') {
        let kind = match raw.get().as_bytes()[0] {
            b'[' => "Array",
            b'"' => "String",
            b'n' => "Null",
            b't' => "True",
            b'f' => "False",
            _ => "Number",
        };
        return Err(Error::InvalidOperation(format!(
            "The requested operation requires an element of type 'Object', but the target element has type '{kind}'."
        )));
    }
    serde_json::from_str(raw.get()).map_err(|e| Error::Encode(e.to_string()))
}
fn string(scope: &Object, name: &str) -> String {
    scope
        .get(name)
        .and_then(|raw| serde_json::from_str(raw.get()).ok())
        .unwrap_or_default()
}
fn number(scope: &Object, name: &str) -> f64 {
    scope
        .get(name)
        .and_then(|raw| {
            raw.get().parse::<f64>().ok().or_else(|| {
                serde_json::from_str::<String>(raw.get())
                    .ok()
                    .and_then(|text| text.trim().parse().ok())
            })
        })
        .unwrap_or(0.0)
}
fn windows(
    scope: &Object,
    names: [&str; 3],
) -> Result<(Option<UsageWindow>, Option<UsageWindow>), Error> {
    let mut primary = window(scope, names[0])?;
    let mut secondary = None;
    if let Some(weekly) = window(scope, names[1])? {
        if primary.is_none() {
            primary = Some(weekly);
        } else {
            secondary = Some(weekly);
        }
    }
    Ok((primary.or(window(scope, names[2])?), secondary))
}
fn window(scope: &Object, name: &str) -> Result<Option<UsageWindow>, Error> {
    let Some(raw) = scope.get(name).filter(|raw| raw.get().starts_with('{')) else {
        return Ok(None);
    };
    let window = object(raw.get())?;
    let mut percent = number(&window, "percent");
    if percent == 0.0 {
        percent = number(&window, "usedPercent");
    }
    if percent == 0.0 {
        let remaining = number(&window, "remainingPercent");
        if remaining > 0.0 {
            percent = (100.0 - remaining).clamp(0.0, 100.0);
        }
    }
    let mut reset = parse_reset(&string(&window, "resetsAt"));
    if reset.is_none() || reset.is_some_and(|date| date.timestamp() == 0 && date.nanosecond() == 0)
    {
        reset = parse_reset(&string(&window, "resetAt"));
    }
    Ok(reset.map(|date| UsageWindow {
        used_percent: percent,
        reset_at: reset_text(date),
        limit_window_seconds: 0,
    }))
}
fn reset_text(date: DateTime<Utc>) -> String {
    let mut text = date.format("%Y-%m-%dT%H:%M:%S").to_string();
    if date.nanosecond() != 0 {
        text.push_str(&format!(".{:07}", date.nanosecond() / 100));
    }
    text.push_str("+00:00");
    text
}
fn valid_date(date: DateTime<Utc>) -> Option<DateTime<Utc>> {
    if date.nanosecond() >= 1_000_000_000 {
        return None;
    }
    // DateTimeOffset.TryParse rounds fractional input to 100ns ticks.
    let rounded = date.checked_add_signed(chrono::Duration::nanoseconds(50))?;
    let rounded = rounded.with_nanosecond(rounded.nanosecond() / 100 * 100)?;
    (1..=9999).contains(&rounded.year()).then_some(rounded)
}
fn valid_offset_date(date: DateTime<FixedOffset>) -> Option<DateTime<Utc>> {
    if date.offset().local_minus_utc().unsigned_abs() > 14 * 60 * 60 {
        return None;
    }
    valid_date(date.with_timezone(&Utc))
}
fn parse_reset(value: &str) -> Option<DateTime<Utc>> {
    if value.is_empty() {
        return DateTime::from_timestamp(0, 0);
    }
    let value = value.trim();
    if let Ok(date) = DateTime::parse_from_rfc3339(value) {
        return valid_offset_date(date);
    }
    if let Ok(date) = DateTime::parse_from_rfc2822(value) {
        return valid_offset_date(date);
    }
    for format in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%m/%d/%Y %H:%M:%S",
        "%m/%d/%Y %I:%M:%S %p",
        "%m/%d/%Y %I:%M %p",
    ] {
        if let Ok(date) = NaiveDateTime::parse_from_str(value, format) {
            return valid_date(date.and_utc());
        }
    }
    for format in ["%Y-%m-%d", "%m/%d/%Y"] {
        if let Ok(date) = NaiveDate::parse_from_str(value, format) {
            return date
                .and_hms_opt(0, 0, 0)
                .and_then(|date| valid_date(date.and_utc()));
        }
    }
    None
}
