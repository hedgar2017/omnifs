//! The feeds of the files, the folders, and the other items of the Drive.
//!
//! The feed of the Drive takes a part for each request: at its first answer
//! the token of the changes after now and then a full listing of the files
//! and the folders, and at each later answer the changes after the token of
//! the answer before it. A request of the changes that Google Drive refuses
//! with `400` for its token begins a full listing again, after which the
//! files and the folders of the projection that it lacks leave it, and a
//! request of a full listing that it refuses with `400` for the token of
//! its page begins the listing again. The own feed of a file
//! takes it by its ID, and its content, and the own feed of a folder takes
//! it by its ID.

use std::collections::BTreeSet;

use omnifs_sdk::error::ProviderErrorKind;
use omnifs_sdk::prelude::*;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::api::{
    Change, ChangeList, ChangeType, Content, DriveApi, FIELDS, FileList, Item, Kind,
    StartPageToken, decode, status_error,
};
use crate::{Drive, DriveKey, File, FileKey, Folder, FolderKey, Id, Other, OtherKey};

/// The files of a request of a full listing, and the changes of a request of
/// the changes: the most that Google Drive gives.
const PAGE: usize = 1000;

// ===========================================================================
// Tokens
// ===========================================================================

/// The token of the feed of the Drive: the token of the changes after its
/// last answer, none before the end of a full listing, and the stage of an
/// unfinished answer. A token with neither begins a full listing again.
#[derive(Serialize, Deserialize)]
struct DriveToken {
    changes: Option<String>,
    stage: Option<Stage>,
}

/// The stage of an unfinished answer of the Drive.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Stage {
    /// A full listing at the token of its next page, none for its first
    /// request, with the token of the changes after its start, and for a
    /// full listing other than the first the IDs of its files and folders so
    /// far; `first` for the first full listing.
    Listing {
        first: bool,
        changes: String,
        page: Option<String>,
        files: BTreeSet<Id>,
        folders: BTreeSet<Id>,
        others: BTreeSet<Id>,
    },
    /// The changes at the token of their next page.
    Changes { page: String },
}

/// The token of the feed of a file: what changes with its content. The token
/// of a removed file is `null`.
#[derive(Serialize, Deserialize)]
struct FileToken {
    revision: String,
}

/// The token of the feed of a folder. The token of a removed folder is
/// `null`.
#[derive(Serialize, Deserialize)]
struct FolderToken {}

/// The token of the feed of another item. The token of a removed item is
/// `null`.
#[derive(Serialize, Deserialize)]
struct OtherToken {}

fn encode<T: Serialize>(token: &T) -> Result<String> {
    serde_json::to_string(token)
        .map_err(|error| ProviderError::internal(format!("feed token: {error}")))
}

fn decode_token<T: DeserializeOwned>(token: &str) -> Result<T> {
    serde_json::from_str(token)
        .map_err(|error| ProviderError::invalid_input(format!("feed token: {error}")))
}

// ===========================================================================
// Facts
// ===========================================================================

const FILES: &str = "/files";
const FOLDERS: &str = "/folders";
const OTHERS: &str = "/others";

fn file_path(id: &Id) -> String {
    format!("{FILES}/{id}")
}

fn folder_path(id: &Id) -> String {
    format!("{FOLDERS}/{id}")
}

fn other_path(id: &Id) -> String {
    format!("{OTHERS}/{id}")
}

/// Write the facts of a file from its object: its folder in `files/`, which
/// joins it as `write` says, `name`, and `file.json`.
fn write_file<K>(
    cx: &FeedCx<K>,
    id: &Id,
    item: &Item,
    object: &RawValue,
    write: ListingWrite,
) -> Result<()> {
    let at = file_path(id);
    cx.dir(FILES, write)?;
    cx.dir(&at, ListingWrite::Partial)?;
    cx.file(format!("{at}/name"), item.name.as_bytes())?;
    cx.file(format!("{at}/file.json"), object.get().as_bytes())?;
    Ok(())
}

