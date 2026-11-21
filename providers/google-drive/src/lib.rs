#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

//! google-drive-provider: the files and the folders of a Google Drive for
//! omnifs.
//!
//! The mount has `files/{id}/` for each file with `name`, `file.json`, and
//! `content`, and `folders/{id}/` for each folder with `name` and
//! `folder.json`, by the ID of Google Drive, as in a link of Google Drive.
//! A file is an uploaded file of at most 64 MiB, whose `content` is its
//! bytes, a document of Google Docs, whose `content` is its Markdown, a
//! spreadsheet of Google Sheets, whose `content` is the CSV of its first
//! sheet, or a presentation of Google Slides, whose `content` is its plain
//! text; the mount has no other item of the Drive. Their facts come from
//! feeds alone: the feed of the Drive lists the files and the folders and
//! takes their changes from the changes of Google Drive, and the own feeds of
//! a file and of a folder take one by its ID, the content of a file too.

use std::future::Future;

use omnifs_sdk::prelude::*;
use serde::{Deserialize, Serialize};

pub(crate) use omnifs_sdk::prelude::Result;

mod api;
mod feed;

/// The ID of a file or a folder of Google Drive: letters, digits, `-`, and
/// `_`.
#[omnifs_sdk::path_segment(validate = is_id)]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Id(String);

impl TryFrom<String> for Id {
    type Error = String;

    fn try_from(id: String) -> std::result::Result<Self, Self::Error> {
        id.parse()
            .map_err(|()| format!("{id} is not an ID of Google Drive"))
    }
}

impl From<Id> for String {
    fn from(id: Id) -> Self {
        id.0
    }
}

fn is_id(segment: &str) -> bool {
    !segment.is_empty()
        && segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

impl Id {
    /// The ID of an object of an answer of Google Drive.
    pub(crate) fn of(id: &str) -> Result<Self> {
        id.parse()
            .map_err(|()| ProviderError::invalid_input(format!("the ID {id} of Google Drive")))
    }
}

#[omnifs_sdk::path_captures]
pub(crate) struct DriveKey {}

#[omnifs_sdk::path_captures]
pub(crate) struct FileKey {
    pub(crate) id: Id,
}

#[omnifs_sdk::path_captures]
pub(crate) struct FolderKey {
    pub(crate) id: Id,
}

/// The Drive of the account, with the files and the folders that it can
/// read.
#[omnifs_sdk::object(kind = "google-drive.drive", key = DriveKey, load = fed_only)]
#[derive(Deserialize)]
pub(crate) struct Drive {}

/// A file of the Drive.
#[omnifs_sdk::object(kind = "google-drive.file", key = FileKey, load = fed_only)]
#[derive(Deserialize)]
pub(crate) struct File {}

/// A folder of the Drive.
#[omnifs_sdk::object(kind = "google-drive.folder", key = FolderKey, load = fed_only)]
#[derive(Deserialize)]
pub(crate) struct Folder {}

/// The load of an object of the mount, which no face asks for: the objects
/// have no canonical bytes, and the facts of their faces come from feeds
/// alone.
fn fed_only<O: Object>(
    _cx: &Cx<O::State>,
    _key: &O::Key,
    _since: Option<Validator>,
) -> impl Future<Output = Result<Load<O>>> {
    std::future::ready(Err(ProviderError::internal(format!(
        "the object {} has no canonical bytes",
        O::kind().as_str()
    ))))
}

#[omnifs_sdk::provider(
    id = "google-drive",
    display_name = "Google Drive",
    description = "files and folders from your Drive",
    mount = "google-drive",
    projection = 1,
    capabilities(domain(
        "www.googleapis.com",
        "Fetch the files, the folders, the content of the files, and the changes of a Google Drive."
    ),),
    limits(memory_mb(
        512,
        "Leave room for the bytes of a file of 64 MiB and its copies on the way to the host."
    ),),
    auth = r#"{"default":"service-account","schemes":[{"oauth":{"key":"service-account","displayName":"Google service account","authorizationEndpoint":"https://accounts.google.com/o/oauth2/v2/auth","tokenEndpoint":"https://oauth2.googleapis.com/token","defaultScopes":["https://www.googleapis.com/auth/drive.readonly"],"flow":"jwtBearer","refreshTokenRotates":false,"injectDomains":["www.googleapis.com"],"injectValuePrefix":"Bearer "}}],"guidance":{"service-account":{"summary":"The key of a Google service account; no browser and no sign-in.","setupSteps":["Create a service account in a Google Cloud project with the Google Drive API enabled, and a JSON key of it.","Share the folders of the Drive with the email of the service account, or give it domain-wide delegation.","Pass `client_email` of the key as --client-id and a file with `private_key` of the key as --client-secret-file."]}}}"#
)]
impl GoogleDriveProvider {
    fn start(r: &mut Router) -> Result<()> {
        r.object::<Drive>("/", |o| {
            o.dynamic();
            o.dir("files").fed()?;
            o.dir("folders").fed()?;
            o.feed(Drive::feed).starts(["files", "folders"])?;
            Ok(())
        })?;

        r.object::<File>("/files/{id}", |o| {
            o.dynamic();
            o.fed();
            o.file("name").fed()?;
            o.file("file.json").fed()?;
            o.file("content").fed()?;
            o.feed(File::feed)
                .own()
                .starts(["", "name", "file.json", "content"])?;
            Ok(())
        })?;

        r.object::<Folder>("/folders/{id}", |o| {
            o.dynamic();
            o.fed();
            o.file("name").fed()?;
            o.file("folder.json").fed()?;
            o.feed(Folder::feed)
                .own()
                .starts(["", "name", "folder.json"])?;
            Ok(())
        })?;

        Ok(())
    }
}
