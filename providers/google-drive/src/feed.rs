//! The feeds of the files, the folders, and the other items of the Drive.
//!
//! The feed of the Drive takes a part for each request: at its first answer
//! the token of the changes after now and then a full listing of the items,
//! and at each later answer the changes after the token of the answer before
//! it. A request of the changes that Google Drive refuses with `400` for its
//! token begins a full listing again, which the items of the projection that
//! it lacks leave, and a request of a full listing that it refuses with `400`
//! for the token of its page begins the listing again.
//!
//! The own feed of an item takes it by its ID. Google Drive reports no
//! change of an item that the feed of the Drive did not bring, so its facts
//! count as lacking [`BY_ID`] after their answer, and a read of them after it
//! takes the item again. The own feed of a file takes its content too, at
//! what Google Drive has of the file by its ID then.
//!
//! An item whose object the provider cannot read is none of the mount: it
//! leaves the projection, and its lookup has the error of its object by its
//! ID.

use std::time::{Duration, SystemTime};

use omnifs_sdk::error::ProviderErrorKind;
use omnifs_sdk::feed::FeedPart;
use omnifs_sdk::prelude::*;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::api::{
    Change, ChangeList, ChangeType, Content, DriveApi, FIELDS, FileList, Identified, Item, Kind,
    StartPageToken, decode,
};
use crate::{Drive, DriveKey, File, Folder, Id, ItemKey, Other};

/// The items of a request of a full listing, and the changes of a request of
/// the changes: the most that Google Drive gives.
const PAGE: usize = 1000;

/// The time that the facts of an item by its ID alone stand, of which Google
/// Drive reports no change: that of the facts of a feed before its deadline.
const BY_ID: Duration = Duration::from_secs(3);

/// The token of the feed of the Drive: its next request, that of the token
/// of the changes after now for a first full listing without one.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DriveToken {
    /// The token of the changes after now for a full listing other than the
    /// first.
    Relisting,
    /// The page of a full listing at its token, the first page without one;
    /// `changes` is the token of the changes after the start of the listing,
    /// and `first` says that the listing is the first.
    Listing {
        first: bool,
        changes: String,
        page: Option<String>,
    },
    /// The changes at the token `page`.
    Changes { page: String },
}

/// The token of the own feed of an item. The token that the feed of the
/// Drive gives differs from the one that an answer by the ID with older
/// facts began with, which the host refuses for it.
#[derive(Serialize, Deserialize)]
struct ItemToken {
    /// Whether the feed of the Drive brought the item, and brings its
    /// changes.
    listed: bool,
    /// What changes with the content of a file.
    revision: Option<String>,
    /// The object of an item by its ID alone whose folder the part before
    /// wrote, for the part of its facts.
    object: Option<Box<RawValue>>,
}

/// The token of the own feed of an item that the mount has not.
const REMOVED: &str = "null";

fn encode<T: Serialize>(token: &T) -> Result<String> {
    serde_json::to_string(token)
        .map_err(|error| ProviderError::internal(format!("feed token: {error}")))
}