/// Write the facts of a folder from its object: its folder in `folders/`,
/// which joins it as `write` says, `name`, and `folder.json`.
fn write_folder<K>(
    cx: &FeedCx<K>,
    id: &Id,
    item: &Item,
    object: &RawValue,
    write: ListingWrite,
) -> Result<()> {
    let at = folder_path(id);
    cx.dir(FOLDERS, write)?;
    cx.dir(&at, ListingWrite::Complete)?;
    cx.file(format!("{at}/name"), item.name.as_bytes())?;
    cx.file(format!("{at}/folder.json"), object.get().as_bytes())?;
    Ok(())
}

/// Write the facts of another item from its object: its folder in `others/`,
/// which joins it as `write` says, `name`, and `item.json`.
fn write_other<K>(
    cx: &FeedCx<K>,
    id: &Id,
    item: &Item,
    object: &RawValue,
    write: ListingWrite,
) -> Result<()> {
    let at = other_path(id);
    cx.dir(OTHERS, write)?;
    cx.dir(&at, ListingWrite::Complete)?;
    cx.file(format!("{at}/name"), item.name.as_bytes())?;
    cx.file(format!("{at}/item.json"), object.get().as_bytes())?;
    Ok(())
}

/// The file of the projection with `id`, by its `file.json`: what changes
/// with its content, none for a file that the projection has not.
fn projected_revision<K>(cx: &FeedCx<K>, id: &Id) -> Result<Option<String>> {
    let Some(object) = cx.read(format!("{}/file.json", file_path(id)))? else {
        return Ok(None);
    };
    let item: Item = decode(&object)?;
    match item.kind() {
        Kind::File(content) => Ok(Some(item.revision(content)?.to_owned())),
        Kind::Folder | Kind::Other => Err(ProviderError::internal(format!(
            "the file.json of {id} is of no file"
        ))),
    }
}

/// The IDs of the entries of the listing of `path` in the projection, none
/// without a listing.
fn listed<K>(cx: &FeedCx<K>, path: &str) -> Result<BTreeSet<Id>> {
    let Some(facts) = cx.listing(path, ListingForm::Names)? else {
        return Ok(BTreeSet::new());
    };
    let ListingEntries::Names(names) = facts.entries else {
        return Err(ProviderError::internal(format!(
            "the listing of {path} has no names"
        )));
    };
    names
        .iter()
        .map(|name| {
            name.parse()
                .map_err(|()| ProviderError::internal(format!("{path} lists {name}")))
        })
        .collect()
}

// ===========================================================================
// The feed of the Drive
// ===========================================================================

/// The token of the feed of a file or a folder, as an inner state of the
/// feed of the Drive.
enum Inner {
    File(Id, String),
    Folder(Id, String),
    Other(Id, String),
}

/// What an item of an answer is in the projection after it, none for an
/// item in the trash.
enum Taken {
    File(Id),
    Folder(Id),
    Other(Id),
    Trashed,
}

/// One call of the feed of the Drive: its context and the inner states of
/// its part.
struct DriveFeed<'a> {
    cx: &'a FeedCx<DriveKey>,
    inner: Vec<Inner>,
}

impl Drive {
    /// The feed of the files and the folders of the Drive.
    pub(crate) async fn feed(cx: FeedCx<DriveKey>) -> Result<Feed> {
        let token = cx.token().map(decode_token::<DriveToken>).transpose()?;
        let mut feed = DriveFeed {
            cx: &cx,
            inner: Vec::new(),
        };
        match token {
            None => feed.start(true).await,
            Some(DriveToken {
                stage:
                    Some(Stage::Listing {
                        first,
                        changes,
                        page,
                        files,
                        folders,
                        others,
                    }),
                ..
            }) => {
                feed.listing(first, changes, page, [files, folders, others])
                    .await
            },
            Some(DriveToken {
                changes: Some(changes),
                stage: Some(Stage::Changes { page }),
            }) => feed.changes(changes, page).await,
            Some(DriveToken {
                changes: Some(changes),
                stage: None,
            }) => feed.changes(changes.clone(), changes).await,
            Some(DriveToken {
                changes: None,
                stage: None,
            }) => feed.start(false).await,
            Some(DriveToken {
                changes: None,
                stage: Some(Stage::Changes { .. }),
            }) => Err(ProviderError::invalid_input(
                "the token of the feed of the Drive has a page of the changes of no token",
            )),
        }
    }
}

