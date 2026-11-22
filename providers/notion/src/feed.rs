//! The feeds of the pages, data sources, and comments of the workspace.
//!
//! The feed of the workspace takes the results of the search of Notion, a
//! part for each request: a full listing at the first read of `pages/` or
//! `databases/` and at each read of one of them in a sync, by
//! `last_edited_time` from the latest, with the pages and the data sources of
//! the projection that it lacks by their IDs after its last page, and else
//! the changes, from the latest `last_edited_time` that the results of its
//! answers brought. A result with an earlier `last_edited_time` than its
//! object of the projection leaves the projection as it is. A page or a data
//! source that the request by its ID of a full listing answers with another
//! status than `200`, `401`, `404`, or `429` stays as the projection has it,
//! and the listing goes on: its own feed has Notion's error for the rest of
//! the sync, which an operation that lacks a fact of it has without a
//! request. The own feed of a page takes it by its ID, its content, and its
//! comments, and the own feed of a data source takes it by its ID, a part for
//! each request.
//!
//! Notion gives `last_edited_time` by the minute, so the content and the
//! comments of a page that Notion answered within the minute of its
//! `last_edited_time`, by the `Date` of their first answer, are unsettled:
//! the next sync takes them once more.

use std::collections::BTreeSet;

use omnifs_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::json;
use serde_json::value::RawValue;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc2822;

use crate::api::{
    CommentObject, DataSourceObject, Found, Kind, List, Markdown, NotionApi, PageObject, decode,
    restricted,
};
use crate::{DataSource, DataSourceKey, Id, Page, PageKey, Workspace, WorkspaceKey};

/// The most results of a request of the search, and the comments of a
/// request of the comments of a page, that Notion answers.
const PAGE: usize = 100;

// ===========================================================================
// Tokens
// ===========================================================================

/// The token of the feed of the workspace: the latest `last_edited_time` of
/// the results of its answers and the count of its results with it, and the
/// stage of an unfinished answer.
#[derive(Serialize, Deserialize)]
struct WorkspaceToken {
    latest: Option<Latest>,
    stage: Option<Stage>,
}

/// The latest `last_edited_time` of results, and the count of them with it.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Latest {
    #[serde(with = "time::serde::rfc3339")]
    time: OffsetDateTime,
    count: usize,
}

impl Latest {
    /// The latest time and count of `latest` with a result at `time`.
    fn with(latest: Option<Self>, time: OffsetDateTime) -> Self {
        match latest {
            Some(latest) if latest.time > time => latest,
            Some(latest) if latest.time == time => Self {
                time,
                count: latest.count + 1,
            },
            _ => Self { time, count: 1 },
        }
    }
}

/// The stage of an unfinished answer of the workspace, with the latest time
/// and count of the results of the answer so far.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Stage {
    /// A full listing at its next cursor, none for its first request, with
    /// its sweep.
    Listing {
        sweep: Sweep,
        cursor: Option<String>,
        seen: Option<Latest>,
    },
    /// The sweep of a full listing after its last page of results.
    Missing { sweep: Sweep, seen: Option<Latest> },
    /// The changes at their next cursor.
    Changes {
        cursor: String,
        seen: Option<Latest>,
    },
}

/// What a full listing takes by their IDs after its last page of results.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Sweep {
    /// The first full listing, a first answer whose parts the projection
    /// takes as they come: the pages and the data sources that the
    /// projection had before it, by lookups by their IDs, which its results
    /// have not brought.
    First(Lacking),
    /// A later full listing: the pages and the data sources that the
    /// projection lists and that the kept parts of the answer neither wrote
    /// nor removed, after the last one that it asked for.
    Later(Option<(Kind, Id)>),
}

/// The IDs of pages and data sources.
#[derive(Serialize, Deserialize)]
struct Lacking {
    pages: BTreeSet<Id>,
    databases: BTreeSet<Id>,
}

impl Lacking {
    /// The first of them, a page before a data source, which leaves them.
    fn pop(&mut self) -> Option<(Kind, Id)> {
        let page = self.pages.pop_first().map(|id| (Kind::Page, id));
        page.or_else(|| self.databases.pop_first().map(|id| (Kind::DataSource, id)))
    }

    /// Leave out `id` of `kind`, which a result brought.
    fn found(&mut self, kind: Kind, id: &Id) {
        match kind {
            Kind::Page => self.pages.remove(id),
            Kind::DataSource => self.databases.remove(id),
        };
    }
}

