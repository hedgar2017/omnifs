//! The typed `NotionApi` endpoint, its rate-limit classifier, and the objects
//! of its answers.
//!
//! Notion asks to take an answer with the status `529` as one with `429`,
//! after the seconds of its `Retry-After`, and an answer `429` whose
//! `rate_limit_reason` is `public_api_request_blocked` as a connection whose
//! access Notion blocked, which no wait unblocks. [`NotionApi`] overrides
//! [`EndpointHooks::classify`] for both; every other status falls through to
//! the SDK's default handling.

use std::time::Duration;

use http::Response;
use omnifs_sdk::endpoint::{EndpointHooks, HttpResponse};
use omnifs_sdk::error::ProviderError;
use serde::Deserialize;
use serde_json::value::RawValue;
use time::OffsetDateTime;

use crate::Result;

/// The API of Notion at the version whose objects the provider reads.
#[derive(omnifs_sdk::Endpoint)]
#[endpoint(
    base = "https://api.notion.com",
    default_header = "Notion-Version: 2026-03-11",
    hooks
)]
pub(crate) struct NotionApi;

impl EndpointHooks for NotionApi {
    /// The rate-limited error of a `429` for an answer `529`, and a `denied`
    /// error with its `message` for an answer `429` of a blocked connection,
    /// which arms no window; `None` for every other answer.
    fn classify(&self, response: &Response<Vec<u8>>) -> Option<ProviderError> {
        match response.status().as_u16() {
            529 => Some(rate_limited(
                529,
                response
                    .headers()
                    .get("retry-after")
                    .and_then(|value| value.to_str().ok()),
            )),
            429 => serde_json::from_slice::<Failure>(response.body())
                .ok()
                .filter(|failure| {
                    failure
                        .additional_data
                        .as_ref()
                        .and_then(|data| data.rate_limit_reason.as_deref())
                        == Some("public_api_request_blocked")
                })
                .map(|failure| ProviderError::denied(failure.message)),
            _ => None,
        }
    }
}

/// The error object of an answer of Notion.
#[derive(Deserialize)]
struct Failure {
    message: String,
    additional_data: Option<AdditionalData>,
}

#[derive(Deserialize)]
struct AdditionalData {
    rate_limit_reason: Option<String>,
}

/// The rate-limited error of an answer with `status` and the seconds of its
/// `Retry-After`.
fn rate_limited(status: u16, retry_after: Option<&str>) -> ProviderError {
    let message = match retry_after {
        Some(value) => format!("HTTP {status}; retry_after={value}"),
        None => format!("HTTP {status}"),
    };
    ProviderError::rate_limited(message).with_retry_after(
        retry_after
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(Duration::from_secs),
    )
}

/// The error of an answer of Notion with a failing status: the rate-limited
/// error of a `429` with the seconds of its `Retry-After`, else the error of
/// its status.
pub(crate) fn status_error(response: &HttpResponse) -> ProviderError {
    match response.status() {
        429 => rate_limited(429, response.header("retry-after")),
        status => ProviderError::from_http_status(status),
    }
}

/// Decode the JSON of an answer of Notion.
pub(crate) fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T> {
    serde_json::from_slice(bytes)
        .map_err(|error| ProviderError::invalid_input(format!("an answer of Notion: {error}")))
}

/// A list of Notion: its objects as they came, and the cursor of its next
/// page while it has more.
#[derive(Deserialize)]
pub(crate) struct List {
    pub(crate) results: Vec<Box<RawValue>>,
    pub(crate) next_cursor: Option<String>,
    pub(crate) has_more: bool,
}

impl List {
    /// The cursor of the next page, `None` after the last page.
    pub(crate) fn next(&self) -> Result<Option<String>> {
        if !self.has_more {
            return Ok(None);
        }
        self.next_cursor.clone().map(Some).ok_or_else(|| {
            ProviderError::invalid_input("a list of Notion with more results has no next_cursor")
        })
    }
}

/// What a result of the search, a page, or a data source has in common.
#[derive(Deserialize)]
pub(crate) struct Found {
    pub(crate) object: String,
    pub(crate) id: String,
    #[serde(with = "time::serde::rfc3339")]
    pub(crate) last_edited_time: OffsetDateTime,
    pub(crate) in_trash: bool,
}

/// The properties of a page.
#[derive(Deserialize)]
pub(crate) struct PageObject {
    properties: std::collections::BTreeMap<String, Property>,
}

/// A property of a page: its title for the type `title`, of which the
/// provider reads nothing else.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Property {
    Title {
        title: Vec<RichText>,
    },
    #[serde(other)]
    Other,
}

impl PageObject {
    /// The `plain_text` of the items of its property of the type `title`,
    /// one after another.
    pub(crate) fn title(&self) -> Result<String> {
        self.properties
            .values()
            .find_map(|property| match property {
                Property::Title { title } => Some(plain_text(title)),
                Property::Other => None,
            })
            .ok_or_else(|| ProviderError::invalid_input("a page of Notion has no title property"))
    }
}

/// The title of a data source.
#[derive(Deserialize)]
pub(crate) struct DataSourceObject {
    title: Vec<RichText>,
}

impl DataSourceObject {
    /// The `plain_text` of the items of its `title`, one after another.
    pub(crate) fn title(&self) -> String {
        plain_text(&self.title)
    }
}

/// A comment: its ID, its rich text, and the name it shows.
#[derive(Deserialize)]
pub(crate) struct CommentObject {
    pub(crate) id: String,
    rich_text: Vec<RichText>,
    display_name: DisplayName,
}

#[derive(Deserialize)]
struct DisplayName {
    resolved_name: Option<String>,
}

impl CommentObject {
    /// The `resolved_name` of its `display_name`, empty for `null`.
    pub(crate) fn author(&self) -> &str {
        self.display_name
            .resolved_name
            .as_deref()
            .unwrap_or_default()
    }

    /// The `plain_text` of the items of its `rich_text`, one after another.
    pub(crate) fn body(&self) -> String {
        plain_text(&self.rich_text)
    }
}

#[derive(Deserialize)]
struct RichText {
    plain_text: String,
}

fn plain_text(items: &[RichText]) -> String {
    items.iter().map(|item| item.plain_text.as_str()).collect()
}

/// The markdown of a page or a block, with the IDs of the blocks that it
/// renders as unknown.
#[derive(Deserialize)]
pub(crate) struct Markdown {
    pub(crate) markdown: String,
    pub(crate) unknown_block_ids: Vec<String>,
}