impl DriveFeed<'_> {
    /// The token of the changes after now, before the first request of a
    /// full listing, whose answers the changes after it complete.
    async fn start(&mut self, first: bool) -> Result<Feed> {
        let response = self
            .cx
            .endpoint(DriveApi)
            .get("/drive/v3/changes/startPageToken")
            .query("supportsAllDrives", "true")
            .send()
            .await?;
        if response.status() != 200 {
            return Err(status_error(&response));
        }
        let start: StartPageToken = decode(response.body())?;
        self.part(
            None,
            Some(Stage::Listing {
                first,
                changes: start.start_page_token,
                page: None,
                files: BTreeSet::new(),
                folders: BTreeSet::new(),
                others: BTreeSet::new(),
            }),
        )
    }

    /// A request of a full listing: its files and folders, then its next
    /// request, and after its last one `files/` and `folders/` complete,
    /// without the files and the folders of the projection that a full
    /// listing other than the first lacks. A token of a page that Google
    /// Drive refuses with `400` begins the listing again, and a page for
    /// which it searched not every drive is the error of the request.
    async fn listing(
        &mut self,
        first: bool,
        changes: String,
        page: Option<String>,
        [mut files, mut folders, mut others]: [BTreeSet<Id>; 3],
    ) -> Result<Feed> {
        let mut request = self
            .cx
            .endpoint(DriveApi)
            .get("/drive/v3/files")
            .query("corpora", "allDrives")
            .query("includeItemsFromAllDrives", "true")
            .query("supportsAllDrives", "true")
            .query("q", "trashed = false")
            .query("pageSize", PAGE)
            .query(
                "fields",
                format!("nextPageToken,incompleteSearch,files({FIELDS})"),
            );
        if let Some(page) = &page {
            request = request.query("pageToken", page);
        }
        let response = request.send().await?;
        match response.status() {
            200 => {},
            400 if page.is_some() => {
                return Ok(Feed::restart(
                    encode(&DriveToken {
                        changes: None,
                        stage: Some(Stage::Listing {
                            first,
                            changes,
                            page: None,
                            files: BTreeSet::new(),
                            folders: BTreeSet::new(),
                            others: BTreeSet::new(),
                        }),
                    })?,
                    status_error(&response),
                ));
            },
            _ => return Err(status_error(&response)),
        }
        let list: FileList = decode(response.body())?;
        if list.incomplete_search {
            return Err(ProviderError::invalid_input(
                "Google Drive searched not every drive for a full listing",
            ));
        }
        let write = if first {
            ListingWrite::Partial
        } else {
            ListingWrite::Complete
        };
        for object in &list.files {
            match self.take(object, write)? {
                Taken::File(id) if !first => {
                    files.insert(id);
                },
                Taken::Folder(id) if !first => {
                    folders.insert(id);
                },
                Taken::Other(id) if !first => {
                    others.insert(id);
                },
                Taken::File(_) | Taken::Folder(_) | Taken::Other(_) | Taken::Trashed => {},
            }
        }
        if let Some(page) = list.next_page_token {
            return self.part(
                None,
                Some(Stage::Listing {
                    first,
                    changes,
                    page: Some(page),
                    files,
                    folders,
                    others,
                }),
            );
        }
        self.cx.dir(FILES, ListingWrite::Complete)?;
        self.cx.dir(FOLDERS, ListingWrite::Complete)?;
        self.cx.dir(OTHERS, ListingWrite::Complete)?;
        if !first {
            for id in listed(self.cx, FILES)?.difference(&files) {
                self.remove_file(id)?;
            }
            for id in listed(self.cx, FOLDERS)?.difference(&folders) {
                self.remove_folder(id)?;
            }
            for id in listed(self.cx, OTHERS)?.difference(&others) {
                self.remove_other(id)?;
            }
        }
        self.part(Some(changes), None)
    }

    /// A request of the changes after `changes`, from the token `page`: the
    /// files and the folders that it brings, and the removal of each that the
    /// account no longer has or that is in the trash; then its next request,
    /// and after its last one the token of the changes after it. A token that
    /// Google Drive refuses with `400` begins a full listing again.
    async fn changes(&mut self, changes: String, page: String) -> Result<Feed> {
        let response = self
            .cx
            .endpoint(DriveApi)
            .get("/drive/v3/changes")
            .query("pageToken", &page)
            .query("includeItemsFromAllDrives", "true")
            .query("supportsAllDrives", "true")
            .query("includeRemoved", "true")
            .query("pageSize", PAGE)
            .query(
                "fields",
                format!(
                    "nextPageToken,newStartPageToken,changes(changeType,fileId,removed,file({FIELDS}))"
                ),
            )
            .send()
            .await?;
        match response.status() {
            200 => {},
            400 => {
                return Ok(Feed::restart(
                    encode(&DriveToken {
                        changes: None,
                        stage: None,
                    })?,
                    status_error(&response),
                ));
            },
            _ => return Err(status_error(&response)),
        }
        let list: ChangeList = decode(response.body())?;
        for change in &list.changes {
            self.change(change)?;
        }
        match (list.next_page_token, list.new_start_page_token) {
            (Some(page), _) => self.part(Some(changes), Some(Stage::Changes { page })),
            (None, Some(changes)) => self.part(Some(changes), None),
            (None, None) => Err(ProviderError::invalid_input(
                "the last page of the changes of Google Drive has no newStartPageToken",
            )),
        }
    }

    /// Take a change of a file or a folder into the projection: its object,
    /// or its removal, as a file and as a folder, for one that the account
    /// no longer has.
    fn change(&mut self, change: &Change) -> Result<()> {
        if change.of == ChangeType::Drive {
            return Ok(());
        }
        match &change.file {
            Some(object) if !change.removed => {
                self.take(object, ListingWrite::Complete)?;
            },
            _ => {
                let id = change.file_id.as_deref().ok_or_else(|| {
                    ProviderError::invalid_input("a change of a file of Google Drive has no fileId")
                })?;
                let id = Id::of(id)?;
                self.remove_file(&id)?;
                self.remove_folder(&id)?;
                self.remove_other(&id)?;
            },
        }
        Ok(())
    }

    /// Take an item of an answer into the projection, its folder joining its
    /// listing as `write` says, with the removal of the content of a file
    /// whose content changed, and of an item in the trash.
    fn take(&mut self, object: &RawValue, write: ListingWrite) -> Result<Taken> {
        let item: Item = decode(object.get().as_bytes())?;
        let id = Id::of(&item.id)?;
        let kind = item.kind();
        if item.trashed {
            match kind {
                Kind::Folder => self.remove_folder(&id)?,
                Kind::File(_) => self.remove_file(&id)?,
                Kind::Other => self.remove_other(&id)?,
            }
            return Ok(Taken::Trashed);
        }
        match kind {
            Kind::Folder => {
                write_folder(self.cx, &id, &item, object, write)?;
                self.inner
                    .push(Inner::Folder(id.clone(), encode(&Some(FolderToken {}))?));
                Ok(Taken::Folder(id))
            },
            Kind::File(content) => {
                let revision = item.revision(content)?;
                let projected = projected_revision(self.cx, &id)?;
                if projected.as_deref() != Some(revision) {
                    if projected.is_some() {
                        self.cx.remove(format!("{}/content", file_path(&id)))?;
                    }
                    let token = FileToken {
                        revision: revision.to_owned(),
                    };
                    self.inner
                        .push(Inner::File(id.clone(), encode(&Some(token))?));
                }
                write_file(self.cx, &id, &item, object, write)?;
                Ok(Taken::File(id))
            },
            Kind::Other => {
                write_other(self.cx, &id, &item, object, write)?;
                self.inner
                    .push(Inner::Other(id.clone(), encode(&Some(OtherToken {}))?));
                Ok(Taken::Other(id))
            },
        }
    }

    fn remove_file(&mut self, id: &Id) -> Result<()> {
        self.cx.remove(file_path(id))?;
        self.inner
            .push(Inner::File(id.clone(), encode(&None::<FileToken>)?));
        Ok(())
    }

    fn remove_folder(&mut self, id: &Id) -> Result<()> {
        self.cx.remove(folder_path(id))?;
        self.inner
            .push(Inner::Folder(id.clone(), encode(&None::<FolderToken>)?));
        Ok(())
    }

    fn remove_other(&mut self, id: &Id) -> Result<()> {
        self.cx.remove(other_path(id))?;
        self.inner
            .push(Inner::Other(id.clone(), encode(&None::<OtherToken>)?));
        Ok(())
    }

    /// A part with the token of the changes and `stage`, followed by more
    /// parts while a stage remains, with the inner states of its results.
    fn part(&mut self, changes: Option<String>, stage: Option<Stage>) -> Result<Feed> {
        let more = stage.is_some();
        let mut part = Feed::part(encode(&DriveToken { changes, stage })?).more(more);
        for inner in self.inner.drain(..) {
            part = match inner {
                Inner::File(id, token) => part.inner::<File>(&FileKey { id }, token),
                Inner::Folder(id, token) => part.inner::<Folder>(&FolderKey { id }, token),
                Inner::Other(id, token) => part.inner::<Other>(&OtherKey { id }, token),
            };
        }
        Ok(part.into())
    }
}

