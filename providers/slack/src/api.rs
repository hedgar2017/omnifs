//! The typed endpoints of Slack, the answers of its methods, and the objects
//! of its answers.
//!
//! A method of Slack answers a failure in its JSON, with `ok` of `false` and
//! its `error`, and a rate limit with the status `429` and the seconds of its
//! `Retry-After`.

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

/// An answer of a method of Slack: `ok`, the `error` of `ok` of `false`, the
/// cursor of the next page, and the objects of the page, under the name that
/// the method gives them.
#[derive(Deserialize)]
struct Envelope {
    ok: bool,
    error: Option<String>,
    response_metadata: Option<Metadata>,
    #[serde(alias = "members", alias = "channels", alias = "messages")]
    objects: Option<Vec<Box<RawValue>>>,
}

#[derive(Deserialize)]
struct Metadata {
    next_cursor: Option<String>,
}

/// A page of a method of Slack: its objects as they came, and the cursor of
/// the next page.
pub(crate) struct Page {
    pub(crate) objects: Vec<Box<RawValue>>,
    pub(crate) next: Option<String>,
}

/// What Slack refused a request for.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refused {
    /// The channel, which the bot sees no more.
    Channel,
    /// The thread, whose message Slack has no more.
    Thread,
    /// The cursor, which Slack takes no more.
    Cursor,
    /// The request itself.
    Request,
}

/// A request without a page: what Slack refused it for, and its error for
/// the operation that it fails.
pub(crate) struct Refusal {
    pub(crate) of: Refused,
    pub(crate) error: ProviderError,
}

impl From<ProviderError> for Refusal {
    fn from(error: ProviderError) -> Self {
        Self {
            of: Refused::Request,
            error,
        }
    }
}

/// The page of a method of Slack in `response`, with its
/// `response_metadata.next_cursor`, none when empty or absent; else its
/// refusal: the rate-limited error with the seconds of its `Retry-After` for
/// `429`, the error of the status for another failing one without the JSON
/// of Slack, and the refusal of the `error` of `ok` of `false`.
pub(crate) fn page(response: &HttpResponse) -> std::result::Result<Page, Refusal> {
    let status = response.status();
    if status == 429 {
        return Err(rate_limited(response.header("retry-after")).into());
    }
    let envelope: Envelope = match serde_json::from_slice(response.body()) {
        Ok(envelope) => envelope,
        Err(_) if status >= 400 => return Err(ProviderError::from_http_status(status).into()),
        Err(error) => return Err(undecoded(&error).into()),
    };
    if !envelope.ok {
        let error = envelope.error.ok_or_else(|| {
            ProviderError::invalid_input("an answer of Slack with ok of false has no error")
        })?;
        return Err(refusal(&error));
    }
    let objects = envelope.objects.ok_or_else(|| {
        ProviderError::invalid_input("an answer of Slack with ok of true has no objects")
    })?;
    Ok(Page {
        objects,
        next: envelope
            .response_metadata
            .and_then(|metadata| metadata.next_cursor)
            .filter(|cursor| !cursor.is_empty()),
    })
}

/// The refusal of a request that Slack answered with the error `error`, with
/// the kind of that error: not found for a channel or a thread that the bot
/// sees no more, a timeout for a cursor that Slack takes no more, a rate
/// limit, a permission error for a token that Slack takes no more, a denial
/// for a token without the right of the request, a failure of the network for
/// a failure of Slack itself, an invalid input for a request that Slack does
/// not take, and an internal error for each other one.
fn refusal(error: &str) -> Refusal {
    let message = format!("Slack answered the error {error}");
    let (of, error) = match error {
        "channel_not_found" | "not_in_channel" => {
            (Refused::Channel, ProviderError::not_found(message))
        },
        "thread_not_found" => (Refused::Thread, ProviderError::not_found(message)),
        "invalid_cursor" => (Refused::Cursor, ProviderError::timeout(message)),
        "ratelimited" => (Refused::Request, ProviderError::rate_limited(message)),
        "invalid_auth" | "not_authed" | "account_inactive" | "token_revoked" | "token_expired" => {
            (Refused::Request, ProviderError::permission_denied(message))
        },
        "missing_scope"
        | "no_permission"
        | "not_allowed_token_type"
        | "access_denied"
        | "ekm_access_denied"
        | "org_login_required"
        | "team_access_not_granted"
        | "enterprise_is_restricted"
        | "two_factor_setup_required" => (Refused::Request, ProviderError::denied(message)),
        "internal_error" | "fatal_error" | "service_unavailable" | "team_added_to_org" => {
            (Refused::Request, ProviderError::network(message))
        },
        "request_timeout" => (Refused::Request, ProviderError::timeout(message)),
        "invalid_arguments"
        | "invalid_arg_name"
        | "invalid_array_arg"
        | "invalid_charset"
        | "invalid_form_data"
        | "invalid_post_type"
        | "missing_post_type"
        | "invalid_limit"
        | "invalid_ts_latest"
        | "invalid_ts_oldest"
        | "invalid_types"
        | "method_deprecated"
        | "deprecated_endpoint" => (Refused::Request, ProviderError::invalid_input(message)),
        _ => (Refused::Request, ProviderError::internal(message)),
    };
    Refusal { of, error }
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

fn undecoded(error: &serde_json::Error) -> ProviderError {
    ProviderError::invalid_input(format!("an answer of Slack: {error}"))
}

/// Decode the JSON of an object of an answer of Slack.
pub(crate) fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|error| undecoded(&error))
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
    reply_count: Option<u64>,
    latest_reply: Option<Ts>,
    files: Option<Vec<FileObject>>,
}

impl MessageObject {
    /// Whether it has replies: a `reply_count` above `0`.
    pub(crate) fn has_replies(&self) -> bool {
        self.reply_count.is_some_and(|count| count > 0)
    }

    /// Whether it has the `reply_count` and the `latest_reply` of `other`.
    pub(crate) fn same_replies(&self, other: &Self) -> bool {
        self.reply_count == other.reply_count && self.latest_reply == other.latest_reply
    }

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
