//! The typed `DriveApi` endpoint, its rate-limit classifier, and the objects
//! of its answers.
//!
//! Google Drive answers a request over a quota with the status `403` and the
//! reason `rateLimitExceeded` or `userRateLimitExceeded` as well as with
//! `429`. [`DriveApi`] overrides [`EndpointHooks::classify`] for the `403`.

use http::Response;
use omnifs_sdk::endpoint::EndpointHooks;
use omnifs_sdk::error::ProviderError;
use serde::Deserialize;
use serde_json::value::RawValue;

use crate::{Id, Result};

/// The API of Google Drive at its version 3.
#[derive(omnifs_sdk::Endpoint)]
#[endpoint(base = "https://www.googleapis.com", hooks)]
pub(crate) struct DriveApi;

impl EndpointHooks for DriveApi {
    fn classify(&self, response: &Response<Vec<u8>>) -> Option<ProviderError> {
        (response.status().as_u16() == 403 && over_quota(response.body()))
            .then(|| ProviderError::rate_limited("HTTP 403"))
    }
}

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

/// The fields of an item that the provider asks Google Drive for, and that
/// `file.json`, `folder.json`, and `item.json` have.
pub(crate) const FIELDS: &str = "id,name,mimeType,description,parents,driveId,trashed,createdTime,modifiedTime,size,md5Checksum,headRevisionId,webViewLink,owners(displayName,emailAddress),lastModifyingUser(displayName,emailAddress)";

pub(crate) fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|error| {
        ProviderError::invalid_input(format!("an answer of Google Drive: {error}"))
    })
}

/// A page of the items of a corpus of Google Drive: its objects as they
/// came, the token of its next page while it has one, and whether Google
/// Drive searched not all of the corpus for it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FileList {
    pub(crate) files: Vec<Box<RawValue>>,
    pub(crate) next_page_token: Option<String>,
    pub(crate) incomplete_search: bool,
}

/// A page of the shared drives of the account, and the token of its next
/// page while it has one.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DriveList {
    pub(crate) drives: Vec<SharedDrive>,
    pub(crate) next_page_token: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct SharedDrive {
    pub(crate) id: String,
}

/// A page of the changes of Google Drive: the token of its next page while
/// it has one, and after its last page the token of the changes after it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ChangeList {
    pub(crate) changes: Vec<DriveChange>,
    pub(crate) next_page_token: Option<String>,
    pub(crate) new_start_page_token: Option<String>,
}

/// A change of Google Drive, of an item or of a shared drive: the ID of
/// what it is of, whether the account no longer has it, and the object of
/// an item as it came while the account has it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DriveChange {
    pub(crate) change_type: ChangeType,
    pub(crate) file_id: Option<Id>,
    pub(crate) drive_id: Option<String>,
    pub(crate) removed: bool,
    pub(crate) file: Option<Box<RawValue>>,
}

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

/// What the provider reads of the object of an item: its ID, its MIME type,
/// and whether it is in the trash, which place it in the mount, and what
/// the files of its folder are of, which Google Drive states for some items
/// alone.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Item {
    pub(crate) id: Id,
    mime_type: String,
    pub(crate) trashed: bool,
    pub(crate) name: Option<String>,
    pub(crate) drive_id: Option<String>,
    size: Option<String>,
    modified_time: Option<String>,
    md5_checksum: Option<String>,
    head_revision_id: Option<String>,
}

/// What an item is for the mount.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    File,
    Folder,
    /// An item without bytes of its own.
    Other,
}

impl Kind {
    pub(crate) const ALL: [Self; 3] = [Self::File, Self::Folder, Self::Other];
}

/// What the file `content` of a file has.
pub(crate) enum Content<'a> {
    /// The bytes of an uploaded file, with their MD5 and the ID of their
    /// revision where Google Drive states them.
    Bytes {
        md5: Option<&'a str>,
        head: Option<&'a str>,
    },
    /// The export with `mime_type` of a file of Google, modified at
    /// `modified` where Google Drive states it.
    Export {
        mime_type: &'static str,
        modified: Option<&'a str>,
    },
}

impl Content<'_> {
    /// What changes with the content: the MD5 of the bytes, else the ID of
    /// their revision, and the time of the modification of an export; none
    /// where Google Drive states neither.
    pub(crate) fn revision(&self) -> Option<&str> {
        match self {
            Self::Bytes { md5, head } => md5.or(*head),
            Self::Export { modified, .. } => *modified,
        }
    }
}

const GOOGLE: &str = "application/vnd.google-apps.";

impl Item {
    /// The content of a file: the bytes of an item of another MIME type than
    /// one of Google that has a size, and the export of a file of Google
    /// with one; none for each other item.
    pub(crate) fn content(&self) -> Option<Content<'_>> {
        let Some(google) = self.mime_type.strip_prefix(GOOGLE) else {
            return self.size.is_some().then_some(Content::Bytes {
                md5: self.md5_checksum.as_deref(),
                head: self.head_revision_id.as_deref(),
            });
        };
        let mime_type = match google {
            "document" => "text/markdown",
            "spreadsheet" => "text/csv",
            "presentation" => "text/plain",
            "drawing" => "image/svg+xml",
            "script" => "application/vnd.google-apps.script+json",
            _ => return None,
        };
        Some(Content::Export {
            mime_type,
            modified: self.modified_time.as_deref(),
        })
    }

    pub(crate) fn kind(&self) -> Kind {
        if self.content().is_some() {
            Kind::File
        } else if self.mime_type.strip_prefix(GOOGLE) == Some("folder") {
            Kind::Folder
        } else {
            Kind::Other
        }
    }
}