// ===========================================================================
// The feed of a file
// ===========================================================================

impl File {
    /// The feed of a file: its content, at the revision of the projection,
    /// for the start `content` of a file with a token; else the file by its ID, its facts with its folder
    /// joining `files/`, or its removal for `404`, in the trash, or that the
    /// mount has not, with the removal of its content for a file whose
    /// content changed, and then its content for the start `content`, or for
    /// the due start `content` of a file whose content changed.
    pub(crate) async fn feed(cx: FeedCx<FileKey>) -> Result<Feed> {
        let id = &cx.key().id;
        let token = cx
            .token()
            .map(decode_token::<Option<FileToken>>)
            .transpose()?
            .flatten();
        if let (Some(_), Some("content")) = (&token, cx.start())
            && let Some(object) = cx.read(format!("{}/file.json", file_path(id)))?
        {
            let item: Item = decode(&object)?;
            if let Kind::File(content) = item.kind() {
                return file_content(&cx, id, content, item.revision(content)?).await;
            }
        }

        let response = cx
            .endpoint(DriveApi)
            .get(format!("/drive/v3/files/{id}"))
            .query("supportsAllDrives", "true")
            .query("fields", FIELDS)
            .send()
            .await?;
        let object: Box<RawValue> = match response.status() {
            200 => decode(response.body())?,
            404 => return file_removal(&cx, id),
            _ => return Err(status_error(&response)),
        };
        let item: Item = decode(object.get().as_bytes())?;
        let content = match item.kind() {
            Kind::File(content) if !item.trashed => content,
            _ => return file_removal(&cx, id),
        };
        let revision = item.revision(content)?;
        let changed = token
            .as_ref()
            .is_some_and(|token| token.revision != revision);
        write_file(&cx, id, &item, &object, ListingWrite::Join)?;
        let start = if changed { cx.due() } else { cx.start() };
        if start == Some("content") {
            return file_content(&cx, id, content, revision).await;
        }
        // The content of another revision leaves; the content that this
        // answer takes is in its place.
        if changed {
            cx.remove(format!("{}/content", file_path(id)))?;
        }
        file_part(revision)
    }
}