/// The token of the feed of a page: the `last_edited_time` of the page, and
/// its unfinished content and its unfinished comments, each of which the read
/// that asks for it continues. The token of a removed page is `null`.
#[derive(Serialize, Deserialize)]
struct PageToken {
    #[serde(with = "time::serde::rfc3339")]
    last_edited_time: OffsetDateTime,
    content: Option<Content>,
    comments: Option<Comments>,
}

impl PageToken {
    /// The token of a page with `last_edited_time` whose content and
    /// comments no read asked for.
    fn new(last_edited_time: OffsetDateTime) -> Self {
        Self {
            last_edited_time,
            content: None,
            comments: None,
        }
    }
}

/// The unfinished content of a page: the IDs of its requests in their order,
/// the page and then each unknown block of each answer once, the count of
/// them that it asked, the markdown of their answers, and whether it is
/// unsettled.
#[derive(Serialize, Deserialize)]
struct Content {
    ids: Vec<Id>,
    asked: usize,
    markdown: String,
    unsettled: bool,
}

impl Content {
    /// The content of the page `id` before its first request.
    fn of(id: &Id) -> Self {
        Self {
            ids: vec![id.clone()],
            asked: 0,
            markdown: String::new(),
            unsettled: false,
        }
    }
}

/// The unfinished comments of a page: their next cursor, none for their
/// first request, their objects so far, and whether they are unsettled.
#[derive(Serialize, Deserialize)]
struct Comments {
    cursor: Option<String>,
    objects: Vec<Box<RawValue>>,
    unsettled: bool,
}

impl Comments {
    /// The comments of a page before their first request.
    fn first() -> Self {
        Self {
            cursor: None,
            objects: Vec::new(),
            unsettled: false,
        }
    }
}

// ===========================================================================
// Facts
// ===========================================================================

const PAGES: &str = "/pages";
const DATABASES: &str = "/databases";

fn page_path(id: &Id) -> String {
    format!("{PAGES}/{id}")
}

fn data_source_path(id: &Id) -> String {
    format!("{DATABASES}/{id}")
}

/// Write the facts of a page from its object: its folder in `pages/`, which
/// joins it as `write` says, `title`, and `page.json`.
fn write_page<K>(cx: &FeedCx<K>, id: &Id, object: &RawValue, write: ListingWrite) -> Result<()> {
    let page: PageObject = decode(object.get().as_bytes())?;
    let at = page_path(id);
    cx.dir(PAGES, write)?;
    cx.dir(&at, ListingWrite::Partial)?;
    cx.file(format!("{at}/title"), page.title()?.into_bytes())?;
    cx.file(format!("{at}/page.json"), object.get().as_bytes())?;
    Ok(())
}

/// Write the facts of a data source from its object: its folder in
/// `databases/`, which joins it as `write` says, `title`, and `schema.json`.
fn write_data_source<K>(
    cx: &FeedCx<K>,
    id: &Id,
    object: &RawValue,
    write: ListingWrite,
) -> Result<()> {
    let data_source: DataSourceObject = decode(object.get().as_bytes())?;
    let at = data_source_path(id);
    cx.dir(DATABASES, write)?;
    cx.dir(&at, ListingWrite::Partial)?;
    cx.file(format!("{at}/title"), data_source.title().into_bytes())?;
    cx.file(format!("{at}/schema.json"), object.get().as_bytes())?;
    Ok(())
}

/// Remove `body.md` and `comments/` of a page, whose content and comments a
/// change of the page may have changed.
fn remove_content<K>(cx: &FeedCx<K>, id: &Id) -> Result<()> {
    let at = page_path(id);
    cx.remove(format!("{at}/body.md"))?;
    cx.remove(format!("{at}/comments"))
}

/// The IDs of the entries of the listing of `path` in the projection, none
/// without a listing.
fn listed<K>(cx: &FeedCx<K>, path: &str) -> Result<BTreeSet<Id>> {
    Ok(cx
        .names(path)?
        .map_or_else(BTreeSet::new, |listed| listed.names.into_iter().collect()))
}

/// The `last_edited_time` of the object of the projection whose JSON is the
/// fed file at `path`, none for a path without one. The file is read whole
/// at the length that the projection gives it.
fn projected_time<K>(cx: &FeedCx<K>, path: &str) -> Result<Option<OffsetDateTime>> {
    let mut length = 0;
    loop {
        let Some(range) = cx.read_range(path, 0, length)? else {
            return Ok(None);
        };
        if range.bytes.len() as u64 == range.length {
            return Ok(Some(decode::<Found>(&range.bytes)?.last_edited_time));
        }
        length = u32::try_from(range.length)
            .map_err(|_| ProviderError::too_large(format!("{path} has {} bytes", range.length)))?;
    }
}

