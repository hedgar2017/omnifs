//! The typed endpoints of Slack, the answers of its methods, and the objects
//! of its answers.
//!
//! A method of Slack answers a failure in its JSON, with `ok` of `false` and
//! its `error`, and a rate limit with the status `429` and its `Retry-After`.

use omnifs_sdk::endpoint::HttpResponse;
use omnifs_sdk::error::ProviderError;
use serde::Deserialize;
use serde::de::DeserializeOwned;
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

#[derive(Deserialize)]
pub(crate) struct Metadata {
    next_cursor: Option<String>,
}

/// The answer of a method of Slack: `ok`, the `error` of `ok` of `false`,
/// the cursor of the next page, and the objects of the page, none in an
/// answer with `ok` of `false`.
pub(crate) struct Answer {
    ok: bool,
    error: Option<String>,
    response_metadata: Option<Metadata>,
    objects: Option<Vec<Box<RawValue>>>,
}

/// A method of Slack, whose answer has its objects under the name that the
/// method gives them.
pub(crate) trait Method: DeserializeOwned {
    fn answer(self) -> Answer;
}

/// The answer of the method `$method` with its objects under `$objects`.
macro_rules! method {
    ($(#[$doc:meta])* $method:ident, $objects:ident) => {
        $(#[$doc])*
        #[derive(Deserialize)]
        pub(crate) struct $method {
            ok: bool,
            error: Option<String>,
            response_metadata: Option<Metadata>,
            $objects: Option<Vec<Box<RawValue>>>,
        }

        impl Method for $method {
            fn answer(self) -> Answer {
                Answer {
                    ok: self.ok,
                    error: self.error,
                    response_metadata: self.response_metadata,
                    objects: self.$objects,
                }
            }
        }
    };
}

method!(
    /// The answer of `users.list`, with the users under `members`.
    UsersList,
    members
);
method!(
    /// The answer of `users.conversations`, with the conversations under
    /// `channels`.
    UsersConversations,
    channels
);
method!(
    /// The answer of `conversations.history` and of `conversations.replies`,
    /// with the messages under `messages`.
    ConversationsMessages,
    messages
);

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
    let answer = match serde_json::from_slice::<M>(response.body()) {
        Ok(answer) => answer.answer(),
        Err(_) if !(200..300).contains(&status) => return Err(failed().into()),
        Err(error) => return Err(undecoded(&error).into()),
    };
    if !answer.ok {
        let error = answer.error.ok_or_else(|| {
            ProviderError::invalid_input("an answer of Slack with ok of false has no error")
        })?;
        return Err(refusal(&error));
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

/// The refusal of a request that Slack answered with the error `error`, with
/// the kind of that error: not found for a channel or a thread that the bot
/// sees no more, a timeout for a cursor that Slack takes no more, which the
/// first request again answers, a refused credential for a token that Slack
/// takes no more, a denial for a token without the right of the request, a
/// failure of the network for a failure of Slack itself, which Slack asks to
/// retry, a timeout for a request that Slack did not answer in time, an
/// invalid input for a request that Slack does not take, and an internal
/// error for each other one.
fn refusal(error: &str) -> Refusal {
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
        "invalid_arguments" | "invalid_arg_name" | "invalid_array_arg" | "invalid_charset"
        | "invalid_form_data" | "invalid_post_type" | "missing_post_type" => {
            (Refused::Request, ProviderError::invalid_input(message))
        },
        _ => (Refused::Request, ProviderError::internal(message)),
    };
    Refusal { of, error }
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