/// The content of a file at `revision` as its file `content`: the bytes of
/// an uploaded file with that checksum, which the host fetches and holds, or
/// the export of a file of Google; the removal of a file with `404`.
async fn file_content(
    cx: &FeedCx<FileKey>,
    id: &Id,
    content: Content,
    revision: &str,
) -> Result<Feed> {
    let at = format!("{}/content", file_path(id));
    match content {
        Content::Bytes => {
            let bytes = || {
                cx.endpoint(DriveApi)
                    .get(format!("/drive/v3/files/{id}"))
                    .query("alt", "media")
                    .query("supportsAllDrives", "true")
            };
            match bytes().into_blob().version(revision).fetch().await {
                Ok(blob) => cx.blob(at, &blob)?,
                Err(error) if error.kind() == ProviderErrorKind::NotFound => {
                    return file_removal(cx, id);
                },
                // The reason of a refusal is in the body of its answer, which
                // the host holds: the answer to the first byte has it too,
                // as a request over a quota.
                Err(error) if error.kind() == ProviderErrorKind::Denied => {
                    let refusal = bytes().header("Range", "bytes=0-0").send().await?;
                    return Err(match refusal.status() {
                        200..=299 => error,
                        _ => status_error(&refusal),
                    });
                },
                Err(error) => return Err(error),
            }
        },
        Content::Export(mime_type) => {
            let response = cx
                .endpoint(DriveApi)
                .get(format!("/drive/v3/files/{id}/export"))
                .query("mimeType", mime_type)
                .send()
                .await?;
            match response.status() {
                200 => cx.file(at, response.body())?,
                404 => return file_removal(cx, id),
                _ => return Err(status_error(&response)),
            }
        },
    }
    file_part(revision)
}

