//! The typed endpoints of Slack, the answers of its methods, and the objects
//! of its answers.
//!
//! A method of Slack answers a failure in its JSON, with `ok` of `false` and
//! its `error`, and a rate limit with the status `429` and its `Retry-After`.

use omnifs_sdk::endpoint::HttpResponse;
use omnifs_sdk::error::{ProviderError, ProviderErrorKind};
use std::marker::PhantomData;

use omnifs_sdk::prelude::Result;
use serde::Deserialize;
use serde::de::{Deserializer, IgnoredAny, MapAccess, Visitor};
use serde_json::value::RawValue;

use crate::{Id, Ts};

/// The Web API of Slack.
#[derive(omnifs_sdk::Endpoint)]
#[endpoint(base = "https://slack.com")]
pub(crate) struct SlackApi;

/// The host of the bytes of the files of Slack.
#[derive(omnifs_sdk::Endpoint)]
#[endpoint(base = "https://files.slack.com")]
pub(crate) struct SlackFiles;

#[derive(Deserialize)]
struct Metadata {
    next_cursor: Option<String>,
}

/// A method of Slack, whose answer has its objects under `OBJECTS`.
pub(crate) trait Method {
    const OBJECTS: &'static str;
}

/// `users.list`, with the users under `members`.
pub(crate) struct UsersList;

impl Method for UsersList {
    const OBJECTS: &'static str = "members";
}

/// `users.conversations`, with the conversations under `channels`.
pub(crate) struct UsersConversations;

impl Method for UsersConversations {
    const OBJECTS: &'static str = "channels";
}

/// `conversations.history` and `conversations.replies`, with the messages
/// under `messages`.
pub(crate) struct ConversationsMessages;

impl Method for ConversationsMessages {
    const OBJECTS: &'static str = "messages";
}

/// The answer of the method `M`: `ok`, the `error` of `ok` of `false`, the
/// cursor of the next page, and the objects under `M::OBJECTS` as they came,
/// none in an answer with `ok` of `false`.
struct Answer<M> {
    ok: bool,
    error: Option<String>,
    response_metadata: Option<Metadata>,
    objects: Option<Vec<Box<RawValue>>>,
    method: PhantomData<M>,
}

impl<'de, M: Method> Deserialize<'de> for Answer<M> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct Fields<M>(PhantomData<M>);

        impl<'de, M: Method> Visitor<'de> for Fields<M> {
            type Value = Answer<M>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(formatter, "an answer of Slack with its {}", M::OBJECTS)
            }

            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Answer<M>, A::Error> {
                let (mut ok, mut error, mut response_metadata, mut objects) =
                    (None, None, None, None);
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "ok" => ok = Some(map.next_value()?),
                        "error" => error = map.next_value()?,
                        "response_metadata" => response_metadata = map.next_value()?,
                        key if key == M::OBJECTS => objects = map.next_value()?,
                        _ => {
                            map.next_value::<IgnoredAny>()?;
                        },
                    }
                }
                Ok(Answer {
                    ok: ok.ok_or_else(|| serde::de::Error::missing_field("ok"))?,
                    error,
                    response_metadata,
                    objects,
                    method: PhantomData,
                })
            }
        }

        deserializer.deserialize_map(Fields(PhantomData))
    }
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

/// The page of the method `M` of Slack in `response`, with its
/// `response_metadata.next_cursor`, none when empty or absent; else its
/// refusal: the error of the status and of its `Retry-After` for `429` and
/// for another failing status without the JSON of Slack, and the refusal of
/// the `error` of `ok` of `false`.
pub(crate) fn page<M: Method>(response: &HttpResponse) -> std::result::Result<Page, Refusal> {
    let status = response.status();
    let failed = || {
        ProviderError::from_http_answer(
            status,
            response
                .headers()
                .iter()
                .filter_map(|(name, value)| Some((name.as_str(), value.to_str().ok()?))),
        )
    };
    if status == 429 {
        return Err(failed().into());
    }
    let answer = match serde_json::from_slice::<Answer<M>>(response.body()) {
        Ok(answer) => answer,
        Err(_) if !(200..300).contains(&status) => return Err(failed().into()),
        Err(error) => return Err(undecoded(&error).into()),
    };
    if !answer.ok {
        let error = answer.error.ok_or_else(|| {
            ProviderError::invalid_input("an answer of Slack with ok of false has no error")
        })?;
        return Err(refusal(&error, failed()));
    }
    let objects = answer.objects.ok_or_else(|| {
        ProviderError::invalid_input("an answer of Slack with ok of true has no objects")
    })?;
    Ok(Page {
        objects,
        next: answer
            .response_metadata
            .and_then(|metadata| metadata.next_cursor)
            .filter(|cursor| !cursor.is_empty()),
    })
}

