//! The typed `DriveApi` endpoint, its rate-limit classifier, and the objects
//! of its answers.
//!
//! Google Drive answers a request over a quota with the status `403` and the
//! reason `rateLimitExceeded` or `userRateLimitExceeded` as well as with
//! `429`. [`DriveApi`] overrides [`EndpointHooks::classify`] for the `403`;
//! every other status falls through to the SDK's default handling.

use http::Response;
use omnifs_sdk::endpoint::{EndpointHooks, HttpResponse};
use omnifs_sdk::error::ProviderError;
use serde::Deserialize;
use serde_json::value::RawValue;

use crate::Result;

/// The API of Google Drive at its version 3.
#[derive(omnifs_sdk::Endpoint)]
#[endpoint(base = "https://www.googleapis.com", hooks)]
pub(crate) struct DriveApi;

impl EndpointHooks for DriveApi {
    /// The rate-limited error of a `403` over a quota; `None` for every
    /// other answer.
    fn classify(&self, response: &Response<Vec<u8>>) -> Option<ProviderError> {
        (response.status().as_u16() == 403 && over_quota(response.body()))
            .then(|| ProviderError::rate_limited("HTTP 403"))
    }
}

/// The error object of an answer of Google Drive.
#[derive(Deserialize)]
struct Failure {
    error: FailureBody,
}

#[derive(Deserialize)]
struct FailureBody {
    errors: Vec<FailureItem>,
}

#[derive(Deserialize)]
struct FailureItem {
    reason: String,
}

/// Whether the error object in `body` has a reason of a request over a
/// quota.
fn over_quota(body: &[u8]) -> bool {
    serde_json::from_slice::<Failure>(body).is_ok_and(|failure| {
        failure.error.errors.iter().any(|item| {
            matches!(
                item.reason.as_str(),
                "rateLimitExceeded" | "userRateLimitExceeded"
            )
        })
    })
}

/// The fields of a file or a folder that the provider asks Google Drive for,
/// and that `file.json` and `folder.json` have.
pub(crate) const FIELDS: &str = "id,name,mimeType,description,parents,driveId,trashed,createdTime,modifiedTime,size,md5Checksum,webViewLink,owners(displayName,emailAddress),lastModifyingUser(displayName,emailAddress)";

/// The largest uploaded file of the mount in bytes, as the largest file that
/// the host serves from its projection.
pub(crate) const MAX_BYTES: u64 = 64 * 1024 * 1024;

/// The error of an answer of Google Drive with a failing status.
pub(crate) fn status_error(response: &HttpResponse) -> ProviderError {
    ProviderError::from_http_status(response.status())
}

/// Decode the JSON of an answer of Google Drive.
pub(crate) fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|error| {
        ProviderError::invalid_input(format!("an answer of Google Drive: {error}"))
    })
}

/// A page of the files of Google Drive: its objects as they came, the token
/// of its next page while it has one, and whether Google Drive searched not
/// every drive for it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FileList {
    pub(crate) files: Vec<Box<RawValue>>,
    pub(crate) next_page_token: Option<String>,
    pub(crate) incomplete_search: bool,
}

/// A page of the changes of Google Drive: the token of its next page while
/// it has one, and after its last page the token of the changes after it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ChangeList {
    pub(crate) changes: Vec<Change>,
    pub(crate) next_page_token: Option<String>,
    pub(crate) new_start_page_token: Option<String>,
}

/// A change of Google Drive: what it is of, and for a file or a folder its
/// ID, whether the account no longer has it, and its object as it came
/// while the account has it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Change {
    #[serde(rename = "changeType")]
    pub(crate) of: ChangeType,
    pub(crate) file_id: Option<String>,
    pub(crate) removed: bool,
    pub(crate) file: Option<Box<RawValue>>,
}

/// What a change is of: a file or a folder, or a shared drive, of which the
/// mount has nothing.
#[derive(Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ChangeType {
    File,
    Drive,
}

/// The token of the changes after now.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StartPageToken {
    pub(crate) start_page_token: String,
}

/// What the provider reads of a file or a folder.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Item {
    pub(crate) id: String,
    pub(crate) name: String,
    mime_type: String,
    pub(crate) trashed: bool,
    modified_time: String,
    md5_checksum: Option<String>,
    size: Option<String>,
}

/// What an item is for the mount.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Folder,
    File(Content),
    /// An item that the mount has not: another type of Google, or an
    /// uploaded file of more than [`MAX_BYTES`].
    Other,
}

/// What the file `content` of a file has.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Content {
    /// The bytes of an uploaded file.
    Bytes,
    /// The export of a file of Google with this MIME type.
    Export(&'static str),
}

const GOOGLE: &str = "application/vnd.google-apps.";

impl Item {
    /// What the item is for the mount, by its MIME type and its size.
    pub(crate) fn kind(&self) -> Result<Kind> {
        Ok(match self.mime_type.strip_prefix(GOOGLE) {
            Some("folder") => Kind::Folder,
            Some("document") => Kind::File(Content::Export("text/markdown")),
            Some("spreadsheet") => Kind::File(Content::Export("text/csv")),
            Some("presentation") => Kind::File(Content::Export("text/plain")),
            Some(_) => Kind::Other,
            None if self.bytes()? <= MAX_BYTES => Kind::File(Content::Bytes),
            None => Kind::Other,
        })
    }

    /// The size of an uploaded file in bytes.
    fn bytes(&self) -> Result<u64> {
        self.size
            .as_deref()
            .ok_or_else(|| {
                ProviderError::invalid_input("an uploaded file of Google Drive has no size")
            })?
            .parse()
            .map_err(|error| {
                ProviderError::invalid_input(format!("the size of a file of Google Drive: {error}"))
            })
    }

    /// What changes with the content of a file: the checksum of an uploaded
    /// file, and the `modifiedTime` of a file of Google.
    pub(crate) fn revision(&self, content: Content) -> Result<&str> {
        match content {
            Content::Bytes => self.md5_checksum.as_deref().ok_or_else(|| {
                ProviderError::invalid_input("an uploaded file of Google Drive has no md5Checksum")
            }),
            Content::Export(_) => Ok(&self.modified_time),
        }
    }
}
