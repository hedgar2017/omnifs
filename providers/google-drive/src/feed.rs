//! The feeds of the files, the folders, and the other items of the Drive.
//!
//! The feed of the Drive takes a part for each request: at its first answer
//! the token of the changes after now and then a full listing of the items,
//! and at each later answer the changes after the token of the answer before
//! it. A full listing lists the items of the user, and then those of each
//! shared drive of the account on its own, which Google Drive searches whole,
//! as it does not each drive of a search of all of them. A request of the
//! changes that Google Drive refuses with `400` for its token begins a full
//! listing again, which the items of the projection that it lacks leave, and
//! a request of a full listing that it refuses with `400` for the token of
//! its page begins the listing again.
//!
//! The own feed of an item takes it by its ID. An item that the feed of the
//! Drive did not bring is unlisted: the host asks its own feed past its
//! deadline at each operation on it, since Google Drive reports no change of
//! it. The own feed of a file takes its content too: at the revision of the
//! projection, and at the revision of Google Drive by its ID for the bytes
//! of another MD5 than those of the projection.
//!
//! An item whose object has an ID, a MIME type, and `trashed` is in the
//! mount. A file that Google Drive states of it beside those is in its
//! folder, and the operation on another one has the error that Google Drive
//! states not what it needs, at the request of the item by its ID: its
//! `name` without the name of its object, and the `content` of a file
//! without the MD5 or the revision of its bytes, or the time of the
//! modification of its export.

use omnifs_sdk::error::ProviderErrorKind;
use omnifs_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::api::{
    ChangeList, ChangeType, Content, DriveApi, DriveChange, DriveList, FIELDS, FileList, Item,
    Kind, StartPageToken, decode,
};
use crate::{Drive, DriveKey, File, Folder, Id, ItemKey, Other};

/// The items of a request of a full listing, and the changes of a request of
/// the changes: the most that Google Drive gives.
const PAGE: usize = 1000;

/// The shared drives of a request of them: the most that Google Drive gives.
const DRIVES: usize = 100;

/// The bytes of one read of an object file of the projection, which has no
/// bound of its own.
const RANGE: u32 = 64 * 1024;

/// The token of the feed of the Drive: its next request, that of the token
/// of the changes after now for a first full listing without one.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DriveToken {
    /// The token of the changes after now for a full listing other than the
    /// first.
    Relisting,
    /// The page of `corpus` in a full listing at its token, the first page
    /// without one; `changes` is the token of the changes after the start of
    /// the listing, and `first` says that the listing is the first.
    Listing {
        first: bool,
        changes: String,
        corpus: Corpus,
        page: Option<String>,
    },
    /// The changes at the token `page`.
    Changes { page: String },
}

/// A corpus of a full listing, in its order: the items of the user, the
/// pages of the shared drives, and the items of each.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Corpus {
    User,
    /// A page of the shared drives.
    Drives,
    /// The items of the shared drive `id`; `next` are the drives after it
    /// of its page of drives, and `drives` is the token of the next page.
    Drive {
        id: String,
        next: Vec<String>,
        drives: Option<String>,
    },
}

/// The token of the own feed of an item that the mount has; `None` for one
/// that it has not.
#[derive(Clone, Serialize, Deserialize)]
struct ItemState {
    /// Whether the feed of the Drive brought the item, and brings its
    /// changes.
    listed: bool,
    /// What changes with the content of a file.
    revision: Option<String>,
}

/// The directory of the items of `kind`.
fn dir(kind: Kind) -> &'static str {
    match kind {
        Kind::File => "/files",
        Kind::Folder => "/folders",
        Kind::Other => "/others",
    }
}

fn path(kind: Kind, id: &Id) -> String {
    format!("{}/{id}", dir(kind))
}

/// The file of the object of an item of `kind`.
fn object_path(kind: Kind, id: &Id) -> String {
    let file = match kind {
        Kind::File => "file.json",
        Kind::Folder => "folder.json",
        Kind::Other => "item.json",
    };
    format!("{}/{file}", path(kind, id))
}