/// Check that the object `found` of an answer to a request of the object
/// `id` is that object.
fn answered(found: &Found, id: &Id) -> Result<()> {
    if Id::of(&found.id)? == *id {
        return Ok(());
    }
    Err(ProviderError::invalid_input(format!(
        "Notion answered the request of {id} with {}",
        found.id
    )))
}

/// A request of Notion with a `start_cursor` whose answer has a status from
/// `400` to `499` other than `429`: Notion no longer takes the cursor.
fn cursor_refused(cursor: Option<&str>, response: &HttpResponse) -> bool {
    cursor.is_some() && matches!(response.status(), 400..=499) && response.status() != 429
}

/// Whether Notion answered `response` within the minute of
/// `last_edited_time`, by its `Date`: Notion gives `last_edited_time` by the
/// minute, so a change in the rest of that minute keeps it.
fn within_minute(last_edited_time: OffsetDateTime, response: &HttpResponse) -> Result<bool> {
    let date = response
        .header("date")
        .ok_or_else(|| ProviderError::invalid_input("an answer of Notion has no Date"))?;
    let date = OffsetDateTime::parse(date, &Rfc2822).map_err(|error| {
        ProviderError::invalid_input(format!("the Date {date} of Notion: {error}"))
    })?;
    Ok(minute(date)? <= minute(last_edited_time)?)
}

/// The minute of `time`.
fn minute(time: OffsetDateTime) -> Result<OffsetDateTime> {
    time.replace_second(0)
        .and_then(|time| time.replace_nanosecond(0))
        .map_err(|error| ProviderError::internal(error.to_string()))
}

// ===========================================================================
// The feed of the workspace
// ===========================================================================

/// The state that a part gives the feed of a page, with its token, or of a
/// data source, with the error of Notion for one that it refused.
enum Inner {
    Page(Id, Option<PageToken>, Option<ProviderError>),
    DataSource(Id, Option<ProviderError>),
}

/// One call of the feed of the workspace: its context, the latest time of
/// its token, the inner states of its part, and whether its part declines
/// the kept parts of the answer that it continues.
struct WorkspaceFeed<'a> {
    cx: &'a FeedCx<WorkspaceKey>,
    latest: Option<Latest>,
    inner: Vec<Inner>,
    anew: bool,
}

impl Workspace {
    /// The feed of the pages and the data sources of the workspace: the next
    /// request of the stage of its token, but for unfinished changes at a
    /// read of `pages/` or `databases/`, its starting paths, which a full
    /// listing answers anew; else the first request of a full listing for a
    /// read of `pages/` or `databases/`, and of the changes for another read.
    pub(crate) async fn feed(cx: FeedCx<WorkspaceKey>) -> Result<Feed> {
        let token = cx.token::<WorkspaceToken>()?;
        let first = token.is_none();
        let (latest, stage) = token.map_or((None, None), |token| (token.latest, token.stage));
        let listing = cx.start().or(cx.due()).is_some();
        let mut feed = WorkspaceFeed {
            cx: &cx,
            latest,
            inner: Vec::new(),
            anew: false,
        };
        match stage {
            Some(Stage::Listing {
                sweep,
                cursor,
                seen,
            }) => feed.listing(sweep, cursor, seen).await,
            Some(Stage::Missing { sweep, seen }) => feed.missing(sweep, seen).await,
            Some(Stage::Changes { .. }) if listing => {
                feed.anew = true;
                feed.listing(Sweep::Later(None), None, None).await
            },
            Some(Stage::Changes { cursor, seen }) => feed.changes(Some(cursor), seen).await,
            None if listing => {
                let sweep = if first {
                    Sweep::First(feed.projected()?)
                } else {
                    Sweep::Later(None)
                };
                feed.listing(sweep, None, None).await
            },
            None => feed.changes(None, None).await,
        }
    }
}

/// The answer of a request of the search: its list, or the refusal of its
/// cursor.
enum Search {
    List(List),
    Refused(ProviderError),
}

