//! Shared HTTP transport hardening for LLM API access.
//!
//! LLM API calls occasionally stall: a request is sent over the wire but no
//! response (not even an error) ever comes back. Without timeouts, such a call
//! hangs forever — the ask never terminates, the agent lock is held, and the
//! GUI tab stays busy with no output. This module bounds every phase of the
//! request and keeps retrying retryable failures (transport errors and rate
//! limits) with capped waits, so transient stalls and short quota windows
//! self-heal.

use futures::StreamExt;
use reqwest::Client;
use std::future::Future;
use std::time::Duration;

use gewu_core::traits::StatusSink;
use serde_json::Value;

/// Connect timeout for the initial TCP/TLS handshake.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum time to wait between bytes once the response has started.
///
/// This is an *idle* timeout, not a total timeout: a healthy stream that keeps
/// producing tokens (or SSE keep-alive comments) never trips it, but a stalled
/// connection is bounded instead of hanging forever.
const READ_TIMEOUT: Duration = Duration::from_secs(120);

/// Base delay between retry attempts; each retry waits four times longer than
/// the previous one (1s, 4s, 16s, ...).
pub(crate) const RETRY_BACKOFF: Duration = Duration::from_secs(1);

/// Upper bound on a single retry wait.
///
/// Retryable failures — transport errors and rate limits alike — retry
/// indefinitely, but never sleep longer than this per attempt: a provider
/// suggesting an hour-long `Retry-After` is retried every 5 minutes instead,
/// and the backoff sequence clamps at the same ceiling. A turn stuck in this
/// loop is still bounded by an explicit interrupt.
pub(crate) const MAX_RETRY_WAIT: Duration = Duration::from_secs(300);

/// Whether an error should be retried, and optionally how long to wait first.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum RetryDecision {
    /// Permanent failure; return the error.
    No,
    /// Retryable. `after` is a provider-suggested wait (e.g. a 429
    /// `Retry-After` header); `None` uses the exponential backoff sequence.
    /// Both are clamped to [`MAX_RETRY_WAIT`].
    Yes { after: Option<Duration> },
}

/// Builds an HTTP client with timeouts suited for LLM API streaming.
///
/// Deliberately no total `timeout()`: streaming completions can legitimately
/// run for many minutes, so only the connect phase and inter-read gaps are
/// bounded.
pub(crate) fn client() -> Client {
    Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .build()
        .expect("static HTTP client configuration should never fail")
}

/// [`with_retries_capped`] with the standard per-wait cap, so tests don't sleep
/// minutes.
pub(crate) async fn with_retries_capped<T, E, Fut, F>(
    backoff: Duration,
    max_wait: Duration,
    mut op: F,
    is_retryable: fn(&E) -> RetryDecision,
    notify: Option<&StatusSink>,
) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        match op().await {
            Ok(value) => return Ok(value),
            Err(e) => match is_retryable(&e) {
                RetryDecision::No => return Err(e),
                RetryDecision::Yes { after } => {
                    let delay = match after {
                        Some(hint) => hint.min(max_wait),
                        // Saturating: once the sequence passes the cap the
                        // product would overflow; `min` clamps it anyway.
                        None => backoff
                            .saturating_mul(4u32.saturating_pow(attempt - 1))
                            .min(max_wait),
                    };
                    tracing::warn!(
                        attempt,
                        delay_secs = delay.as_secs(),
                        capped = after.is_some_and(|hint| hint > max_wait),
                        error = %e,
                        "retryable LLM request failure; retrying"
                    );
                    // Surface the backoff to frontends: without this a
                    // retry wait is indistinguishable from a silent hang.
                    if let Some(notify) = notify {
                        notify(format!(
                            "LLM request failed: {e}; retrying in {}s (attempt {attempt})",
                            delay.as_secs()
                        ));
                    }
                    tokio::time::sleep(delay).await;
                }
            },
        }
    }
}