fn decode_token<T: DeserializeOwned>(token: &str) -> Result<T> {
    serde_json::from_str(token)
        .map_err(|error| ProviderError::invalid_input(format!("feed token: {error}")))
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

fn content_path(id: &Id) -> String {
    format!("{}/content", path(Kind::File, id))
}

/// Write the folder of an item, which joins the listing of its kind as
/// `listed` says.
fn write_folder<K>(cx: &FeedCx<K>, kind: Kind, id: &Id, listed: ListingWrite) -> Result<()> {
    // A file has `content` beside the files of its object.
    let faces = match kind {
        Kind::File => ListingWrite::Partial,
        Kind::Folder | Kind::Other => ListingWrite::Complete,
    };
    cx.dir(dir(kind), listed)?;
    cx.dir(path(kind, id), faces)?;
    Ok(())
}

/// Write the facts of an item from its object: `name`, and the file of its
/// object as it came.
fn write_facts<K>(
    cx: &FeedCx<K>,
    kind: Kind,
    id: &Id,
    name: &str,
    object: &RawValue,
) -> Result<()> {
    let file = match kind {
        Kind::File => "file.json",
        Kind::Folder => "folder.json",
        Kind::Other => "item.json",
    };
    let at = path(kind, id);
    cx.file(format!("{at}/name"), name.as_bytes())?;
    cx.file(format!("{at}/{file}"), object.get().as_bytes())?;
    Ok(())
}

/// What changes with the content of the file `id` of the projection, by its
/// `file.json`; none for a file that the projection has not.
fn projected_revision<K>(cx: &FeedCx<K>, id: &Id) -> Result<Option<String>> {
    let Some(object) = cx.read(format!("{}/file.json", path(Kind::File, id)))? else {
        return Ok(None);
    };
    let item: Item = decode(&object)?;
    let content = item
        .content()
        .ok_or_else(|| ProviderError::internal(format!("the file.json of {id} is of no file")))?;
    Ok(Some(content.revision().to_owned()))
}

/// The IDs of the items of `kind` of the projection, by their listing.
fn listing<K>(cx: &FeedCx<K>, kind: Kind) -> Result<Vec<Id>> {
    let at = dir(kind);
    let Some(ListingFacts {
        entries: ListingEntries::Names(names),
        ..
    }) = cx.listing(at, ListingForm::Names)?
    else {
        return Err(ProviderError::internal(format!(
            "the projection has no names of {at}"
        )));
    };
    names
        .iter()
        .map(|name| {
            name.parse()
                .map_err(|()| ProviderError::internal(format!("{at} lists {name}")))
        })
        .collect()
}

/// One call of the feed of the Drive: its context and the tokens of the own
/// feeds of the items of its part.
struct DriveFeed<'a> {
    cx: &'a FeedCx<DriveKey>,
    inner: Vec<(Kind, Id, String)>,
}

impl Drive {
    pub(crate) async fn feed(cx: FeedCx<DriveKey>) -> Result<Feed> {
        let mut feed = DriveFeed {
            cx: &cx,
            inner: Vec::new(),
        };
        match cx.token().map(decode_token::<DriveToken>).transpose()? {
            None => feed.start(true).await,
            Some(DriveToken::Relisting) => feed.start(false).await,
            Some(DriveToken::Listing {
                first,
                changes,
                page,
            }) => feed.listing(first, changes, page).await,
            Some(DriveToken::Changes { page }) => feed.changes(page).await,
        }
    }
}

