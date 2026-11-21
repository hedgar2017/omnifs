//! The feeds of the files and the folders of the Drive.
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

use omnifs_sdk::prelude::*;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::api::{
    Change, ChangeList, ChangeType, Content, DriveApi, FIELDS, FileList, Item, Kind,
    StartPageToken, decode, status_error,
};
use crate::{Drive, DriveKey, File, FileKey, Folder, FolderKey, Id};

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

fn file_path(id: &Id) -> String {
    format!("{FILES}/{id}")
}

fn folder_path(id: &Id) -> String {
    format!("{FOLDERS}/{id}")
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

/// The file of the projection with `id`, by its `file.json`: what changes
/// with its content, none for a file that the projection has not.
fn projected_revision<K>(cx: &FeedCx<K>, id: &Id) -> Result<Option<String>> {
    let Some(object) = cx.read(format!("{}/file.json", file_path(id)))? else {
        return Ok(None);
    };
    let item: Item = decode(&object)?;
    match item.kind()? {
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
}

/// What a file or a folder of an answer is in the projection after it.
enum Taken {
    File(Id),
    Folder(Id),
    Neither,
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
                    }),
                ..
            }) => feed.listing(first, changes, page, files, folders).await,
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
        mut files: BTreeSet<Id>,
        mut folders: BTreeSet<Id>,
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
                Taken::File(_) | Taken::Folder(_) | Taken::Neither => {},
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
                }),
            );
        }
        self.cx.dir(FILES, ListingWrite::Complete)?;
        self.cx.dir(FOLDERS, ListingWrite::Complete)?;
        if !first {
            for id in listed(self.cx, FILES)?.difference(&files) {
                self.remove_file(id)?;
            }
            for id in listed(self.cx, FOLDERS)?.difference(&folders) {
                self.remove_folder(id)?;
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
            },
        }
        Ok(())
    }

    /// Take a file or a folder of an answer into the projection, its folder
    /// joining its listing as `write` says, with the removal of the content
    /// of a file whose content changed, and of an item in the trash or that
    /// the mount has not.
    fn take(&mut self, object: &RawValue, write: ListingWrite) -> Result<Taken> {
        let item: Item = decode(object.get().as_bytes())?;
        let id = Id::of(&item.id)?;
        let kind = item.kind()?;
        if item.trashed {
            match kind {
                Kind::Folder => self.remove_folder(&id)?,
                Kind::File(_) | Kind::Other => self.remove_file(&id)?,
            }
            return Ok(Taken::Neither);
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
                self.remove_file(&id)?;
                Ok(Taken::Neither)
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

    /// A part with the token of the changes and `stage`, followed by more
    /// parts while a stage remains, with the inner states of its results.
    fn part(&mut self, changes: Option<String>, stage: Option<Stage>) -> Result<Feed> {
        let more = stage.is_some();
        let mut part = Feed::part(encode(&DriveToken { changes, stage })?).more(more);
        for inner in self.inner.drain(..) {
            part = match inner {
                Inner::File(id, token) => part.inner::<File>(&FileKey { id }, token),
                Inner::Folder(id, token) => part.inner::<Folder>(&FolderKey { id }, token),
            };
        }
        Ok(part.into())
    }
}

// ===========================================================================
// The feed of a file
// ===========================================================================

impl File {
    /// The feed of a file: its content for the start `content` of a file
    /// with a token; else the file by its ID, its facts with its folder
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
        if let (Some(token), Some("content")) = (&token, cx.start())
            && let Some(object) = cx.read(format!("{}/file.json", file_path(id)))?
        {
            let item: Item = decode(&object)?;
            if let Kind::File(content) = item.kind()? {
                return file_content(&cx, id, content, &token.revision, true).await;
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
        let content = match item.kind()? {
            Kind::File(content) if !item.trashed => content,
            _ => return file_removal(&cx, id),
        };
        let revision = item.revision(content)?;
        let changed = token
            .as_ref()
            .is_some_and(|token| token.revision != revision);
        if changed {
            cx.remove(format!("{}/content", file_path(id)))?;
        }
        write_file(&cx, id, &item, &object, ListingWrite::Join)?;
        let start = if changed { cx.due() } else { cx.start() };
        if start == Some("content") {
            return file_content(&cx, id, content, revision, false).await;
        }
        file_part(revision)
    }
}

/// The content of a file at `revision`: the bytes of an uploaded file, or
/// the export of a file of Google, as its file `content`; the removal of a
/// file with `404`. For `projected`, a revision of the projection from
/// before the request, the content is of the file only while the projection
/// still has that revision after it.
async fn file_content(
    cx: &FeedCx<FileKey>,
    id: &Id,
    content: Content,
    revision: &str,
    projected: bool,
) -> Result<Feed> {
    let endpoint = cx.endpoint(DriveApi);
    let request = match content {
        Content::Bytes => endpoint
            .get(format!("/drive/v3/files/{id}"))
            .query("alt", "media")
            .query("supportsAllDrives", "true"),
        Content::Export(mime_type) => endpoint
            .get(format!("/drive/v3/files/{id}/export"))
            .query("mimeType", mime_type),
    };
    let response = request.send().await?;
    match response.status() {
        200 => {},
        404 => return file_removal(cx, id),
        _ => return Err(status_error(&response)),
    }
    // A change of the file during the request left another revision in the
    // projection, whose content the next read takes.
    if !projected || projected_revision(cx, id)?.as_deref() == Some(revision) {
        cx.file(format!("{}/content", file_path(id)), response.body())?;
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
        if item.trashed || item.kind()? != Kind::Folder {
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