/// A stream that restarts its underlying request from scratch while the
/// failure is retryable and nothing has been yielded yet.
///
/// Mid-stream failures — an in-flight `error` SSE event (a provider
/// overload, say) or a dropped connection — would otherwise kill the turn.
/// Restarting is only safe before any visible output: once content or
/// reasoning deltas reached the consumer, a fresh request would duplicate
/// them, so late failures surface as-is.
///
/// Waits follow the same policy as [`with_retries_capped`]: the provider's
/// hint when present, else the exponential backoff sequence, both clamped to
/// `max_wait`; retrying is unbounded and bounded externally by an explicit
/// interrupt.
pub(crate) fn retrying_stream<T, E, F, S>(
    mut make_stream: F,
    is_retryable: fn(&E) -> RetryDecision,
    notify: Option<StatusSink>,
    backoff: Duration,
    max_wait: Duration,
) -> impl futures::Stream<Item = Result<T, E>>
where
    F: FnMut() -> S,
    S: futures::Stream<Item = Result<T, E>>,
    E: std::fmt::Display,
{
    async_stream::try_stream! {
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            let mut inner = Box::pin(make_stream());
            let mut forwarded = 0usize;
            let mut restart = false;
            while let Some(item) = inner.next().await {
                match item {
                    Ok(event) => {
                        forwarded += 1;
                        yield event;
                    }
                    Err(error) => match is_retryable(&error) {
                        RetryDecision::No => Err::<(), E>(error)?,
                        // Retryable, but output already went out: a restart
                        // would duplicate what the consumer has seen.
                        RetryDecision::Yes { .. } if forwarded > 0 => Err::<(), E>(error)?,
                        RetryDecision::Yes { after } => {
                            let delay = match after {
                                Some(hint) => hint.min(max_wait),
                                None => backoff
                                    .saturating_mul(4u32.saturating_pow(attempt - 1))
                                    .min(max_wait),
                            };
                            tracing::warn!(
                                attempt,
                                delay_secs = delay.as_secs(),
                                capped = after.is_some_and(|hint| hint > max_wait),
                                error = %error,
                                "retryable LLM stream failure; restarting"
                            );
                            if let Some(notify) = &notify {
                                notify(format!(
                                    "LLM request failed: {error}; retrying in {}s (attempt {attempt})",
                                    delay.as_secs()
                                ));
                            }
                            tokio::time::sleep(delay).await;
                            restart = true;
                            break;
                        }
                    },
                }
            }
            if !restart {
                break;
            }
        }
    }
}

/// The provider-suggested wait from a 429 response's `Retry-After` header.
///
/// Only integral seconds are understood (the common LLM-gateway form); HTTP
/// dates yield `None` rather than a mis-parsed delay. Call before consuming
/// the response body.
pub(crate) fn retry_after(response: &reqwest::Response) -> Option<Duration> {
    let header = response.headers().get(reqwest::header::RETRY_AFTER)?;
    parse_retry_after(header.to_str().ok()?)
}

/// Parses a raw `Retry-After` header value: integral seconds only.
fn parse_retry_after(value: &str) -> Option<Duration> {
    value.trim().parse::<u64>().ok().map(Duration::from_secs)
}