impl DriveFeed<'_> {
    /// The token of the changes after now, before the first request of a
    /// full listing, whose answers the changes after it complete. A listing
    /// other than the first begins with the removal of each item of the
    /// projection: the host keeps the items that the listing writes after
    /// it, with their content, so that the ones that it lacks leave, an item
    /// by its ID alone too, which its next lookup takes again.
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
                for id in listing(self.cx, kind)? {
                    self.remove(kind, &id)?;
                }
            }
        }
        self.part(
            &DriveToken::Listing {
                first,
                changes: start.start_page_token,
                page: None,
            },
            true,
        )
    }

    /// A request of a full listing: its items, then its next request, and
    /// after its last one the listing of each kind complete. A token of a
    /// page that Google Drive refuses with `400` begins the listing again,
    /// and a page for which it searched not every drive is the error of the
    /// request.
    async fn listing(
        &mut self,
        first: bool,
        changes: String,
        page: Option<String>,
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
                // The answers of a first listing are in the projection, and
                // the changes after its start complete them; the host drops
                // the answers of a later one with its removals.
                let again = if first {
                    DriveToken::Listing {
                        first,
                        changes,
                        page: None,
                    }
                } else {
                    DriveToken::Relisting
                };
                return Ok(Feed::restart(
                    encode(&again)?,
                    ProviderError::from_http_status(400),
                ));
            },
            status => return Err(ProviderError::from_http_status(status)),
        }
        let list: FileList = decode(response.body())?;
        if list.incomplete_search {
            return Err(ProviderError::invalid_input(
                "Google Drive searched not every drive for a full listing",
            ));
        }
        for object in &list.files {
            let Identified { id } = decode(object.get().as_bytes())?;
            if self.take(&id, object, ListingWrite::Partial)?.is_none() {
                self.leave(&id, None)?;
            }
        }
        if let Some(page) = list.next_page_token {
            return self.part(
                &DriveToken::Listing {
                    first,
                    changes,
                    page: Some(page),
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
    /// brings, and the removal of each that the account no longer has or
    /// that is in the trash; then its next request, and after its last one
    /// the token of the changes after it. A token that Google Drive refuses
    /// with `400` begins a full listing again.
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
                    "nextPageToken,newStartPageToken,changes(changeType,fileId,removed,file({FIELDS}))"
                ),
            )
            .send()
            .await?;
        match response.status() {
            200 => {},
            400 => {
                return Ok(Feed::restart(
                    encode(&DriveToken::Relisting)?,
                    ProviderError::from_http_status(400),
                ));
            },
            status => return Err(ProviderError::from_http_status(status)),
        }
        let list: ChangeList = decode(response.body())?;
        for change in &list.changes {
            self.change(change)?;
        }
        match (list.next_page_token, list.new_start_page_token) {
            (Some(page), _) => self.part(&DriveToken::Changes { page }, true),
            (None, Some(page)) => self.part(&DriveToken::Changes { page }, false),
            (None, None) => Err(ProviderError::invalid_input(
                "the last page of the changes of Google Drive has no newStartPageToken",
            )),
        }
    }

    /// Take a change of an item into the projection: its object under its
    /// kind, and its removal under each other kind; an item that the account
    /// no longer has, and one whose change has no object, leave each kind.
    fn change(&mut self, change: &Change) -> Result<()> {
        if change.of == ChangeType::Drive {
            return Ok(());
        }
        let id = change.file_id.as_ref().ok_or_else(|| {
            ProviderError::invalid_input("a change of an item of Google Drive has no fileId")
        })?;
        let kind = match &change.file {
            Some(object) if !change.removed => self.take(id, object, ListingWrite::Complete)?,
            _ => None,
        };
        self.leave(id, kind)
    }

    /// Take the item `id` of an answer into the projection from its object,
    /// its folder joining the listing of its kind as `listed` says, with the
    /// removal of the content of a file whose content changed. Its kind, none
    /// for an item in the trash and for one whose object the provider cannot
    /// read, which the projection takes not.
    fn take(&mut self, id: &Id, object: &RawValue, listed: ListingWrite) -> Result<Option<Kind>> {
        let Ok(item) = decode::<Item>(object.get().as_bytes()) else {
            return Ok(None);
        };
        if item.trashed {
            return Ok(None);
        }
        let kind = item.kind();
        let content = item.content();
        let revision = content.as_ref().map(Content::revision);
        if let Some(revision) = revision
            && projected_revision(self.cx, id)?.is_some_and(|projected| projected != revision)
        {
            self.cx.remove(content_path(id))?;
        }
        write_folder(self.cx, kind, id, listed)?;
        write_facts(self.cx, kind, id, &item.name, object)?;
        let token = encode(&ItemToken {
            listed: true,
            revision: revision.map(str::to_owned),
            object: None,
        })?;
        self.inner.push((kind, id.clone(), token));
        Ok(Some(kind))
    }

    /// The removal of the item `id` under each kind other than `kept`.
    fn leave(&mut self, id: &Id, kept: Option<Kind>) -> Result<()> {
        for kind in Kind::ALL {
            if Some(kind) != kept {
                self.remove(kind, id)?;
            }
        }
        Ok(())
    }

    fn remove(&mut self, kind: Kind, id: &Id) -> Result<()> {
        self.cx.remove(path(kind, id))?;
        self.inner.push((kind, id.clone(), REMOVED.to_owned()));
        Ok(())
    }

    /// A part with `token`, followed by `more` parts, with the tokens of the
    /// own feeds of its items.
    fn part(&mut self, token: &DriveToken, more: bool) -> Result<Feed> {
        let mut part = Feed::part(encode(token)?).more(more);
        for (kind, id, token) in self.inner.drain(..) {
            let key = ItemKey { id };
            part = match kind {
                Kind::File => part.inner::<File>(&key, token),
                Kind::Folder => part.inner::<Folder>(&key, token),
                Kind::Other => part.inner::<Other>(&key, token),
            };
        }
        Ok(part.into())
    }
}

/// The object of the item `id` by its ID as it came; none for `404`.
async fn by_id(cx: &FeedCx<ItemKey>, id: &Id) -> Result<Option<Box<RawValue>>> {
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
        status => Err(ProviderError::from_http_status(status)),
    }
}

fn removal(cx: &FeedCx<ItemKey>, kind: Kind, id: &Id) -> Result<Feed> {
    cx.remove(path(kind, id))?;
    Ok(Feed::part(REMOVED).into())
}