impl WorkspaceFeed<'_> {
    /// A request of the search: the results with `page_size`, from
    /// `cursor`, by `last_edited_time` from the latest.
    async fn search(&self, cursor: Option<&str>, page_size: usize) -> Result<Search> {
        let mut body = json!({
            "sort": { "direction": "descending", "timestamp": "last_edited_time" },
            "page_size": page_size,
        });
        if let Some(cursor) = cursor {
            body["start_cursor"] = cursor.into();
        }
        let response = self
            .cx
            .endpoint(NotionApi)
            .post("/v1/search")
            .body_json(&body)
            .send()
            .await?;
        match response.error_for_status_ref() {
            Ok(()) => Ok(Search::List(decode(response.body())?)),
            Err(error) if cursor_refused(cursor, &response) => Ok(Search::Refused(error)),
            Err(error) => Err(error),
        }
    }

    /// The restart of an answer whose cursor Notion refused, with `stage` in
    /// the token of the feed: the first full listing at its first request,
    /// and none after it, so that the next read decides between a full
    /// listing and the changes.
    fn restart(&self, stage: Option<Stage>, error: ProviderError) -> Result<Feed> {
        Feed::restart(
            &WorkspaceToken {
                latest: self.latest,
                stage,
            },
            error,
        )
    }

    /// The pages and the data sources of the projection, which the first
    /// full listing lacks before its first request.
    fn projected(&self) -> Result<Lacking> {
        Ok(Lacking {
            pages: listed(self.cx, PAGES)?,
            databases: listed(self.cx, DATABASES)?,
        })
    }

    /// The first page or data source after `after` that the projection
    /// lists and that the kept parts of the answer neither wrote nor
    /// removed, a page before a data source.
    fn lacking_after(&self, after: Option<&(Kind, Id)>) -> Result<Option<(Kind, Id)>> {
        let mut lacking = Vec::new();
        for (kind, path) in [(Kind::Page, PAGES), (Kind::DataSource, DATABASES)] {
            let kept = self.cx.kept::<Id>(path)?;
            lacking.extend(
                listed(self.cx, path)?
                    .into_iter()
                    .filter(|id| !kept.written.contains(id) && !kept.removed.contains(id))
                    .map(|id| (kind, id)),
            );
        }
        Ok(lacking
            .into_iter()
            .find(|lacking| after.is_none_or(|after| lacking > after)))
    }

    /// Take the result `found` of the search, or of a request by its ID,
    /// with its `object`, into the projection, its folder joining its listing
    /// as `write` says: nothing of one with an earlier `last_edited_time`
    /// than its object of the projection; the removal of one in the trash;
    /// and the facts of another, with the state of its feed for a data source
    /// and for a page that the projection lacks or has with another
    /// `last_edited_time`, whose content and comments it removes.
    fn take(&mut self, found: &Found, object: &RawValue, write: ListingWrite) -> Result<Id> {
        let id = Id::of(&found.id)?;
        let projected = projected_time(
            self.cx,
            &match found.object {
                Kind::Page => format!("{}/page.json", page_path(&id)),
                Kind::DataSource => format!("{}/schema.json", data_source_path(&id)),
            },
        )?;
        if projected.is_some_and(|projected| projected > found.last_edited_time) {
            return Ok(id);
        }
        if found.in_trash {
            self.remove(found.object, id.clone())?;
            return Ok(id);
        }
        match found.object {
            Kind::Page => {
                if projected != Some(found.last_edited_time) {
                    if projected.is_some() {
                        remove_content(self.cx, &id)?;
                    }
                    let token = PageToken::new(found.last_edited_time);
                    self.inner.push(Inner::Page(id.clone(), Some(token), None));
                }
                write_page(self.cx, &id, object, write)?;
            },
            Kind::DataSource => {
                write_data_source(self.cx, &id, object, write)?;
                self.inner.push(Inner::DataSource(id.clone(), None));
            },
        }
        Ok(id)
    }

    /// Remove the page or the data source `id` from the projection, with
    /// the state of a removed one for its feed.
    fn remove(&mut self, kind: Kind, id: Id) -> Result<()> {
        match kind {
            Kind::Page => {
                self.cx.remove(page_path(&id))?;
                self.inner.push(Inner::Page(id, None, None));
            },
            Kind::DataSource => {
                self.cx.remove(data_source_path(&id))?;
                self.inner.push(Inner::DataSource(id, None));
            },
        }
        Ok(())
    }

    /// A request of a full listing: its results, which the sweep of the
    /// first full listing leaves, then its next request, or after its last
    /// one its sweep.
    async fn listing(
        &mut self,
        mut sweep: Sweep,
        cursor: Option<String>,
        mut seen: Option<Latest>,
    ) -> Result<Feed> {
        let list = match self.search(cursor.as_deref(), PAGE).await? {
            Search::List(list) => list,
            Search::Refused(error) => {
                let stage = match sweep {
                    Sweep::First(_) => Some(Stage::Listing {
                        sweep: Sweep::First(self.projected()?),
                        cursor: None,
                        seen: None,
                    }),
                    Sweep::Later(_) => None,
                };
                return self.restart(stage, error);
            },
        };
        let write = listing_write(&sweep);
        for result in &list.results {
            let found: Found = decode(result.get().as_bytes())?;
            let id = self.take(&found, result, write)?;
            if let Sweep::First(lacking) = &mut sweep {
                lacking.found(found.object, &id);
            }
            seen = Some(Latest::with(seen, found.last_edited_time));
        }
        self.part(Some(match list.next()? {
            Some(cursor) => Stage::Listing {
                sweep,
                cursor: Some(cursor),
                seen,
            },
            None => Stage::Missing { sweep, seen },
        }))
    }

    /// A request by its ID of the next page or data source of the sweep of a
    /// full listing: its facts, its removal for `404` or in the trash, or for
    /// another status than `401` and `429`, its facts as the projection has
    /// them with the error of Notion for its own feed; the last part of the
    /// listing after the sweep. An answer of another object is
    /// the error of the request.
    async fn missing(&mut self, mut sweep: Sweep, seen: Option<Latest>) -> Result<Feed> {
        let next = match &mut sweep {
            Sweep::First(lacking) => lacking.pop(),
            Sweep::Later(after) => self.lacking_after(after.as_ref())?,
        };
        let Some((kind, id)) = next else {
            return self.end(matches!(sweep, Sweep::First(_)), seen);
        };
        let request = match kind {
            Kind::Page => format!("/v1/pages/{id}"),
            Kind::DataSource => format!("/v1/data_sources/{id}"),
        };
        let response = self.cx.endpoint(NotionApi).get(request).send().await?;
        match (response.status(), response.error_for_status_ref()) {
            (_, Ok(())) => {
                let object: Box<RawValue> = decode(response.body())?;
                let found: Found = decode(object.get().as_bytes())?;
                answered(&found, &id)?;
                self.take(&found, &object, listing_write(&sweep))?;
            },
            (404, Err(_)) => self.remove(kind, id.clone())?,
            (401 | 429, Err(error)) => return Err(error),
            (_, Err(error)) => self.refused(kind, id.clone(), error)?,
        }
        if let Sweep::Later(after) = &mut sweep {
            *after = Some((kind, id));
        }
        self.part(Some(Stage::Missing { sweep, seen }))
    }

    /// Leave the page or the data source `id` that Notion refused with
    /// `error` as the projection has it, with the token of what the
    /// projection has and `error` for its own feed for the rest of the sync.
    fn refused(&mut self, kind: Kind, id: Id, error: ProviderError) -> Result<()> {
        match kind {
            Kind::Page => {
                let token = projected_time(self.cx, &format!("{}/page.json", page_path(&id)))?
                    .map(PageToken::new);
                self.inner.push(Inner::Page(id, token, Some(error)));
            },
            Kind::DataSource => self.inner.push(Inner::DataSource(id, Some(error))),
        }
        Ok(())
    }

    /// A request of the changes: the first with the `page_size` of the count
    /// of the latest time and one more, at most `100`, each next one with
    /// `100`, until a result before the latest time, which they leave.
    async fn changes(&mut self, cursor: Option<String>, mut seen: Option<Latest>) -> Result<Feed> {
        let page_size = match cursor {
            Some(_) => PAGE,
            None => self.latest.map_or(1, |latest| latest.count + 1).min(PAGE),
        };
        let list = match self.search(cursor.as_deref(), page_size).await? {
            Search::List(list) => list,
            Search::Refused(error) => return self.restart(None, error),
        };
        let mut older = false;
        for result in &list.results {
            let found: Found = decode(result.get().as_bytes())?;
            if self
                .latest
                .is_some_and(|latest| found.last_edited_time < latest.time)
            {
                older = true;
                break;
            }
            self.take(&found, result, ListingWrite::Complete)?;
            seen = Some(Latest::with(seen, found.last_edited_time));
        }
        match list.next()? {
            Some(cursor) if !older => self.part(Some(Stage::Changes { cursor, seen })),
            _ => self.end(false, seen),
        }
    }

    /// The last part of an answer: the latest time and its count of the
    /// results of the answer, as they were without results, and for the
    /// first full listing `pages/` and `databases/` complete.
    fn end(&mut self, first: bool, seen: Option<Latest>) -> Result<Feed> {
        if first {
            self.cx.dir(PAGES, ListingWrite::Complete)?;
            self.cx.dir(DATABASES, ListingWrite::Complete)?;
        }
        if seen.is_some() {
            self.latest = seen;
        }
        self.part(None)
    }

    /// A part with the latest time of the token and `stage`, followed by more
    /// parts while a stage remains, with the inner states of its results,
    /// which declines the kept parts of the answer that it continues for a
    /// full listing in place of the changes.
    fn part(&mut self, stage: Option<Stage>) -> Result<Feed> {
        let more = stage.is_some();
        let mut part = Feed::part(&WorkspaceToken {
            latest: self.latest,
            stage,
        })?
        .more(more);
        if self.anew {
            part = part.anew();
        }
        for inner in self.inner.drain(..) {
            part = match inner {
                Inner::Page(id, token, None) => part.inner::<Page>(&PageKey { id }, &token)?,
                Inner::Page(id, token, Some(error)) => {
                    part.inner_failed::<Page>(&PageKey { id }, &token, error)?
                },
                Inner::DataSource(id, None) => {
                    part.inner::<DataSource>(&DataSourceKey { id }, &())?
                },
                Inner::DataSource(id, Some(error)) => {
                    part.inner_failed::<DataSource>(&DataSourceKey { id }, &(), error)?
                },
            };
        }
        Ok(part.into())
    }
}