/// Human message for a 429 body, in preference order:
/// `error.message` (OpenAI and new-api relays), `message`, then the raw body
/// truncated to one line so the error stays readable.
pub(crate) fn rate_limit_message(body: &str) -> String {
    let parsed: Option<serde_json::Value> = serde_json::from_str(body).ok();
    let extracted = parsed.as_ref().and_then(|value| {
        value
            .pointer("/error/message")
            .or_else(|| value.get("message"))
            .and_then(Value::as_str)
            .map(str::to_string)
    });
    match extracted {
        Some(message) => message,
        None => {
            let mut flat = body.trim().replace(['\n', '\r'], " ");
            if flat.chars().count() > 200 {
                flat = flat.chars().take(200).collect::<String>() + "…";
            }
            if flat.is_empty() {
                "no response body".to_string()
            } else {
                flat
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Backoff small enough that retry sleeps don't slow the test suite.
    const TEST_BACKOFF: Duration = Duration::from_millis(1);

    /// Counts calls and fails with a "transient" error for the first
    /// `failures` attempts.
    fn counting_op(
        calls: Arc<AtomicUsize>,
        failures: usize,
    ) -> impl FnMut() -> std::pin::Pin<Box<dyn Future<Output = Result<u32, String>> + Send>> {
        move || {
            let calls = calls.clone();
            Box::pin(async move {
                let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                if n <= failures {
                    Err(format!("transient {n}"))
                } else {
                    Ok(n as u32)
                }
            })
        }
    }

    #[tokio::test]
    async fn retries_transient_failures_then_succeeds() {
        let calls = Arc::new(AtomicUsize::new(0));
        let result: Result<u32, String> = with_retries_capped(
            TEST_BACKOFF,
            MAX_RETRY_WAIT,
            counting_op(calls.clone(), 2),
            |e| {
                if e.starts_with("transient") {
                    RetryDecision::Yes { after: None }
                } else {
                    RetryDecision::No
                }
            },
            None,
        )
        .await;
        assert_eq!(result.unwrap(), 3);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn retry_backoff_is_reported_to_the_status_sink() {
        let calls = Arc::new(AtomicUsize::new(0));
        let statuses = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink_statuses = statuses.clone();
        let sink: StatusSink = Arc::new(move |message: String| {
            sink_statuses.lock().unwrap().push(message);
        });
        let result: Result<u32, String> = with_retries_capped(
            TEST_BACKOFF,
            MAX_RETRY_WAIT,
            counting_op(calls.clone(), 2),
            |e| {
                if e.starts_with("transient") {
                    RetryDecision::Yes { after: None }
                } else {
                    RetryDecision::No
                }
            },
            Some(&sink),
        )
        .await;
        assert_eq!(result.unwrap(), 3);
        let reported = statuses.lock().unwrap();
        assert_eq!(reported.len(), 2, "one status per failed attempt");
        assert!(reported[0].contains("transient 1"), "{}", reported[0]);
        assert!(reported[0].contains("attempt 1"), "{}", reported[0]);
        assert!(reported[1].contains("attempt 2"), "{}", reported[1]);
    }

    #[tokio::test]
    async fn does_not_retry_permanent_failures() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let result: Result<u32, String> = with_retries_capped(
            TEST_BACKOFF,
            MAX_RETRY_WAIT,
            || {
                let c = c.clone();
                Box::pin(async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Err::<u32, _>("fatal: bad request".to_string())
                })
                    as std::pin::Pin<Box<dyn Future<Output = Result<u32, String>> + Send>>
            },
            |e| {
                if e.starts_with("transient") {
                    RetryDecision::Yes { after: None }
                } else {
                    RetryDecision::No
                }
            },
            None,
        )
        .await;
        assert_eq!(result.unwrap_err(), "fatal: bad request");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn transport_failures_retry_indefinitely_until_success() {
        // Far more failures than the old attempt cap allowed.
        let calls = Arc::new(AtomicUsize::new(0));
        let result: Result<u32, String> = with_retries_capped(
            TEST_BACKOFF,
            TEST_BACKOFF,
            counting_op(calls.clone(), 7),
            |e| {
                if e.starts_with("transient") {
                    RetryDecision::Yes { after: None }
                } else {
                    RetryDecision::No
                }
            },
            None,
        )
        .await;
        assert_eq!(result.unwrap(), 8);
        assert_eq!(calls.load(Ordering::SeqCst), 8);
    }

    #[tokio::test]
    async fn rate_limit_hint_shorter_than_the_cap_is_honored() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let result: Result<u32, String> = with_retries_capped(
            Duration::from_secs(999), // must not be used: the hint wins
            MAX_RETRY_WAIT,
            move || {
                let c = c.clone();
                Box::pin(async move {
                    let n = c.fetch_add(1, Ordering::SeqCst) + 1;
                    if n == 1 {
                        Err::<u32, _>("rate limited".to_string())
                    } else {
                        Ok(n as u32)
                    }
                })
                    as std::pin::Pin<Box<dyn Future<Output = Result<u32, String>> + Send>>
            },
            |e| {
                if e.contains("rate limited") {
                    RetryDecision::Yes {
                        after: Some(Duration::from_millis(1)),
                    }
                } else {
                    RetryDecision::No
                }
            },
            None,
        )
        .await;
        assert_eq!(result.unwrap(), 2);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn rate_limit_hint_longer_than_the_cap_is_clamped_and_retried() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let result: Result<u32, String> = with_retries_capped(
            TEST_BACKOFF,
            Duration::from_millis(1), // tiny injected cap: a 3600s hint clamps to this
            move || {
                let c = c.clone();
                Box::pin(async move {
                    let n = c.fetch_add(1, Ordering::SeqCst) + 1;
                    if n == 1 {
                        Err::<u32, _>("quota exhausted for an hour".to_string())
                    } else {
                        Ok(n as u32)
                    }
                })
                    as std::pin::Pin<Box<dyn Future<Output = Result<u32, String>> + Send>>
            },
            |e| {
                if e.contains("quota") {
                    RetryDecision::Yes {
                        after: Some(Duration::from_secs(3600)),
                    }
                } else {
                    RetryDecision::No
                }
            },
            None,
        )
        .await;
        assert_eq!(
            result.unwrap(),
            2,
            "a hint beyond the cap must clamp, not fail"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn backoff_growth_clamps_at_the_cap() {
        let calls = Arc::new(AtomicUsize::new(0));
        // Cap smaller than the third backoff step (1ms * 4^2 = 16ms), so the
        // sequence must be truncated rather than exploding.
        let result: Result<u32, String> = with_retries_capped(
            Duration::from_millis(1),
            Duration::from_millis(2),
            counting_op(calls.clone(), 5),
            |e| {
                if e.starts_with("transient") {
                    RetryDecision::Yes { after: None }
                } else {
                    RetryDecision::No
                }
            },
            None,
        )
        .await;
        assert_eq!(result.unwrap(), 6);
        assert_eq!(calls.load(Ordering::SeqCst), 6);
    }

    #[test]
    fn retry_after_header_parses_seconds_only() {
        // Integral seconds are the common LLM-gateway form.
        assert_eq!(parse_retry_after("45"), Some(Duration::from_secs(45)));
        assert_eq!(parse_retry_after(" 45 "), Some(Duration::from_secs(45)));
        // HTTP-date and garbage forms yield no delay rather than a mis-parse.
        assert_eq!(parse_retry_after("Wed, 21 Oct 2026 07:28:00 GMT"), None);
        assert_eq!(parse_retry_after("soon"), None);
        assert_eq!(parse_retry_after(""), None);
    }

    #[test]
    fn rate_limit_message_prefers_structured_fields() {
        let new_api = r#"{"error":{"code":"user_rate_limit_exceeded","message":"You have reached the request limit: at most 1 requests per 60 minutes","type":"new_api_error"}}"#;
        assert_eq!(
            rate_limit_message(new_api),
            "You have reached the request limit: at most 1 requests per 60 minutes"
        );
        assert_eq!(
            rate_limit_message(r#"{"message":"slow down"}"#),
            "slow down"
        );
        // Unparseable bodies are flattened and truncated, not dropped.
        let raw = format!("oops\n{}", "x".repeat(400));
        let message = rate_limit_message(&raw);
        assert!(!message.contains('\n'));
        assert!(message.chars().count() <= 201);
        assert_eq!(rate_limit_message(""), "no response body");
    }

    // -- retrying_stream ----------------------------------------------------

    type StreamError = Box<dyn std::error::Error + Send + Sync>;

    /// Serves one canned attempt stream per call, counting invocations.
    fn canned_streams(
        attempts: Vec<Vec<Result<u32, StreamError>>>,
    ) -> (
        impl FnMut() -> futures::stream::BoxStream<'static, Result<u32, StreamError>>,
        Arc<AtomicUsize>,
    ) {
        let mut attempts = std::collections::VecDeque::from(attempts);
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = calls.clone();
        let make = move || {
            calls_clone.fetch_add(1, Ordering::SeqCst);
            futures::stream::iter(attempts.pop_front().unwrap_or_default()).boxed()
        };
        (make, calls)
    }

    fn retry_on_overloaded(error: &StreamError) -> RetryDecision {
        if error.to_string().contains("overloaded") {
            RetryDecision::Yes { after: None }
        } else {
            RetryDecision::No
        }
    }

    #[tokio::test]
    async fn retrying_stream_restarts_transient_failures_before_output() {
        let (make, calls) = canned_streams(vec![
            vec![Err("server overloaded".into())],
            vec![Ok(1), Ok(2)],
        ]);
        let events: Vec<_> = Box::pin(retrying_stream(
            make,
            retry_on_overloaded,
            None,
            TEST_BACKOFF,
            MAX_RETRY_WAIT,
        ))
        .collect()
        .await;
        assert!(matches!(events.as_slice(), [Ok(1), Ok(2)]), "{events:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn retrying_stream_surfaces_permanent_failures_without_restart() {
        let (make, calls) = canned_streams(vec![vec![Err("fatal: bad request".into())]]);
        let events: Vec<_> = Box::pin(retrying_stream(
            make,
            retry_on_overloaded,
            None,
            TEST_BACKOFF,
            MAX_RETRY_WAIT,
        ))
        .collect()
        .await;
        assert!(matches!(events.as_slice(), [Err(e)] if e.to_string() == "fatal: bad request"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retrying_stream_never_restarts_after_visible_output() {
        // A failure that arrives after events were yielded must surface:
        // a restart would duplicate what the consumer already saw.
        let (make, calls) = canned_streams(vec![vec![
            Ok(1),
            Err("server overloaded mid-stream".into()),
        ]]);
        let events: Vec<_> = Box::pin(retrying_stream(
            make,
            retry_on_overloaded,
            None,
            TEST_BACKOFF,
            MAX_RETRY_WAIT,
        ))
        .collect()
        .await;
        assert!(
            matches!(events.as_slice(), [Ok(1), Err(e)] if e.to_string().contains("overloaded")),
            "{events:?}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retrying_stream_reports_the_backoff_to_the_status_sink() {
        let (make, calls) =
            canned_streams(vec![vec![Err("server overloaded".into())], vec![Ok(7)]]);
        let statuses = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink_statuses = statuses.clone();
        let sink: StatusSink = Arc::new(move |message: String| {
            sink_statuses.lock().unwrap().push(message);
        });
        let events: Vec<_> = Box::pin(retrying_stream(
            make,
            retry_on_overloaded,
            Some(sink),
            TEST_BACKOFF,
            MAX_RETRY_WAIT,
        ))
        .collect()
        .await;
        assert!(matches!(events.as_slice(), [Ok(7)]), "{events:?}");
        let reported = statuses.lock().unwrap();
        assert_eq!(reported.len(), 1);
        assert!(reported[0].contains("overloaded"), "{}", reported[0]);
        assert!(reported[0].contains("attempt 1"), "{}", reported[0]);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
