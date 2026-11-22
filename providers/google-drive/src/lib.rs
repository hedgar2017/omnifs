#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

//! google-drive-provider: the items of a Google Drive for omnifs.
//!
//! The mount has `files/{id}/` for each file with `name`, `file.json`, and
//! `content`, `folders/{id}/` for each folder with `name` and `folder.json`,
//! and `others/{id}/` for each other item with `name` and `item.json`, by
//! the ID of Google Drive, as in a link of Google Drive. A file is an
//! uploaded file, an item of another MIME type than one of Google with a
//! size, whose `content` is its bytes, of any size, or a file of Google with
//! an export: a document of Google Docs, whose `content` is its Markdown, a
//! spreadsheet of Google Sheets, whose `content` is the CSV of its first
//! sheet, a presentation of Google Slides, whose `content` is its plain
//! text, a drawing of Google Drawings, whose `content` is its SVG, or a
//! project of Google Apps Script, whose `content` is its JSON. Another item
//! is one without bytes of its own: one of Google, as a form or a shortcut,
//! or one without a size.
//!
//! Their facts come from feeds alone. The feed of the Drive lists the items
//! of the account in `files/`, `folders/`, and `others/` and takes their
//! changes from the changes of Google Drive. The own feed of a file, a
//! folder, or another item takes one by its ID, and again at each operation
//! on it past its deadline while the feed of the Drive did not bring it,
//! since Google Drive reports no change of an item that the account has by
//! its link alone; the own feed of a file takes its content too.

use omnifs_sdk::prelude::*;
use serde::Deserialize;

pub(crate) use omnifs_sdk::prelude::Result;

mod api;
mod feed;

/// The ID of an item of Google Drive: letters, digits, `-`, and `_`.
#[omnifs_sdk::path_segment(validate = is_id)]
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct Id(String);

impl TryFrom<String> for Id {
    type Error = String;

    fn try_from(id: String) -> std::result::Result<Self, Self::Error> {
        id.parse()
            .map_err(|()| format!("{id} is not an ID of Google Drive"))
    }
}

fn is_id(segment: &str) -> bool {
    !segment.is_empty()
        && segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

#[omnifs_sdk::path_captures]
pub(crate) struct DriveKey {}

#[omnifs_sdk::path_captures]
pub(crate) struct ItemKey {
    pub(crate) id: Id,
}

/// The Drive of the account, with the items that it can read.
#[omnifs_sdk::object(kind = "google-drive.drive", key = DriveKey, no_load)]
#[derive(Deserialize)]
pub(crate) struct Drive {}

/// A file of the Drive.
#[omnifs_sdk::object(kind = "google-drive.file", key = ItemKey, no_load)]
#[derive(Deserialize)]
pub(crate) struct File {}

/// A folder of the Drive.
#[omnifs_sdk::object(kind = "google-drive.folder", key = ItemKey, no_load)]
#[derive(Deserialize)]
pub(crate) struct Folder {}

/// An item of the Drive that is no file and no folder.
#[omnifs_sdk::object(kind = "google-drive.other", key = ItemKey, no_load)]
#[derive(Deserialize)]
pub(crate) struct Other {}

#[omnifs_sdk::provider(
    id = "google-drive",
    display_name = "Google Drive",
    description = "files, folders, and other items from your Drive",
    mount = "google-drive",
    projection = 4,
    capabilities(domain(
        "www.googleapis.com",
        "Fetch the items, the content of the files, and the changes of a Google Drive."
    ),),
    limits(
        memory_mb(
            256,
            "Leave room for a page of 1,000 items, and the names of the items of a listing of the projection."
        ),
        fetch_blob_bytes(
            5_497_558_138_880,
            "The largest file of Google Drive, of 5 TB, and an export, of at most 10 MB, whose bytes the host holds."
        ),
    ),
    auth = r#"{"default":"service-account","schemes":[{"oauth":{"key":"service-account","displayName":"Google service account","tokenEndpoint":"https://oauth2.googleapis.com/token","defaultScopes":["https://www.googleapis.com/auth/drive.readonly"],"flow":{"jwtBearerScopeClaim":{"claim":"scope"}},"injectDomains":["www.googleapis.com"],"injectValuePrefix":"Bearer "}}],"guidance":{"service-account":{"summary":"The key of a Google service account; no browser and no sign-in.","setupSteps":["Create a service account in a Google Cloud project with the Google Drive API enabled, and a JSON key of it.","Share the folders of the Drive with the email of the service account, or give it domain-wide delegation for the scope of the mount and pass the user whose Drive it reads as --subject.","Pass `client_email` of the key as --client-id and a file with `private_key` of the key as --client-secret-file."]}}}"#
)]
impl GoogleDriveProvider {
    fn start(r: &mut Router) -> Result<()> {
        r.object::<Drive>("/", |o| {
            o.dynamic();
            o.dir("files").fed()?;
            o.dir("folders").fed()?;
            o.dir("others").fed()?;
            o.feed(Drive::feed).starts(["files", "folders", "others"])?;
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

        r.object::<Other>("/others/{id}", |o| {
            o.dynamic();
            o.fed();
            o.file("name").fed()?;
            o.file("item.json").fed()?;
            o.feed(Other::feed)
                .own()
                .starts(["", "name", "item.json"])?;
            Ok(())
        })?;

        Ok(())
    }
}