fn content_path(id: &Id) -> String {
    format!("{}/content", path(Kind::File, id))
}

/// Write the folder of an item, which joins the listing of its kind as
/// `listed` says, and the files of its object: `name`, which leaves for an
/// object without a name, and the object as it came.
fn write_item<K>(
    cx: &FeedCx<K>,
    kind: Kind,
    item: &Item,
    object: &RawValue,
    listed: ListingWrite,
) -> Result<()> {
    // A file has `content` beside the files of its object.
    let faces = match kind {
        Kind::File => ListingWrite::Partial,
        Kind::Folder | Kind::Other => ListingWrite::Complete,
    };
    let at = path(kind, &item.id);
    cx.dir(dir(kind), listed)?;
    cx.dir(&at, faces)?;
    match &item.name {
        Some(name) => cx.file(format!("{at}/name"), name.as_bytes())?,
        None => cx.remove(format!("{at}/name"))?,
    }
    cx.file(object_path(kind, &item.id), object.get().as_bytes())?;
    Ok(())
}

/// The whole of the fed file at `path` of the projection, by its ranges;
/// none for a path without one.
fn read_object<K>(cx: &FeedCx<K>, path: &str) -> Result<Option<Vec<u8>>> {
    let Some(first) = cx.read_range(path, 0, RANGE)? else {
        return Ok(None);
    };
    let mut bytes = first.bytes;
    while (bytes.len() as u64) < first.length {
        let range = cx
            .read_range(path, bytes.len() as u64, RANGE)?
            .ok_or_else(|| ProviderError::internal(format!("{path} left while it was read")))?;
        bytes.extend(range.bytes);
    }
    Ok(Some(bytes))
}

/// The object of the item `id` of `kind` in the projection; none for an
/// item that the projection has not of that kind.
fn projected<K>(cx: &FeedCx<K>, kind: Kind, id: &Id) -> Result<Option<Item>> {
    read_object(cx, &object_path(kind, id))?
        .map(|object| decode(&object))
        .transpose()
}

/// The kinds of which the projection has the item `id`.
fn present<K>(cx: &FeedCx<K>, id: &Id) -> Result<Vec<Kind>> {
    let mut kinds = Vec::new();
    for kind in Kind::ALL {
        if cx.read_range(object_path(kind, id), 0, 0)?.is_some() {
            kinds.push(kind);
        }
    }
    Ok(kinds)
}

/// The object of the item `id` by its ID as it came; none for `404`.
async fn by_id<K>(cx: &FeedCx<K>, id: &Id) -> Result<Option<Box<RawValue>>> {
    let response = cx
        .endpoint(DriveApi)
        .get(format!("/drive/v3/files/{id}"))
        .query("supportsAllDrives", "true")
        .query("fields", FIELDS)
        .send()
        .await?;
    match response.status() {
        200 => Ok(Some(decode(response.body())?)),
        404 => Ok(None),
        status => Err(answer_error(&response, status)),
    }
}

fn answer_error(response: &omnifs_sdk::endpoint::HttpResponse, status: u16) -> ProviderError {
    ProviderError::from_http_answer(
        status,
        response
            .headers()
            .iter()
            .filter_map(|(name, value)| Some((name.as_str(), value.to_str().ok()?))),
    )
}

/// One call of the feed of the Drive: its context and the tokens of the own
/// feeds of the items of its part.
struct DriveFeed<'a> {
    cx: &'a FeedCx<DriveKey>,
    inner: Vec<(Kind, Id, Option<ItemState>)>,
}

