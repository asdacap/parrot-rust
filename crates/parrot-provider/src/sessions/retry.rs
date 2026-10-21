//! Retry policy for failures before client-visible output. Transport/session
//! ownership and cancellable waiting remain with the caller.

use std::{fmt, time::Duration};

#[derive(Clone, Eq, PartialEq)]
pub enum Failure {
    HeaderTimeout,
    HeaderRetryLimit {
        maximum_retries: u32,
        attempts: u64,
    },
    Http {
        status: i32,
        error_type: String,
        code: String,
        detail: String,
    },
    Response {
        error_type: String,
        code: String,
        detail: String,
    },
    RequestTooLarge,
    Upgrade {
        status: i32,
    },
    WebSocketTransport,
    Configuration,
    Protocol,
    Network,
    Cancelled,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HeaderTimeout => f.write_str("provider response headers timed out"),
            Self::HeaderRetryLimit {
                maximum_retries,
                attempts,
            } => write!(
                f,
                "Provider header timeout retry limit exceeded ({maximum_retries} retries, {attempts} attempts)."
            ),
            Self::Http { status, .. } => write!(f, "provider returned HTTP {status}"),
            Self::Response { .. } => f.write_str("provider stream error"),
            Self::RequestTooLarge => f.write_str("provider request too large"),
            Self::Upgrade { status } => write!(f, "WebSocket upgrade returned HTTP {status}"),
            Self::WebSocketTransport => f.write_str("WebSocket transport failure"),
            Self::Configuration => f.write_str("provider configuration failure"),
            Self::Protocol => f.write_str("provider protocol failure"),
            Self::Network => f.write_str("provider network failure"),
            Self::Cancelled => f.write_str("provider call cancelled"),
        }
    }
}

impl fmt::Debug for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}
impl std::error::Error for Failure {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetryAction {
    pub attempt: u32,
    pub delay: Duration,
    pub reason: String,
    pub fallback_http: bool,
    pub max_output_tokens: i32,
}

/// One instance belongs to one logical call, including all of its attempts.
#[derive(Clone, Debug)]
pub struct RetryPolicy {
    maximum_tokens: i32,
    header_max_retries: u32,
    header_attempt: u32,
    overload_attempt: u32,
    stream_attempt: u32,
    context_adjusted: bool,
    output_emitted: bool,
}

impl RetryPolicy {
    pub fn new(max_output_tokens: i32, header_max_retries: u32) -> Self {
        Self {
            maximum_tokens: max_output_tokens,
            header_max_retries,
            header_attempt: 0,
            overload_attempt: 0,
            stream_attempt: 0,
            context_adjusted: false,
            output_emitted: false,
        }
    }

    pub fn mark_visible(&mut self) {
        self.output_emitted = true;
    }

    pub fn max_output_tokens(&self) -> i32 {
        self.maximum_tokens
    }

    /// Called only after the underlying session successfully selects HTTP.
    /// Header, overload, context-adjustment and visibility state are retained.
    pub fn reset_stream(&mut self) {
        self.stream_attempt = 0;
    }

    pub fn classify(&mut self, failure: &Failure, cancelled: bool) -> Result<RetryAction, Failure> {
        if self.output_emitted || cancelled || matches!(failure, Failure::Cancelled) {
            return Err(failure.clone());
        }
        if !self.context_adjusted {
            if let Some(reduced) = try_reduce_context_budget(failure, self.maximum_tokens) {
                self.context_adjusted = true;
                self.maximum_tokens = reduced;
                return Ok(self.action(
                    1,
                    Duration::ZERO,
                    "Context limit exceeded. Retrying with a reduced output token budget.".into(),
                    false,
                ));
            }
        }

        match failure {
            Failure::HeaderTimeout => {
                if self.header_attempt >= self.header_max_retries {
                    return Err(Failure::HeaderRetryLimit {
                        maximum_retries: self.header_max_retries,
                        attempts: u64::from(self.header_attempt) + 1,
                    });
                }
                let delay = slow_delay(self.header_attempt);
                self.header_attempt += 1;
                Ok(self.action(
                    self.header_attempt,
                    delay,
                    "Provider response headers timed out. Retrying.".into(),
                    false,
                ))
            }
            // HTTP overload precedes even usage/context classification in source.
            Failure::Http { .. } if is_engine_overloaded(failure) => self.overload(failure),
            Failure::Http { .. } | Failure::Response { .. }
                if is_usage_limit(failure) || is_context_length_exceeded(failure) =>
            {
                Err(failure.clone())
            }
            Failure::RequestTooLarge
            | Failure::Configuration
            | Failure::HeaderRetryLimit { .. } => Err(failure.clone()),
            Failure::Response { .. } if is_engine_overloaded(failure) => self.overload(failure),
            Failure::Http { status, .. } => {
                if retryable_status(*status) && self.take_stream() {
                    Ok(self.stream_action(format!("Provider returned HTTP {status}. Retrying.")))
                } else {
                    Err(failure.clone())
                }
            }
            Failure::Upgrade { status } => {
                if !retryable_status(*status) {
                    return Err(failure.clone());
                }
                if self.take_stream() {
                    Ok(self.stream_action(format!(
                        "WebSocket upgrade returned HTTP {status}. Retrying."
                    )))
                } else {
                    Ok(self.fallback())
                }
            }
            _ => {
                if self.take_stream() {
                    Ok(self
                        .stream_action("Provider connection or protocol failure. Retrying.".into()))
                } else if matches!(failure, Failure::WebSocketTransport) {
                    Ok(self.fallback())
                } else {
                    Err(failure.clone())
                }
            }
        }
    }

