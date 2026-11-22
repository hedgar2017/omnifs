//! The typed endpoints of Slack, the answers of its methods, and the objects
//! of its answers.
//!
//! A method of Slack answers a failure in its JSON, with `ok` of `false` and
//! its `error`, and a rate limit with the status `429` and the seconds of its
//! `Retry-After`; the endpoints have no hooks.

use std::time::Duration;

use omnifs_sdk::endpoint::HttpResponse;
use omnifs_sdk::error::ProviderError;
use serde::Deserialize;
use serde_json::value::RawValue;

use crate::{Id, Result, Ts};

/// The Web API of Slack.
#[derive(omnifs_sdk::Endpoint)]
#[endpoint(base = "https://slack.com")]
pub(crate) struct SlackApi;

/// The host of the bytes of the files of Slack.
#[derive(omnifs_sdk::Endpoint)]
#[endpoint(base = "https://files.slack.com")]
pub(crate) struct SlackFiles;

/// What an answer of a method of Slack has beside its objects.
#[derive(Deserialize)]
struct Envelope {
    ok: bool,
    error: Option<String>,
    response_metadata: Option<Metadata>,
}

#[derive(Deserialize)]
struct Metadata {
    next_cursor: Option<String>,
}

/// An answer of a method of Slack: a page, with the cursor of the next one,
/// or the `error` of an answer with `ok` of `false`.
pub(crate) enum Answer {
    Page { next: Option<String> },
    Error(String),
}

/// The answer of a method of Slack in `response`: the rate-limited error with
/// the seconds of its `Retry-After` for `429`, the error of the status for
/// another failing one without the JSON of Slack, the `error` of `ok` of
/// `false`, and else a page with its `response_metadata.next_cursor`, none
/// when empty or absent.
pub(crate) fn answer(response: &HttpResponse) -> Result<Answer> {
    let status = response.status();
    if status == 429 {
        return Err(rate_limited(response.header("retry-after")));
    }
    let envelope: Envelope = match serde_json::from_slice(response.body()) {
        Ok(envelope) => envelope,
        Err(_) if status >= 400 => return Err(ProviderError::from_http_status(status)),
        Err(error) => {
            return Err(ProviderError::invalid_input(format!(
                "an answer of Slack: {error}"
            )));
        },
    };
    if !envelope.ok {
        return envelope.error.map(Answer::Error).ok_or_else(|| {
            ProviderError::invalid_input("an answer of Slack with ok of false has no error")
        });
    }
    Ok(Answer::Page {
        next: envelope
            .response_metadata
            .and_then(|metadata| metadata.next_cursor)
            .filter(|cursor| !cursor.is_empty()),
    })
}

/// The failure of a request that Slack answered with the error `error`.
pub(crate) fn failure(error: &str) -> ProviderError {
    ProviderError::denied(format!("Slack answered the error {error}"))
}

/// The rate-limited error of an answer `429` with the seconds of its
/// `Retry-After`.
fn rate_limited(retry_after: Option<&str>) -> ProviderError {
    let message = match retry_after {
        Some(value) => format!("HTTP 429; retry_after={value}"),
        None => "HTTP 429".to_owned(),
    };
    ProviderError::rate_limited(message).with_retry_after(
        retry_after
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(Duration::from_secs),
    )
}

/// Decode the JSON of an answer of Slack.
pub(crate) fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T> {
    serde_json::from_slice(bytes)
        .map_err(|error| ProviderError::invalid_input(format!("an answer of Slack: {error}")))
}

/// The users of a page of `users.list`, as they came.
#[derive(Deserialize)]
pub(crate) struct Members {
    pub(crate) members: Vec<Box<RawValue>>,
}

/// The conversations of a page of `users.conversations`, as they came.
#[derive(Deserialize)]
pub(crate) struct Conversations {
    pub(crate) channels: Vec<Box<RawValue>>,
}

/// The messages of a page of `conversations.history` or
/// `conversations.replies`, as they came.
#[derive(Deserialize)]
pub(crate) struct Messages {
    pub(crate) messages: Vec<Box<RawValue>>,
}

/// What the provider reads of a user.
#[derive(Deserialize)]
pub(crate) struct UserObject {
    pub(crate) id: Id,
    profile: Profile,
}

#[derive(Deserialize)]
struct Profile {
    real_name: Option<String>,
}

impl UserObject {
    /// The `real_name` of its `profile`, empty for a user without one.
    pub(crate) fn name(&self) -> &str {
        self.profile.real_name.as_deref().unwrap_or_default()
    }
}

/// What the provider reads of a conversation.
#[derive(Deserialize)]
pub(crate) struct ConversationObject {
    pub(crate) id: Id,
    pub(crate) name: String,
}

/// What the provider reads of a message or a reply.
#[derive(Deserialize)]
pub(crate) struct MessageObject {
    pub(crate) ts: Ts,
    pub(crate) user: Option<String>,
    pub(crate) text: String,
    pub(crate) thread_ts: Option<Ts>,
    pub(crate) reply_count: Option<u64>,
    pub(crate) latest_reply: Option<Ts>,
    files: Option<Vec<FileObject>>,
}

impl MessageObject {
    /// Its files with a `url_private_download`, with that URL.
    pub(crate) fn downloads(&self) -> impl Iterator<Item = (&FileObject, &str)> {
        self.files
            .iter()
            .flatten()
            .filter_map(|file| Some((file, file.url_private_download.as_deref()?)))
    }
}

/// What the provider reads of a file of a message or a reply.
#[derive(Deserialize)]
pub(crate) struct FileObject {
    pub(crate) id: Id,
    pub(crate) name: Option<String>,
    url_private_download: Option<String>,
    pub(crate) edit_timestamp: Option<u64>,
}