impl Drive {
    pub(crate) async fn feed(cx: FeedCx<DriveKey>) -> Result<Feed> {
        let mut feed = DriveFeed {
            cx: &cx,
            inner: Vec::new(),
        };
        match cx.token::<DriveToken>()? {
            None => feed.start(true).await,
            Some(DriveToken::Relisting) => feed.start(false).await,
            Some(DriveToken::Listing {
                first,
                changes,
                corpus,
                page,
            }) => feed.listing(first, changes, corpus, page).await,
            Some(DriveToken::Changes { page }) => feed.changes(page).await,
        }
    }
}

impl DriveFeed<'_> {
    /// The token of the changes after now, before the first request of a
    /// full listing, whose answers the changes after it complete. A listing
    /// other than the first begins with the removal of each item of the
    /// projection: the host keeps the items that the listing writes after
    /// it, with their content, so that the ones that it lacks leave.
    async fn start(&mut self, first: bool) -> Result<Feed> {
        let start: StartPageToken = self
            .cx
            .endpoint(DriveApi)
            .get("/drive/v3/changes/startPageToken")
            .query("supportsAllDrives", "true")
            .json()
            .await?;
        if !first {
            for kind in Kind::ALL {
                if let Some(listed) = self.cx.names::<Id>(dir(kind))? {
                    for id in listed.names {
                        self.remove(kind, id)?;
                    }
                }
            }
        }
        self.part(
            &DriveToken::Listing {
                first,
                changes: start.start_page_token,
                corpus: Corpus::User,
                page: None,
            },
            true,
        )
    }

    /// A request of a full listing in `corpus` from the token `page`: its
    /// items, or the shared drives, then its next request, and after its
    /// last one the listing of each kind complete. A token of a page that
    /// Google Drive refuses with `400` begins the listing again, and a page
    /// for which it searched not all of its corpus is the error of the
    /// request.
    async fn listing(
        &mut self,
        first: bool,
        changes: String,
        corpus: Corpus,
        page: Option<String>,
    ) -> Result<Feed> {
        let endpoint = self.cx.endpoint(DriveApi);
        let mut request = match &corpus {
            Corpus::Drives => endpoint
                .get("/drive/v3/drives")
                .query("pageSize", DRIVES)
                .query("fields", "nextPageToken,drives(id)"),
            Corpus::User | Corpus::Drive { .. } => {
                let request = match &corpus {
                    Corpus::Drive { id, .. } => endpoint
                        .get("/drive/v3/files")
                        .query("corpora", "drive")
                        .query("driveId", id),
                    _ => endpoint.get("/drive/v3/files").query("corpora", "user"),
                };
                request
                    .query("includeItemsFromAllDrives", "true")
                    .query("supportsAllDrives", "true")
                    .query("q", "trashed = false")
                    .query("pageSize", PAGE)
                    .query(
                        "fields",
                        format!("nextPageToken,incompleteSearch,files({FIELDS})"),
                    )
            },
        };
        if let Some(page) = &page {
            request = request.query("pageToken", page);
        }
        let response = request.send().await?;
        match response.status() {
            200 => {},
            400 if page.is_some() => {
                // The answers of a first listing are in the projection, and
                // the changes after its start complete them; the host drops
                // the answers of a later one with its removals.
                let again = if first {
                    DriveToken::Listing {
                        first,
                        changes,
                        corpus: Corpus::User,
                        page: None,
                    }
                } else {
                    DriveToken::Relisting
                };
                return Feed::restart(&again, answer_error(&response, 400));
            },
            status => return Err(answer_error(&response, status)),
        }
        let next = if let Corpus::Drives = corpus {
            let list: DriveList = decode(response.body())?;
            let mut ids = list.drives.into_iter().map(|drive| drive.id);
            match (ids.next(), list.next_page_token) {
                (Some(id), drives) => Some((
                    Corpus::Drive {
                        id,
                        next: ids.collect(),
                        drives,
                    },
                    None,
                )),
                (None, Some(drives)) => Some((Corpus::Drives, Some(drives))),
                (None, None) => None,
            }
        } else {
            let list: FileList = decode(response.body())?;
            if list.incomplete_search {
                return Err(ProviderError::invalid_input(
                    "Google Drive searched not all of a corpus for a full listing",
                ));
            }
            for object in &list.files {
                self.take(object, ListingWrite::Partial)?;
            }
            match (list.next_page_token, corpus) {
                (Some(page), corpus) => Some((corpus, Some(page))),
                (None, Corpus::User) => Some((Corpus::Drives, None)),
                (None, Corpus::Drive { next, drives, .. }) => {
                    let mut next = next.into_iter();
                    match (next.next(), drives) {
                        (Some(id), drives) => Some((
                            Corpus::Drive {
                                id,
                                next: next.collect(),
                                drives,
                            },
                            None,
                        )),
                        (None, Some(drives)) => Some((Corpus::Drives, Some(drives))),
                        (None, None) => None,
                    }
                },
                (None, Corpus::Drives) => unreachable!("a page of drives has no items"),
            }
        };
        if let Some((corpus, page)) = next {
            return self.part(
                &DriveToken::Listing {
                    first,
                    changes,
                    corpus,
                    page,
                },
                true,
            );
        }
        for kind in Kind::ALL {
            self.cx.dir(dir(kind), ListingWrite::Complete)?;
        }
        self.part(&DriveToken::Changes { page: changes }, false)
    }

    /// A request of the changes at the token `page`: the items that it
    /// brings, and the removal of each that the account no longer has, that
    /// is in the trash, or that is in a shared drive that the account no
    /// longer has; then its next request, and after its last one the token
    /// of the changes after it. A token that Google Drive refuses with `400`
    /// begins a full listing again.
    async fn changes(&mut self, page: String) -> Result<Feed> {
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
                    "nextPageToken,newStartPageToken,changes(changeType,fileId,driveId,removed,file({FIELDS}))"
                ),
            )
            .send()
            .await?;
        match response.status() {
            200 => {},
            400 => {
                return Feed::restart(&DriveToken::Relisting, answer_error(&response, 400));
            },
            status => return Err(answer_error(&response, status)),
        }
        let list: ChangeList = decode(response.body())?;
        for change in &list.changes {
            self.change(change).await?;
        }
        match (list.next_page_token, list.new_start_page_token) {
            (Some(page), _) => self.part(&DriveToken::Changes { page }, true),
            (None, Some(page)) => self.part(&DriveToken::Changes { page }, false),
            (None, None) => Err(ProviderError::invalid_input(
                "the last page of the changes of Google Drive has no newStartPageToken",
            )),
        }
    }

    /// Take a change into the projection: the object of an item, which the
    /// item by its ID gives for a change without it, or its removal, and the
    /// removal of each item of a shared drive that the account no longer
    /// has.
    async fn change(&mut self, change: &DriveChange) -> Result<()> {
        if change.change_type == ChangeType::Drive {
            let drive = change.drive_id.as_deref().ok_or_else(|| {
                ProviderError::invalid_input("a change of a shared drive has no driveId")
            })?;
            if change.removed {
                self.leave_drive(drive)?;
            }
            return Ok(());
        }
        if let (Some(object), false) = (&change.file, change.removed) {
            return self.take(object, ListingWrite::Complete);
        }
        let id = change.file_id.as_ref().ok_or_else(|| {
            ProviderError::invalid_input("a change of an item of Google Drive has no fileId")
        })?;
        let object = if change.removed {
            None
        } else {
            by_id(self.cx, id).await?
        };
        match object {
            Some(object) => self.take(&object, ListingWrite::Complete),
            None => self.leave(id, None),
        }
    }

    /// Take the item of an object of an answer into the projection, under
    /// its kind alone, with its folder joining the listing of its kind as
    /// `listed` says and the removal of the content of a file whose content
    /// changed; the removal of an item in the trash.
    fn take(&mut self, object: &RawValue, listed: ListingWrite) -> Result<()> {
        let item: Item = decode(object.get().as_bytes())?;
        if item.trashed {
            return self.leave(&item.id, None);
        }
        let kind = item.kind();
        self.leave(&item.id, Some(kind))?;
        let revision = item
            .content()
            .and_then(|content| content.revision().map(str::to_owned));
        if let Some(projected) = projected(self.cx, Kind::File, &item.id)?
            && projected
                .content()
                .and_then(|content| content.revision().map(str::to_owned))
                != revision
        {
            self.cx.remove(content_path(&item.id))?;
        }
        write_item(self.cx, kind, &item, object, listed)?;
        self.inner.push((
            kind,
            item.id,
            Some(ItemState {
                listed: true,
                revision,
            }),
        ));
        Ok(())
    }

    /// The removal of the item `id` under each kind of the projection other
    /// than `kept`.
    fn leave(&mut self, id: &Id, kept: Option<Kind>) -> Result<()> {
        for kind in present(self.cx, id)? {
            if Some(kind) != kept {
                self.remove(kind, id.clone())?;
            }
        }
        Ok(())
    }

    /// The removal of each item of the projection in the shared drive
    /// `drive`.
    fn leave_drive(&mut self, drive: &str) -> Result<()> {
        for kind in Kind::ALL {
            let Some(listed) = self.cx.names::<Id>(dir(kind))? else {
                continue;
            };
            for id in listed.names {
                if projected(self.cx, kind, &id)?
                    .is_some_and(|item| item.drive_id.as_deref() == Some(drive))
                {
                    self.remove(kind, id)?;
                }
            }
        }
        Ok(())
    }

    fn remove(&mut self, kind: Kind, id: Id) -> Result<()> {
        self.cx.remove(path(kind, &id))?;
        self.inner.push((kind, id, None));
        Ok(())
    }

    /// A part with `token`, followed by `more` parts, with the tokens of the
    /// own feeds of its items.
    fn part(&mut self, token: &DriveToken, more: bool) -> Result<Feed> {
        let mut part = Feed::part(token)?.more(more);
        for (kind, id, state) in self.inner.drain(..) {
            let key = ItemKey { id };
            part = match kind {
                Kind::File => part.inner::<File>(&key, &state)?,
                Kind::Folder => part.inner::<Folder>(&key, &state)?,
                Kind::Other => part.inner::<Other>(&key, &state)?,
            };
        }
        Ok(part.into())
    }
}

