//! The typed `GitHubApi` endpoint and its rate-limit classifier.
//!
//! GitHub signals rate limits off the status line: a `403` with
//! `x-ratelimit-remaining: 0`, secondary-limit / abuse-detection bodies, and
//! an error of the type `RATE_LIMITED` in the `200` of GitHub GraphQL, which
//! the SDK's default `429` handling cannot see. [`GitHubApi`] overrides
//! [`EndpointHooks::classify`] to map those to a typed `RateLimited` error, so
//! every `cx.endpoint(GitHubApi)` terminal arms the breaker and surfaces the
//! retry window. Every other status falls through to the SDK's default
//! `error_for_status` mapping.

use core::time::Duration;
use http::{HeaderMap, Response, StatusCode};
use omnifs_sdk::endpoint::EndpointHooks;
use omnifs_sdk::error::ProviderError;
use serde::Deserialize;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(omnifs_sdk::Endpoint)]
#[endpoint(
    base = "https://api.github.com",
    default_header = "X-GitHub-Api-Version: 2022-11-28",
    default_header = "Accept: application/vnd.github+json",
    hooks
)]
pub struct GitHubApi;

impl EndpointHooks for GitHubApi {
    /// Map a GitHub rate-limit response to a typed `RateLimited` error so the
    /// endpoint breaker arms on it, or to the error of a header of its window
    /// that does not parse; `None` for everything else, leaving the SDK's
    /// default 4xx/5xx mapping in charge.
    fn classify(&self, resp: &Response<Vec<u8>>) -> Option<ProviderError> {
        if !is_rate_limited(resp) {
            return None;
        }
        Some(match retry_window(resp.headers()) {
            Ok(window) => {
                ProviderError::rate_limited(rate_limit_message(resp)).with_retry_after(Some(window))
            },
            Err(error) => error,
        })
    }
}

/// The window GitHub asks for after a secondary rate limit without
/// `Retry-After`: at least one minute.
const SECONDARY_WINDOW: Duration = Duration::from_secs(60);

/// The backoff window of a rate-limited answer from its `headers`, as GitHub
/// asks: the seconds of `Retry-After`; else, for the primary limit, which
/// `x-ratelimit-remaining: 0` says, the `x-ratelimit-reset` epoch minus now,
/// zero for a reset that passed; else, for a secondary limit, one minute. A
/// header of the window that is not text, or that does not parse as seconds,
/// fails, as does a primary limit without its reset.
fn retry_window(headers: &HeaderMap) -> Result<Duration, ProviderError> {
    let seconds = |name: &str| -> Result<Option<u64>, ProviderError> {
        let Some(value) = headers.get(name) else {
            return Ok(None);
        };
        let text = value.to_str().map_err(|error| {
            ProviderError::invalid_input(format!("the header {name} is not text: {error}"))
        })?;
        text.trim().parse::<u64>().map(Some).map_err(|error| {
            ProviderError::invalid_input(format!("the header {name} {text:?}: {error}"))
        })
    };
    if let Some(secs) = seconds("retry-after")? {
        return Ok(Duration::from_secs(secs));
    }
    if seconds("x-ratelimit-remaining")? != Some(0) {
        return Ok(SECONDARY_WINDOW);
    }
    let reset = seconds("x-ratelimit-reset")?.ok_or_else(|| {
        ProviderError::invalid_input("a primary rate limit without x-ratelimit-reset")
    })?;
    // The reset is a second of the epoch: the window counts whole seconds to
    // it, so that it never ends before the reset.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| ProviderError::internal(format!("the clock: {error}")))?;
    Ok(Duration::from_secs(reset.saturating_sub(now.as_secs())))
}

fn is_rate_limited(resp: &Response<Vec<u8>>) -> bool {
    match resp.status() {
        StatusCode::TOO_MANY_REQUESTS => true,
        StatusCode::FORBIDDEN => {
            let zero_remaining = resp.headers().iter().any(|(name, value)| {
                name.as_str().eq_ignore_ascii_case("x-ratelimit-remaining") && value == "0"
            });
            if zero_remaining {
                return true;
            }
            let body = String::from_utf8_lossy(resp.body()).to_ascii_lowercase();
            body.contains("rate limit") || body.contains("abuse detection")
        },
        StatusCode::OK => graphql_rate_limited(resp.body()),
        _ => false,
    }
}

/// Whether `body` is an answer of GitHub GraphQL with an error of the type
/// `RATE_LIMITED`. Only a body with that text is parsed.
fn graphql_rate_limited(body: &[u8]) -> bool {
    #[derive(Deserialize)]
    struct Answer {
        errors: Vec<Error>,
    }
    #[derive(Deserialize)]
    struct Error {
        #[serde(rename = "type")]
        kind: Option<String>,
    }
    const RATE_LIMITED: &str = "RATE_LIMITED";
    body.windows(RATE_LIMITED.len())
        .any(|window| window == RATE_LIMITED.as_bytes())
        && serde_json::from_slice::<Answer>(body).is_ok_and(|answer| {
            answer
                .errors
                .iter()
                .any(|error| error.kind.as_deref() == Some(RATE_LIMITED))
        })
}

fn rate_limit_message(resp: &Response<Vec<u8>>) -> String {
    let mut message = format!("GitHub API rate limited: HTTP {}", resp.status().as_u16());
    append_header_hint(resp, &mut message, "retry-after", "retry_after");
    append_header_hint(resp, &mut message, "x-ratelimit-reset", "reset_epoch");
    append_header_hint(resp, &mut message, "x-ratelimit-resource", "resource");
    message
}

fn append_header_hint(resp: &Response<Vec<u8>>, message: &mut String, header: &str, label: &str) {
    if let Some(value) = resp
        .headers()
        .get(header)
        .and_then(|value| value.to_str().ok())
    {
        message.push_str("; ");
        message.push_str(label);
        message.push('=');
        message.push_str(value);
    }
}

#[cfg(test)]
mod tests {
    use super::{SECONDARY_WINDOW, retry_window};
    use http::{HeaderMap, HeaderValue};

    /// A header of the window of a rate limit that is not text, or that does
    /// not parse as seconds, fails, as does a primary limit without its
    /// reset; a secondary limit without `Retry-After` waits a minute,
    /// whatever the reset of the primary limit.
    #[test]
    fn the_window_of_a_rate_limit_follows_its_headers() {
        let headers = |pairs: &[(&'static str, HeaderValue)]| {
            let mut headers = HeaderMap::new();
            for (name, value) in pairs {
                headers.insert(*name, value.clone());
            }
            headers
        };
        let zero = HeaderValue::from_static("0");
        for invalid in [
            headers(&[("retry-after", HeaderValue::from_bytes(b"2\xff").unwrap())]),
            headers(&[
                ("x-ratelimit-remaining", zero.clone()),
                ("x-ratelimit-reset", HeaderValue::from_static("soon")),
            ]),
            headers(&[("x-ratelimit-remaining", zero)]),
        ] {
            assert!(retry_window(&invalid).is_err(), "{invalid:?}");
        }
        for secondary in [
            headers(&[]),
            headers(&[
                ("x-ratelimit-remaining", HeaderValue::from_static("4000")),
                ("x-ratelimit-reset", HeaderValue::from_static("0")),
            ]),
        ] {
            assert_eq!(retry_window(&secondary).unwrap(), SECONDARY_WINDOW);
        }
    }
}