/// How the results of a full listing with `sweep` join `pages/` and
/// `databases/`: as they are before the last part of the first full listing,
/// complete after it.
fn listing_write(sweep: &Sweep) -> ListingWrite {
    match sweep {
        Sweep::First(_) => ListingWrite::Partial,
        Sweep::Later(_) => ListingWrite::Complete,
    }
}

// ===========================================================================
// The feed of a page
// ===========================================================================

/// One call of the feed of a page: its context and its ID.
struct PageFeed<'a> {
    cx: &'a FeedCx<PageKey>,
    id: &'a Id,
}

/// What a starting path of a page asks for beside the page: its content, its
/// comments, or one comment.
#[derive(Clone, Copy)]
enum Asked {
    Content,
    Comments,
    Comment,
}

impl Asked {
    /// What the starting path `start` asks for: the content for `body.md`,
    /// the comments for `comments`, and a comment for a path in `comments`.
    fn of(start: &str) -> Option<Self> {
        match start {
            "body.md" => Some(Self::Content),
            "comments" => Some(Self::Comments),
            start if start.starts_with("comments/") => Some(Self::Comment),
            _ => None,
        }
    }
}

impl Page {
    /// The feed of a page. A page without a token takes the page by its ID,
    /// and after it, in the same answer, what its start asks for. A read
    /// without its fact takes what its start asks for, from the next request
    /// of it in the token. A read in a sync takes the page by its ID, and
    /// after it, in the same answer, what its path asks for of a page
    /// with another `last_edited_time`. An answer that continues without its
    /// start takes the next request of the token, of the content before the
    /// comments. Each answer makes a part of each request.
    pub(crate) async fn feed(cx: FeedCx<PageKey>) -> Result<Feed> {
        let feed = PageFeed {
            cx: &cx,
            id: &cx.key().id,
        };
        let token = cx.token::<Option<PageToken>>()?.flatten();
        let asked = cx.start().or(cx.due()).and_then(Asked::of);
        let Some(token) = token else {
            return feed.page(None, asked).await;
        };
        let request = match (cx.start(), cx.due()) {
            (Some(_), _) => asked,
            (None, Some(_)) => asked.filter(|asked| match asked {
                Asked::Content => token.content.is_some(),
                Asked::Comments | Asked::Comment => token.comments.is_some(),
            }),
            (None, None) if token.content.is_some() => Some(Asked::Content),
            (None, None) => token.comments.as_ref().map(|_| Asked::Comments),
        };
        match request {
            Some(Asked::Content) => feed.content(token).await,
            Some(Asked::Comment) if token.comments.is_none() && feed.has_comments()? => {
                page_part(&token, false, false)
            },
            Some(Asked::Comments | Asked::Comment) => feed.comments(token).await,
            None => feed.page(Some(token), asked).await,
        }
    }
}