    fn action(
        &self,
        attempt: u32,
        delay: Duration,
        reason: String,
        fallback_http: bool,
    ) -> RetryAction {
        RetryAction {
            attempt,
            delay,
            reason,
            fallback_http,
            max_output_tokens: self.maximum_tokens,
        }
    }

    fn overload(&mut self, failure: &Failure) -> Result<RetryAction, Failure> {
        if self.overload_attempt == 5 {
            return Err(failure.clone());
        }
        self.overload_attempt += 1;
        Ok(self.action(
            self.overload_attempt,
            slow_delay(self.overload_attempt - 1),
            format!(
                "Provider servers are overloaded. Retrying (attempt {}/5).",
                self.overload_attempt
            ),
            false,
        ))
    }

    fn take_stream(&mut self) -> bool {
        if self.stream_attempt == 5 {
            false
        } else {
            self.stream_attempt += 1;
            true
        }
    }

    fn stream_action(&self, reason: String) -> RetryAction {
        self.action(
            self.stream_attempt,
            Duration::from_millis(200 << (self.stream_attempt - 1).min(7)),
            reason,
            false,
        )
    }

    fn fallback(&self) -> RetryAction {
        self.action(
            6,
            Duration::ZERO,
            "WebSocket retry budget exhausted. Retrying over HTTP.".into(),
            true,
        )
    }
}

fn slow_delay(attempt: u32) -> Duration {
    Duration::from_secs((2 << attempt.min(4)).min(30))
}

fn retryable_status(status: i32) -> bool {
    status == 429 || status >= 500
}

fn fields(failure: &Failure) -> Option<(&str, &str, &str)> {
    match failure {
        Failure::Http {
            error_type,
            code,
            detail,
            ..
        }
        | Failure::Response {
            error_type,
            code,
            detail,
        } => Some((error_type, code, detail)),
        _ => None,
    }
}

pub fn is_usage_limit(failure: &Failure) -> bool {
    fields(failure).is_some_and(|(kind, code, _)| {
        [kind, code].iter().any(|value| {
            matches!(
                value.trim().to_lowercase().as_str(),
                "usage_limit_reached"
                    | "usage_limit_exceeded"
                    | "insufficient_quota"
                    | "billing_hard_limit_reached"
            )
        })
    })
}

pub fn is_engine_overloaded(failure: &Failure) -> bool {
    if matches!(failure, Failure::Http { status: 503, .. }) {
        return true;
    }
    fields(failure).is_some_and(|(kind, code, detail)| {
        [kind, code].iter().any(|value| {
            matches!(
                value.trim().to_lowercase().as_str(),
                "engine_overloaded_error"
                    | "service_unavailable_error"
                    | "server_is_overloaded"
                    | "overloaded_error"
            )
        }) || retryable_provider_message(detail)
    })
}

pub fn is_context_length_exceeded(failure: &Failure) -> bool {
    fields(failure).is_some_and(|(kind, code, detail)| {
        [kind, code].iter().any(|value| {
            matches!(
                value.trim().to_lowercase().as_str(),
                "context_length_exceeded" | "context_window_exceeded"
            )
        }) || (matches!(
            failure,
            Failure::Response { .. } | Failure::Http { status: 400, .. }
        ) && context_counts(detail).is_some())
    })
}

pub fn try_reduce_context_budget(failure: &Failure, maximum_tokens: i32) -> Option<i32> {
    if maximum_tokens <= 0 || is_usage_limit(failure) {
        return None;
    }
    let detail = match failure {
        Failure::Http {
            status: 400,
            detail,
            ..
        }
        | Failure::Response { detail, .. } => detail,
        _ => return None,
    };
    let counts = context_counts(detail)?;
    let context = counts[0].parse::<i32>().ok()?;
    let total = counts[1].parse::<i32>().ok()?;
    let input = counts[2].parse::<i32>().ok()?;
    let completion = counts[3].parse::<i32>().ok()?;
    if i64::from(total) != i64::from(input) + i64::from(completion) || total <= context {
        return None;
    }
    let headroom = input / 20 + i32::from(input % 20 > 0);
    let available = i64::from(context) - i64::from(input) - i64::from(headroom);
    (available > 0 && available < i64::from(maximum_tokens)).then_some(available as i32)
}

fn retryable_provider_message(message: &str) -> bool {
    const PREFIX: &str = "an error occurred while processing your request. you can retry your request, or contact us through our help center at help.openai.com if the error persists. please include the request id ";
    const SUFFIX: &str = " in your message.";
    let normalized = message.trim().to_lowercase();
    normalized
        .strip_prefix(PREFIX)
        .and_then(|rest| rest.strip_suffix(SUFFIX))
        .is_some_and(|request_id| !request_id.trim().is_empty())
}

/// Finds the first complete ASCII-digit source pattern, just as Regex.Match
/// does; an overflowing first match is not replaced by a later valid match.
fn context_counts(detail: &str) -> Option<[&str; 4]> {
    const PREFIX: &str = "Requested token count exceeds the model's maximum context length of ";
    const DELIMITERS: [&str; 4] = [
        " tokens. You requested a total of ",
        " tokens: ",
        " tokens from the input messages and ",
        " tokens for the completion.",
    ];
    for (start, _) in detail.match_indices(PREFIX) {
        let mut rest = &detail[start + PREFIX.len()..];
        let mut counts = [""; 4];
        let mut matched = true;
        for (index, delimiter) in DELIMITERS.iter().enumerate() {
            let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
            if digits == 0 {
                matched = false;
                break;
            }
            counts[index] = &rest[..digits];
            let Some(after) = rest[digits..].strip_prefix(delimiter) else {
                matched = false;
                break;
            };
            rest = after;
        }
        if matched {
            return Some(counts);
        }
    }
    None
}