fn removal(cx: &FeedCx<ItemKey>, kind: Kind, id: &Id) -> Result<Feed> {
    cx.remove(path(kind, id))?;
    Ok(Feed::part(&None::<ItemState>)?.into())
}

/// The part of the own feed of an item with `state`, which says that no
/// listing has an item that the feed of the Drive did not bring.
fn item_part(state: &ItemState) -> Result<Feed> {
    let part = Feed::part(&Some(state))?;
    Ok(if state.listed { part } else { part.unlisted() }.into())
}

/// The own feed of an item of `kind`: the item by its ID, or its removal
/// for `404`, in the trash, or of another kind, and the content of a file
/// for the start `content`, or for the due start `content` of a file whose
/// content changed; the content of another revision leaves. The content of
/// a file of the projection comes at its revision without a request of the
/// item, unless Google Drive has bytes of another MD5.
async fn own(cx: FeedCx<ItemKey>, kind: Kind) -> Result<Feed> {
    let id = &cx.key().id;
    let state = cx.token::<Option<ItemState>>()?.flatten();
    if let (Some(state), Some("content")) = (&state, cx.start())
        && let Some(item) = projected(&cx, kind, id)?
        && let Some(content) = item.content()
    {
        match take_content(&cx, id, &content).await {
            Ok(true) => return item_part(state),
            Ok(false) => return removal(&cx, kind, id),
            Err(error) if error.kind() == ProviderErrorKind::VersionMismatch => {},
            Err(error) => return Err(error),
        }
    }
    let Some(object) = by_id(&cx, id).await? else {
        return removal(&cx, kind, id);
    };
    let item: Item = decode(object.get().as_bytes())?;
    if item.trashed || item.kind() != kind {
        return removal(&cx, kind, id);
    }
    if cx.start() == Some("name") && item.name.is_none() {
        return Err(ProviderError::invalid_input(format!(
            "Google Drive states no name of {id}"
        )));
    }
    let content = item.content();
    let revision = content
        .as_ref()
        .and_then(|content| content.revision().map(str::to_owned));
    let changed = state
        .as_ref()
        .is_some_and(|state| state.revision != revision);
    write_item(&cx, kind, &item, &object, ListingWrite::Join)?;
    if let Some(content) = &content {
        if cx.start() == Some("content") || changed && cx.due() == Some("content") {
            if !take_content(&cx, id, content).await? {
                return removal(&cx, kind, id);
            }
        } else if changed {
            cx.remove(content_path(id))?;
        }
    }
    item_part(&ItemState {
        listed: state.is_some_and(|state| state.listed),
        revision,
    })
}