impl PageFeed<'_> {
    fn path(&self) -> String {
        page_path(self.id)
    }

    /// The page by its ID: its facts, its folder joining `pages/`, with the
    /// removal of its content and its comments for another `last_edited_time`
    /// than that of `token`, or its removal for `404` or in the trash; then,
    /// in the same answer, what `asked` names, for a page without a token or
    /// with another `last_edited_time`. An answer of another page is the
    /// error of the request.
    async fn page(&self, token: Option<PageToken>, asked: Option<Asked>) -> Result<Feed> {
        let response = self
            .cx
            .endpoint(NotionApi)
            .get(format!("/v1/pages/{}", self.id))
            .send()
            .await?;
        if response.status() == 404 {
            return self.removal();
        }
        response.error_for_status_ref()?;
        let object: Box<RawValue> = decode(response.body())?;
        let found: Found = decode(object.get().as_bytes())?;
        answered(&found, self.id)?;
        if found.in_trash {
            return self.removal();
        }
        write_page(self.cx, self.id, &object, ListingWrite::Join)?;
        let mut token = match token {
            Some(token) if token.last_edited_time == found.last_edited_time => {
                return page_part(&token, false, false);
            },
            Some(_) => {
                remove_content(self.cx, self.id)?;
                PageToken::new(found.last_edited_time)
            },
            None => PageToken::new(found.last_edited_time),
        };
        match asked {
            Some(Asked::Content) => token.content = Some(Content::of(self.id)),
            Some(Asked::Comments | Asked::Comment) => token.comments = Some(Comments::first()),
            None => {},
        }
        page_part(&token, asked.is_some(), false)
    }

    /// A request of the content: the markdown of the page, then that of each
    /// unknown block of each answer once, in turn, a block with `404` without
    /// its markdown, and at its last part `body.md`, the markdown of the
    /// page, then that of each block in their order, each after an empty
    /// line. A page with `404` is removed. The content is unsettled when
    /// Notion answered the markdown of the page within the minute of its
    /// `last_edited_time`.
    async fn content(&self, mut token: PageToken) -> Result<Feed> {
        let mut content = token.content.take().unwrap_or_else(|| Content::of(self.id));
        if let Some(id) = content.ids.get(content.asked) {
            let response = self
                .cx
                .endpoint(NotionApi)
                .get(format!("/v1/pages/{id}/markdown"))
                .query("markdown_version", "v2")
                .send()
                .await?;
            let page = content.asked == 0;
            match (response.status(), response.error_for_status_ref()) {
                (_, Ok(())) => {
                    let answer: Markdown = decode(response.body())?;
                    if page {
                        content.unsettled = within_minute(token.last_edited_time, &response)?;
                    } else {
                        content.markdown.push_str("\n\n");
                    }
                    content.markdown.push_str(&answer.markdown);
                    for id in &answer.unknown_block_ids {
                        let id = Id::of(id)?;
                        if !content.ids.contains(&id) {
                            content.ids.push(id);
                        }
                    }
                },
                (404, Err(_)) if page => return self.removal(),
                (404, Err(_)) => {},
                (_, Err(error)) => return Err(error),
            }
            content.asked += 1;
        }
        if content.asked < content.ids.len() {
            token.content = Some(content);
            return page_part(&token, true, false);
        }
        self.cx.file(
            format!("{}/body.md", self.path()),
            content.markdown.into_bytes(),
        )?;
        page_part(&token, false, content.unsettled)
    }

    /// Whether the comments of the page stand: `comments/` complete and
    /// settled in the projection.
    fn has_comments(&self) -> Result<bool> {
        Ok(self
            .cx
            .count(format!("{}/comments", self.path()))?
            .is_some_and(|comments| comments.complete && !comments.unsettled))
    }

    /// A request of the comments, by the cursor of the previous one, and at
    /// their last part each comment, `comments/` complete, and the removal of
    /// each comment of the projection that their answers lack. The comments
    /// that Notion restricts for the connection are no comments. A cursor
    /// that Notion refuses restarts the answer, which drops its kept parts,
    /// with the token of the page that the projection has, and a page with
    /// `404` to its first request is removed. The comments are unsettled when
    /// Notion answered their first request within the minute of the
    /// `last_edited_time` of the page.
    async fn comments(&self, mut token: PageToken) -> Result<Feed> {
        let Comments {
            cursor,
            mut objects,
            mut unsettled,
        } = token.comments.take().unwrap_or_else(Comments::first);
        let mut request = self
            .cx
            .endpoint(NotionApi)
            .get("/v1/comments")
            .query("block_id", self.id)
            .query("page_size", PAGE);
        if let Some(cursor) = &cursor {
            request = request.query("start_cursor", cursor);
        }
        let response = request.send().await?;
        let none = match response.error_for_status_ref() {
            Ok(()) => false,
            Err(error) if cursor_refused(cursor.as_deref(), &response) => {
                return Feed::restart(&self.projected(token)?, error);
            },
            Err(_) if response.status() == 404 => return self.removal(),
            Err(_) if restricted(&response) => true,
            Err(error) => return Err(error),
        };
        if cursor.is_none() {
            unsettled = within_minute(token.last_edited_time, &response)?;
        }
        if !none {
            let list: List = decode(response.body())?;
            let next = list.next()?;
            objects.extend(list.results);
            if let Some(cursor) = next {
                token.comments = Some(Comments {
                    cursor: Some(cursor),
                    objects,
                    unsettled,
                });
                return page_part(&token, true, false);
            }
        }
        let folder = format!("{}/comments", self.path());
        let mut written = BTreeSet::new();
        for object in &objects {
            let comment: CommentObject = decode(object.get().as_bytes())?;
            let id = Id::of(&comment.id)?;
            let at = format!("{folder}/{id}");
            self.cx.dir(&at, ListingWrite::Partial)?;
            self.cx
                .file(format!("{at}/author"), comment.author().as_bytes())?;
            self.cx
                .file(format!("{at}/body.md"), comment.body().into_bytes())?;
            self.cx
                .file(format!("{at}/comment.json"), object.get().as_bytes())?;
            written.insert(id);
        }
        self.cx.dir(&folder, ListingWrite::Complete)?;
        for id in listed(self.cx, &folder)?.difference(&written) {
            self.cx.remove(format!("{folder}/{id}"))?;
        }
        page_part(&token, false, unsettled)
    }

    /// The token of the page that the projection has, without the kept
    /// parts of an answer: its `last_edited_time` by its `page.json`, with
    /// the unfinished content of `token` taken at that time; `null` for a
    /// page that the projection lacks.
    fn projected(&self, token: PageToken) -> Result<Option<PageToken>> {
        let projected = projected_time(self.cx, &format!("{}/page.json", self.path()))?;
        Ok(projected.map(|last_edited_time| PageToken {
            content: token
                .content
                .filter(|_| last_edited_time == token.last_edited_time),
            ..PageToken::new(last_edited_time)
        }))
    }

    /// The removal of the page, with the token of a removed page.
    fn removal(&self) -> Result<Feed> {
        self.cx.remove(self.path())?;
        Ok(Feed::part(&None::<PageToken>)?.into())
    }
}