/// The refusal of a request that Slack answered with the error `error`: not
/// found for a channel or a thread that the bot sees no more, a timeout for a
/// cursor that Slack takes no more, which the first request again answers, a
/// refused credential for a token that Slack takes no more, a denial for a
/// token without the right of the request, a failure of the network for a
/// failure of Slack itself, which Slack asks to retry, and a timeout for a
/// request that Slack did not answer in time; each other one is `failed`,
/// the failure of the request by its status, with the `error` of Slack.
fn refusal(error: &str, failed: ProviderError) -> Refusal {
    let message = format!("Slack answered the error {error}");
    let (of, error) = match error {
        "channel_not_found" | "not_in_channel" => {
            (Refused::Channel, ProviderError::not_found(message))
        },
        "thread_not_found" => (Refused::Thread, ProviderError::not_found(message)),
        "invalid_cursor" => (Refused::Cursor, ProviderError::timeout(message)),
        "invalid_auth" | "not_authed" | "account_inactive" | "token_revoked" | "token_expired" => {
            (Refused::Request, ProviderError::permission_denied(message))
        },
        "missing_scope" | "no_permission" | "not_allowed_token_type" | "access_denied" => {
            (Refused::Request, ProviderError::denied(message))
        },
        "internal_error" | "fatal_error" | "service_unavailable" => {
            (Refused::Request, ProviderError::network(message))
        },
        "request_timeout" => (Refused::Request, ProviderError::timeout(message)),
        _ => (Refused::Request, with_message(&failed, message)),
    };
    Refusal { of, error }
}

/// `error` of its kind and its `Retry-After`, with `message`.
fn with_message(error: &ProviderError, message: String) -> ProviderError {
    let made = match error.kind() {
        ProviderErrorKind::NotFound => ProviderError::not_found(message),
        ProviderErrorKind::NotADirectory => ProviderError::not_a_directory(message),
        ProviderErrorKind::NotAFile => ProviderError::not_a_file(message),
        ProviderErrorKind::PermissionDenied => ProviderError::permission_denied(message),
        ProviderErrorKind::Network => ProviderError::network(message),
        ProviderErrorKind::Timeout => ProviderError::timeout(message),
        ProviderErrorKind::Denied => ProviderError::denied(message),
        ProviderErrorKind::InvalidInput => ProviderError::invalid_input(message),
        ProviderErrorKind::TooLarge => ProviderError::too_large(message),
        ProviderErrorKind::RateLimited => ProviderError::rate_limited(message),
        ProviderErrorKind::VersionMismatch => ProviderError::version_mismatch(message),
        ProviderErrorKind::Unimplemented => ProviderError::unimplemented(message),
        ProviderErrorKind::Internal => ProviderError::internal(message),
    };
    made.with_retry_after(error.retry_after())
}

fn undecoded(error: &serde_json::Error) -> ProviderError {
    ProviderError::invalid_input(format!("an answer of Slack: {error}"))
}

/// Decode the JSON of an object of an answer of Slack.
pub(crate) fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|error| undecoded(&error))
}

/// What the provider reads of a user: its `id` and the `real_name` of its
/// `profile`.
#[derive(Deserialize)]
pub(crate) struct UserObject {
    pub(crate) id: Id,
    pub(crate) profile: Profile,
}

#[derive(Deserialize)]
pub(crate) struct Profile {
    pub(crate) real_name: Option<String>,
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

/// The replies of a thread as its parent tells them: their `reply_count`
/// and the `ts` of the latest one.
#[derive(PartialEq, Eq)]
pub(crate) struct Thread<'a> {
    pub(crate) count: u64,
    pub(crate) latest: Option<&'a Ts>,
}

impl MessageObject {
    /// The replies of its thread, none for a message without a reply.
    pub(crate) fn thread(&self) -> Option<Thread<'_>> {
        self.reply_count
            .filter(|count| *count > 0)
            .map(|count| Thread {
                count,
                latest: self.latest_reply.as_ref(),
            })
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