/// A part of the feed of a file with what changes with its content.
fn file_part(revision: &str) -> Result<Feed> {
    let token = FileToken {
        revision: revision.to_owned(),
    };
    Ok(Feed::part(encode(&Some(token))?).into())
}

/// The removal of a file, with the token of a removed file.
fn file_removal(cx: &FeedCx<FileKey>, id: &Id) -> Result<Feed> {
    cx.remove(file_path(id))?;
    Ok(Feed::part(encode(&None::<FileToken>)?).into())
}

// ===========================================================================
// The feed of a folder
// ===========================================================================

impl Folder {
    /// The feed of a folder: the folder by its ID, its facts with its folder
    /// joining `folders/`, or its removal for `404`, in the trash, or of no
    /// folder.
    pub(crate) async fn feed(cx: FeedCx<FolderKey>) -> Result<Feed> {
        let id = &cx.key().id;
        let response = cx
            .endpoint(DriveApi)
            .get(format!("/drive/v3/files/{id}"))
            .query("supportsAllDrives", "true")
            .query("fields", FIELDS)
            .send()
            .await?;
        let object: Box<RawValue> = match response.status() {
            200 => decode(response.body())?,
            404 => return folder_removal(&cx, id),
            _ => return Err(status_error(&response)),
        };
        let item: Item = decode(object.get().as_bytes())?;
        if item.trashed || item.kind() != Kind::Folder {
            return folder_removal(&cx, id);
        }
        write_folder(&cx, id, &item, &object, ListingWrite::Join)?;
        Ok(Feed::part(encode(&Some(FolderToken {}))?).into())
    }
}

/// The removal of a folder, with the token of a removed folder.
fn folder_removal(cx: &FeedCx<FolderKey>, id: &Id) -> Result<Feed> {
    cx.remove(folder_path(id))?;
    Ok(Feed::part(encode(&None::<FolderToken>)?).into())
}

// ===========================================================================
// The feed of another item
// ===========================================================================

impl Other {
    /// The feed of another item: the item by its ID, its facts with its
    /// folder joining `others/`, or its removal for `404`, in the trash, or
    /// of a file or a folder.
    pub(crate) async fn feed(cx: FeedCx<OtherKey>) -> Result<Feed> {
        let id = &cx.key().id;
        let response = cx
            .endpoint(DriveApi)
            .get(format!("/drive/v3/files/{id}"))
            .query("supportsAllDrives", "true")
            .query("fields", FIELDS)
            .send()
            .await?;
        let removal = || {
            cx.remove(other_path(id))?;
            Ok(Feed::part(encode(&None::<OtherToken>)?).into())
        };
        let object: Box<RawValue> = match response.status() {
            200 => decode(response.body())?,
            404 => return removal(),
            _ => return Err(status_error(&response)),
        };
        let item: Item = decode(object.get().as_bytes())?;
        if item.trashed || item.kind() != Kind::Other {
            return removal();
        }
        write_other(&cx, id, &item, &object, ListingWrite::Join)?;
        Ok(Feed::part(encode(&Some(OtherToken {}))?).into())
    }
}