// ===========================================================================
// The feed of a data source
// ===========================================================================

impl DataSource {
    /// The feed of a data source, whose token is `null`: the data source by
    /// its ID, its facts with its folder joining `databases/`, or its removal
    /// for `404` or in the trash. An answer of another data source is the
    /// error of the request.
    pub(crate) async fn feed(cx: FeedCx<DataSourceKey>) -> Result<Feed> {
        let id = &cx.key().id;
        let response = cx
            .endpoint(NotionApi)
            .get(format!("/v1/data_sources/{id}"))
            .send()
            .await?;
        if response.status() == 404 {
            return data_source_removal(&cx, id);
        }
        response.error_for_status_ref()?;
        let object: Box<RawValue> = decode(response.body())?;
        let found: Found = decode(object.get().as_bytes())?;
        answered(&found, id)?;
        if found.in_trash {
            return data_source_removal(&cx, id);
        }
        write_data_source(&cx, id, &object, ListingWrite::Join)?;
        Ok(Feed::part(&())?.into())
    }
}

/// A part of the feed of a page with `token`, followed by `more` parts, its
/// facts unsettled for `unsettled`.
fn page_part(token: &PageToken, more: bool, unsettled: bool) -> Result<Feed> {
    let part = Feed::part(&Some(token))?.more(more);
    Ok(if unsettled { part.unsettled() } else { part }.into())
}

/// The removal of a data source.
fn data_source_removal(cx: &FeedCx<DataSourceKey>, id: &Id) -> Result<Feed> {
    cx.remove(data_source_path(id))?;
    Ok(Feed::part(&())?.into())
}