impl File {
    pub(crate) async fn feed(cx: FeedCx<ItemKey>) -> Result<Feed> {
        own(cx, Kind::File).await
    }
}

impl Folder {
    pub(crate) async fn feed(cx: FeedCx<ItemKey>) -> Result<Feed> {
        own(cx, Kind::Folder).await
    }
}

impl Other {
    pub(crate) async fn feed(cx: FeedCx<ItemKey>) -> Result<Feed> {
        own(cx, Kind::Other).await
    }
}

/// Take `content` as the file `content` of the file `id`, which the host
/// fetches and holds for its revision: the bytes of an uploaded file of its
/// MD5, or of the revision of its bytes, or the export of a file of Google.
/// Whether Google Drive has the file.
async fn take_content(cx: &FeedCx<ItemKey>, id: &Id, content: &Content<'_>) -> Result<bool> {
    let revision = content.revision().ok_or_else(|| {
        ProviderError::invalid_input(format!(
            "Google Drive states no revision of the content of {id}"
        ))
    })?;
    let request = || match content {
        Content::Bytes { .. } => cx
            .endpoint(DriveApi)
            .get(format!("/drive/v3/files/{id}"))
            .query("alt", "media")
            .query("supportsAllDrives", "true")
            .follow_redirects(),
        Content::Export { mime_type, .. } => cx
            .endpoint(DriveApi)
            .get(format!("/drive/v3/files/{id}/export"))
            .query("mimeType", mime_type)
            .follow_redirects(),
    };
    let blob = request().into_blob();
    let blob = match content {
        Content::Bytes { md5: Some(md5), .. } => blob.md5(*md5),
        Content::Bytes { md5: None, .. } | Content::Export { .. } => blob.version(revision),
    };
    let at = content_path(id);
    match blob.fetch().await {
        Ok(blob) => cx.blob(at, &blob)?,
        Err(error) if error.kind() == ProviderErrorKind::NotFound => return Ok(false),
        // The reason of a refusal is in the body of its answer, which the
        // host keeps not: an inline request has it too, as a request over a
        // quota. For the bytes, the request is of the first byte, which a
        // file without one has with `416`.
        Err(error) if error.kind() == ProviderErrorKind::Denied => {
            let refusal = match content {
                Content::Bytes { .. } => request().header("Range", "bytes=0-0"),
                Content::Export { .. } => request(),
            }
            .send()
            .await?;
            return match (refusal.status(), content) {
                (200, Content::Export { .. }) => {
                    cx.file(at, refusal.body())?;
                    Ok(true)
                },
                (200..=299 | 416, _) => Err(error),
                (404, _) => Ok(false),
                (status, _) => Err(answer_error(&refusal, status)),
            };
        },
        Err(error) => return Err(error),
    }
    Ok(true)
}