/// The own feed of an item of `kind`: the item by its ID, or its removal
/// for `404`, in the trash, or of another kind. The feed of the Drive brings
/// the facts of an item that it brought, which this feed writes for another
/// revision of a file alone. An item by its ID alone has its folder, which
/// joins the listing of its kind as complete as it was, in a part before the
/// part of its facts, which lack after [`BY_ID`].
///
/// The content of a file comes with its folder, at the revision of that
/// answer, for the start `content`, or for the due start `content` of a file
/// whose content changed; the content of another revision leaves.
async fn own(cx: FeedCx<ItemKey>, kind: Kind) -> Result<Feed> {
    let id = &cx.key().id;
    let mut token = cx
        .token()
        .map(decode_token::<Option<ItemToken>>)
        .transpose()?
        .flatten();
    let before = token.as_mut().and_then(|token| token.object.take());
    let facts = before.is_some();
    let object = match before {
        Some(object) => object,
        None => match by_id(&cx, id).await? {
            Some(object) => object,
            None => return removal(&cx, kind, id),
        },
    };
    let item: Item = decode(object.get().as_bytes())?;
    if item.trashed || item.kind() != kind {
        return removal(&cx, kind, id);
    }
    let content = item.content();
    let revision = content.as_ref().map(Content::revision);
    let listed = token.as_ref().is_some_and(|token| token.listed);
    let changed = token
        .as_ref()
        .is_some_and(|token| token.revision.as_deref() != revision);
    if let Some(content) = content.as_ref().filter(|_| !facts) {
        if cx.start() == Some("content") || changed && cx.due() == Some("content") {
            if !take_content(&cx, id, content).await? {
                return removal(&cx, kind, id);
            }
        } else if changed {
            cx.remove(content_path(id))?;
        }
    }
    let part = |object| -> Result<FeedPart> {
        Ok(Feed::part(encode(&ItemToken {
            listed,
            revision: revision.map(str::to_owned),
            object,
        })?))
    };
    if listed {
        if changed {
            write_facts(&cx, kind, id, &item.name, &object)?;
        }
        return Ok(part(None)?.into());
    }
    if !facts {
        write_folder(&cx, kind, id, ListingWrite::Join)?;
        return Ok(part(Some(object))?.more(true).into());
    }
    write_facts(&cx, kind, id, &item.name, &object)?;
    Ok(part(None)?.until(SystemTime::now() + BY_ID).into())
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

/// Take `content` as the file `content` of the file `id`: the bytes of an
/// uploaded file of that MD5, which the host fetches and holds, or the
/// export of a file of Google. Whether Google Drive has the file.
async fn take_content(cx: &FeedCx<ItemKey>, id: &Id, content: &Content<'_>) -> Result<bool> {
    let at = content_path(id);
    match content {
        Content::Bytes(md5) => {
            let bytes = || {
                cx.endpoint(DriveApi)
                    .get(format!("/drive/v3/files/{id}"))
                    .query("alt", "media")
                    .query("supportsAllDrives", "true")
            };
            match bytes().into_blob().md5(*md5).fetch().await {
                Ok(blob) => cx.blob(at, &blob)?,
                Err(error) if error.kind() == ProviderErrorKind::NotFound => return Ok(false),
                // The reason of a refusal is in the body of its answer, which
                // the host holds: the answer to the first byte has it too,
                // as a request over a quota. A file without a first byte has
                // `416` for it.
                Err(error) if error.kind() == ProviderErrorKind::Denied => {
                    let refusal = bytes().header("Range", "bytes=0-0").send().await?;
                    return match refusal.status() {
                        200..=299 | 416 => Err(error),
                        404 => Ok(false),
                        status => Err(ProviderError::from_http_status(status)),
                    };
                },
                Err(error) => return Err(error),
            }
        },
        Content::Export { mime_type, .. } => {
            let response = cx
                .endpoint(DriveApi)
                .get(format!("/drive/v3/files/{id}/export"))
                .query("mimeType", mime_type)
                .send()
                .await?;
            match response.status() {
                200 => cx.file(at, response.body())?,
                404 => return Ok(false),
                status => return Err(ProviderError::from_http_status(status)),
            }
        },
    }
    Ok(true)
}
