//! The feeds of the pages, data sources, and comments of the workspace.
//!
//! The feed of the workspace takes the results of the search of Notion, a
//! part for each request: a full listing at the first read of `pages/` or
//! `databases/` and at each read of one of them past the deadline of the
//! feed, by `last_edited_time` from the latest, with the pages and the data
//! sources of the projection that it lacks by their IDs after its last page,
//! and else the changes, from the latest `last_edited_time` that the results
//! of its answers brought. The own feed of a page takes it by its ID, its
//! content, and its comments, and the own feed of a data source takes it by
//! its ID, a part for each request.
//!
//! Notion gives `last_edited_time` by the minute, so the content and the
//! comments that a page answers within the minute of its `last_edited_time`,
//! by the `Date` of their first answer, count as lacking after the end of that
//! minute, and the next read takes them once more.

use std::collections::{BTreeSet, VecDeque};
use std::time::SystemTime;

use omnifs_sdk::prelude::*;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;
use serde_json::value::RawValue;
use time::format_description::well_known::Rfc2822;
use time::{Duration, OffsetDateTime};

use crate::api::{
    CommentObject, DataSourceObject, Found, List, Markdown, NotionApi, PageObject, decode,
    status_error,
};
use crate::{DataSource, DataSourceKey, Id, Page, PageKey, Workspace, WorkspaceKey};

/// The results of a request of a full listing or of the changes, and the
/// comments of a request of the comments of a page.
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
    /// the IDs of its results; `first` for the first full listing.
    Listing {
        first: bool,
        cursor: Option<String>,
        pages: BTreeSet<Id>,
        databases: BTreeSet<Id>,
        seen: Option<Latest>,
    },
    /// The IDs of the pages and the data sources of the projection that a
    /// full listing lacks.
    Missing {
        first: bool,
        pages: BTreeSet<Id>,
        databases: BTreeSet<Id>,
        seen: Option<Latest>,
    },
    /// The changes at their next cursor, none for their first request.
    Changes {
        cursor: Option<String>,
        seen: Option<Latest>,
    },
}

/// The token of the feed of a page: the `last_edited_time` of the page and
/// the next request of an unfinished answer. The token of a removed page is
/// `null`.
#[derive(Serialize, Deserialize)]
struct PageToken {
    #[serde(with = "time::serde::rfc3339")]
    last_edited_time: OffsetDateTime,
    next: Option<PageRequest>,
}

/// The next request of an unfinished answer of a page, with the time of the
/// computer at the end of the minute of the `last_edited_time` of the page
/// for an answer within it.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PageRequest {
    /// The content: its markdown so far, none before its first answer, and
    /// its queue of unknown block IDs.
    Content {
        markdown: Option<String>,
        queue: VecDeque<String>,
        #[serde(with = "time::serde::rfc3339::option")]
        until: Option<OffsetDateTime>,
    },
    /// The comments at their next cursor, none for their first request, with
    /// their objects so far.
    Comments {
        cursor: Option<String>,
        comments: Vec<Box<RawValue>>,
        #[serde(with = "time::serde::rfc3339::option")]
        until: Option<OffsetDateTime>,
    },
}

/// The token of the feed of a data source: its `last_edited_time`. The token
/// of a removed data source is `null`.
#[derive(Serialize, Deserialize)]
struct DataSourceToken {
    #[serde(with = "time::serde::rfc3339")]
    last_edited_time: OffsetDateTime,
}

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

/// A request of Notion with a `start_cursor` whose answer has a status from
/// `400` to `499` other than `429`: Notion no longer takes the cursor.
fn cursor_refused(cursor: Option<&String>, response: &HttpResponse) -> bool {
    cursor.is_some() && matches!(response.status(), 400..=499) && response.status() != 429
}

/// The time of the computer at the end of the minute of `last_edited_time`
/// by the `Date` of `response`, for a `Date` within that minute, as Notion
/// gives `last_edited_time` by the minute.
fn minute_end(
    last_edited_time: OffsetDateTime,
    response: &HttpResponse,
) -> Result<Option<OffsetDateTime>> {
    let date = response
        .header("date")
        .ok_or_else(|| ProviderError::invalid_input("an answer of Notion has no Date"))?;
    let date = OffsetDateTime::parse(date, &Rfc2822).map_err(|error| {
        ProviderError::invalid_input(format!("the Date {date} of Notion: {error}"))
    })?;
    let minute = last_edited_time
        .replace_second(0)
        .and_then(|time| time.replace_nanosecond(0))
        .map_err(|error| ProviderError::internal(error.to_string()))?;
    let end = minute + Duration::MINUTE;
    if date >= end {
        return Ok(None);
    }
    Ok(Some(OffsetDateTime::now_utc() + (end - date)))
}

// ===========================================================================
// The feed of the workspace
// ===========================================================================

/// The token of the feed of a page or a data source, as an inner state of
/// the feed of the workspace.
enum Inner {
    Page(Id, String),
    DataSource(Id, String),
}

/// One call of the feed of the workspace: its context, the latest time of
/// its token, and the inner states of its part.
struct WorkspaceFeed<'a> {
    cx: &'a FeedCx<WorkspaceKey>,
    latest: Option<Latest>,
    inner: Vec<Inner>,
}

impl Workspace {
    /// The feed of the pages and the data sources of the workspace.
    pub(crate) async fn feed(cx: FeedCx<WorkspaceKey>) -> Result<Feed> {
        let token = cx.token().map(decode_token::<WorkspaceToken>).transpose()?;
        let first = token.is_none();
        let (latest, stage) = match token {
            Some(WorkspaceToken { latest, stage }) => (latest, stage),
            None => (None, None),
        };
        let listing = [cx.start(), cx.due()]
            .into_iter()
            .flatten()
            .any(|start| start == "pages" || start == "databases");
        let stage = match stage {
            Some(stage) => stage,
            None if first || listing => Stage::Listing {
                first,
                cursor: None,
                pages: BTreeSet::new(),
                databases: BTreeSet::new(),
                seen: None,
            },
            None => Stage::Changes {
                cursor: None,
                seen: None,
            },
        };
        let mut feed = WorkspaceFeed {
            cx: &cx,
            latest,
            inner: Vec::new(),
        };
        match stage {
            Stage::Listing {
                first,
                cursor,
                pages,
                databases,
                seen,
            } => feed.listing(first, cursor, pages, databases, seen).await,
            Stage::Missing {
                first,
                pages,
                databases,
                seen,
            } => feed.missing(first, pages, databases, seen).await,
            Stage::Changes { cursor, seen } => feed.changes(cursor, seen).await,
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
    async fn search(&self, cursor: Option<&String>, page_size: usize) -> Result<Search> {
        let mut body = json!({
            "sort": { "direction": "descending", "timestamp": "last_edited_time" },
            "page_size": page_size,
        });
        if let Some(cursor) = cursor {
            body["start_cursor"] = cursor.clone().into();
        }
        let response = self
            .cx
            .endpoint(NotionApi)
            .post("/v1/search")
            .body_json(&body)
            .send()
            .await?;
        match response.status() {
            200 => Ok(Search::List(decode(response.body())?)),
            _ if cursor_refused(cursor, &response) => Ok(Search::Refused(status_error(&response))),
            _ => Err(status_error(&response)),
        }
    }

    /// The restart of an answer whose cursor Notion refused, with the token
    /// of the answer at its first request.
    fn restart(&self, stage: Stage, error: ProviderError) -> Result<Feed> {
        Ok(Feed::restart(
            encode(&WorkspaceToken {
                latest: self.latest,
                stage: Some(stage),
            })?,
            error,
        ))
    }

    /// Take a result of the search, or of a request by its ID, into the
    /// projection, its folder joining its listing as `write` says, and give
    /// its feed its token: the removal of one in the trash, and the facts of
    /// another, with the removal of the content and the comments of a page
    /// with another `last_edited_time` than its `page.json`.
    fn take(&mut self, object: &RawValue, write: ListingWrite) -> Result<(Found, Id)> {
        let found: Found = decode(object.get().as_bytes())?;
        let id = Id::of(&found.id)?;
        match found.object.as_str() {
            "page" if found.in_trash => {
                self.cx.remove(page_path(&id))?;
                self.inner
                    .push(Inner::Page(id.clone(), encode(&None::<PageToken>)?));
            },
            "page" => {
                let projected = self
                    .cx
                    .read(format!("{}/page.json", page_path(&id)))?
                    .map(|page| decode::<Found>(&page))
                    .transpose()?;
                if projected
                    .as_ref()
                    .is_none_or(|page| page.last_edited_time != found.last_edited_time)
                {
                    if projected.is_some() {
                        remove_content(self.cx, &id)?;
                    }
                    let token = PageToken {
                        last_edited_time: found.last_edited_time,
                        next: None,
                    };
                    self.inner
                        .push(Inner::Page(id.clone(), encode(&Some(token))?));
                }
                write_page(self.cx, &id, object, write)?;
            },
            "data_source" if found.in_trash => {
                self.cx.remove(data_source_path(&id))?;
                self.inner.push(Inner::DataSource(
                    id.clone(),
                    encode(&None::<DataSourceToken>)?,
                ));
            },
            "data_source" => {
                write_data_source(self.cx, &id, object, write)?;
                let token = DataSourceToken {
                    last_edited_time: found.last_edited_time,
                };
                self.inner
                    .push(Inner::DataSource(id.clone(), encode(&Some(token))?));
            },
            object => {
                return Err(ProviderError::invalid_input(format!(
                    "the search of Notion gave a {object}"
                )));
            },
        }
        Ok((found, id))
    }

    /// A request of a full listing: its results, then its next request, the
    /// pages and the data sources of the projection that its results lack,
    /// or its end.
    async fn listing(
        &mut self,
        first: bool,
        cursor: Option<String>,
        mut pages: BTreeSet<Id>,
        mut databases: BTreeSet<Id>,
        mut seen: Option<Latest>,
    ) -> Result<Feed> {
        let list = match self.search(cursor.as_ref(), PAGE).await? {
            Search::List(list) => list,
            Search::Refused(error) => {
                let stage = Stage::Listing {
                    first,
                    cursor: None,
                    pages: BTreeSet::new(),
                    databases: BTreeSet::new(),
                    seen: None,
                };
                return self.restart(stage, error);
            },
        };
        let write = listing_write(first);
        for result in &list.results {
            let (found, id) = self.take(result, write)?;
            if found.object == "page" {
                pages.insert(id);
            } else {
                databases.insert(id);
            }
            seen = Some(Latest::with(seen, found.last_edited_time));
        }
        if let Some(cursor) = list.next()? {
            let stage = Stage::Listing {
                first,
                cursor: Some(cursor),
                pages,
                databases,
                seen,
            };
            return self.part(Some(stage));
        }
        let pages = &listed(self.cx, PAGES)? - &pages;
        let databases = &listed(self.cx, DATABASES)? - &databases;
        if pages.is_empty() && databases.is_empty() {
            return self.end(first, seen);
        }
        self.part(Some(Stage::Missing {
            first,
            pages,
            databases,
            seen,
        }))
    }

    /// A request of a page or a data source of the projection that a full
    /// listing lacks, by its ID: its facts, or its removal for `404` or in
    /// the trash.
    async fn missing(
        &mut self,
        first: bool,
        mut pages: BTreeSet<Id>,
        mut databases: BTreeSet<Id>,
        seen: Option<Latest>,
    ) -> Result<Feed> {
        let (path, request) = if let Some(id) = pages.pop_first() {
            (page_path(&id), format!("/v1/pages/{id}"))
        } else if let Some(id) = databases.pop_first() {
            (data_source_path(&id), format!("/v1/data_sources/{id}"))
        } else {
            return Err(ProviderError::invalid_input(
                "a full listing takes no missing page or data source",
            ));
        };
        let response = self.cx.endpoint(NotionApi).get(request).send().await?;
        match response.status() {
            200 => {
                let object: Box<RawValue> = decode(response.body())?;
                self.take(&object, listing_write(first))?;
            },
            404 => self.cx.remove(&path)?,
            _ => return Err(status_error(&response)),
        }
        if pages.is_empty() && databases.is_empty() {
            return self.end(first, seen);
        }
        self.part(Some(Stage::Missing {
            first,
            pages,
            databases,
            seen,
        }))
    }

    /// A request of the changes: the first with the `page_size` of the count
    /// of the latest time and one more, at most `100`, each next one with
    /// `100`, until a result before the latest time, which they leave.
    async fn changes(&mut self, cursor: Option<String>, mut seen: Option<Latest>) -> Result<Feed> {
        let page_size = match cursor {
            Some(_) => PAGE,
            None => self.latest.map_or(1, |latest| latest.count + 1).min(PAGE),
        };
        let list = match self.search(cursor.as_ref(), page_size).await? {
            Search::List(list) => list,
            Search::Refused(error) => {
                let stage = Stage::Changes {
                    cursor: None,
                    seen: None,
                };
                return self.restart(stage, error);
            },
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
            self.take(result, ListingWrite::Complete)?;
            seen = Some(Latest::with(seen, found.last_edited_time));
        }
        match list.next()? {
            Some(cursor) if !older => self.part(Some(Stage::Changes {
                cursor: Some(cursor),
                seen,
            })),
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
    /// parts while a stage remains, with the inner states of its results.
    fn part(&mut self, stage: Option<Stage>) -> Result<Feed> {
        let more = stage.is_some();
        let mut part = Feed::part(encode(&WorkspaceToken {
            latest: self.latest,
            stage,
        })?)
        .more(more);
        for inner in self.inner.drain(..) {
            part = match inner {
                Inner::Page(id, token) => part.inner::<Page>(&PageKey { id }, token),
                Inner::DataSource(id, token) => {
                    part.inner::<DataSource>(&DataSourceKey { id }, token)
                },
            };
        }
        Ok(part.into())
    }
}

/// How the results of a full listing join `pages/` and `databases/`: as they
/// are before the last part of the first full listing, complete after it.
fn listing_write(first: bool) -> ListingWrite {
    if first {
        ListingWrite::Partial
    } else {
        ListingWrite::Complete
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

/// What a request of a page leaves: its next request, its end with the time
/// after which its facts count as lacking, the page gone from the workspace,
/// or the refusal of its cursor.
enum PageStep {
    Next(PageRequest),
    Done(Option<OffsetDateTime>),
    Gone,
    Refused(ProviderError),
}

impl Page {
    /// The feed of a page: the next request of its token; else its content
    /// for the start `body.md`, and its comments for a start in `comments`,
    /// each after the page by its ID in the same answer for a page without a
    /// token; else the page by its ID. Each answer makes a part of each
    /// request.
    pub(crate) async fn feed(cx: FeedCx<PageKey>) -> Result<Feed> {
        let feed = PageFeed {
            cx: &cx,
            id: &cx.key().id,
        };
        let token = cx
            .token()
            .map(decode_token::<Option<PageToken>>)
            .transpose()?
            .flatten();
        let request = match cx.start() {
            Some("body.md") => Some(PageRequest::Content {
                markdown: None,
                queue: VecDeque::new(),
                until: None,
            }),
            Some(start) if start == "comments" || start.starts_with("comments/") => {
                Some(PageRequest::Comments {
                    cursor: None,
                    comments: Vec::new(),
                    until: None,
                })
            },
            _ => None,
        };
        match (token, request) {
            (
                Some(PageToken {
                    last_edited_time,
                    next: Some(next),
                }),
                _,
            ) => feed.step(last_edited_time, next).await,
            (Some(token), Some(request)) => feed.step(token.last_edited_time, request).await,
            (token, request) => feed.page(token, request).await,
        }
    }
}

impl PageFeed<'_> {
    fn path(&self) -> String {
        page_path(self.id)
    }

    /// The page by its ID: its facts, its folder joining `pages/`, with the
    /// removal of its content and its comments for another `last_edited_time`
    /// than that of `token`, or its removal for `404` or in the trash; then
    /// `request`, in the same answer.
    async fn page(&self, token: Option<PageToken>, request: Option<PageRequest>) -> Result<Feed> {
        let response = self
            .cx
            .endpoint(NotionApi)
            .get(format!("/v1/pages/{}", self.id))
            .send()
            .await?;
        let object: Box<RawValue> = match response.status() {
            200 => decode(response.body())?,
            404 => return self.removal(),
            _ => return Err(status_error(&response)),
        };
        let found: Found = decode(object.get().as_bytes())?;
        if found.in_trash {
            return self.removal();
        }
        if token
            .as_ref()
            .is_some_and(|token| token.last_edited_time != found.last_edited_time)
        {
            remove_content(self.cx, self.id)?;
        }
        write_page(self.cx, self.id, &object, ListingWrite::Join)?;
        page_part(found.last_edited_time, request, None)
    }

    /// One request of an unfinished answer of the page, a part with the next
    /// request in its token, or its last part.
    async fn step(&self, last_edited_time: OffsetDateTime, request: PageRequest) -> Result<Feed> {
        let step = match request {
            PageRequest::Content {
                markdown,
                queue,
                until,
            } => {
                self.content(last_edited_time, markdown, queue, until)
                    .await?
            },
            PageRequest::Comments {
                cursor,
                comments,
                until,
            } => {
                self.comments(last_edited_time, cursor, comments, until)
                    .await?
            },
        };
        match step {
            PageStep::Next(request) => page_part(last_edited_time, Some(request), None),
            PageStep::Done(until) => page_part(last_edited_time, None, until),
            PageStep::Gone => self.removal(),
            PageStep::Refused(error) => {
                let token = PageToken {
                    last_edited_time,
                    next: None,
                };
                Ok(Feed::restart(encode(&Some(token))?, error))
            },
        }
    }

    /// A request of the content: the markdown of the page, then that of each
    /// unknown block of each answer in turn, a block with `404` without its
    /// markdown, and at its last part `body.md`, the markdown of the page,
    /// then that of each block in their order, each after an empty line.
    async fn content(
        &self,
        last_edited_time: OffsetDateTime,
        markdown: Option<String>,
        mut queue: VecDeque<String>,
        until: Option<OffsetDateTime>,
    ) -> Result<PageStep> {
        let (markdown, until) = match markdown {
            None => {
                let response = self.markdown(self.id.as_str()).await?;
                match response.status() {
                    200 => {},
                    404 => return Ok(PageStep::Gone),
                    _ => return Err(status_error(&response)),
                }
                let answer: Markdown = decode(response.body())?;
                queue.extend(answer.unknown_block_ids);
                (answer.markdown, minute_end(last_edited_time, &response)?)
            },
            Some(mut markdown) => {
                let block = queue.pop_front().ok_or_else(|| {
                    ProviderError::invalid_input("the content of a page takes no unknown block")
                })?;
                let response = self.markdown(&block).await?;
                match response.status() {
                    200 => {
                        let answer: Markdown = decode(response.body())?;
                        queue.extend(answer.unknown_block_ids);
                        markdown.push_str("\n\n");
                        markdown.push_str(&answer.markdown);
                    },
                    404 => {},
                    _ => return Err(status_error(&response)),
                }
                (markdown, until)
            },
        };
        if !queue.is_empty() {
            return Ok(PageStep::Next(PageRequest::Content {
                markdown: Some(markdown),
                queue,
                until,
            }));
        }
        self.cx
            .file(format!("{}/body.md", self.path()), markdown.into_bytes())?;
        Ok(PageStep::Done(until))
    }

    /// `GET /v1/pages/{id}/markdown?markdown_version=v2` of the page or of a
    /// block, by its ID.
    async fn markdown(&self, id: &str) -> Result<HttpResponse> {
        self.cx
            .endpoint(NotionApi)
            .get(format!("/v1/pages/{id}/markdown"))
            .query("markdown_version", "v2")
            .send()
            .await
    }

    /// A request of the comments, by the cursor of the previous one, and at
    /// their last part each comment, `comments/` complete, and the removal of
    /// each comment of the projection that their answers lack.
    async fn comments(
        &self,
        last_edited_time: OffsetDateTime,
        cursor: Option<String>,
        mut comments: Vec<Box<RawValue>>,
        until: Option<OffsetDateTime>,
    ) -> Result<PageStep> {
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
        match response.status() {
            200 => {},
            404 => return Ok(PageStep::Gone),
            _ if cursor_refused(cursor.as_ref(), &response) => {
                return Ok(PageStep::Refused(status_error(&response)));
            },
            _ => return Err(status_error(&response)),
        }
        let until = match cursor {
            None => minute_end(last_edited_time, &response)?,
            Some(_) => until,
        };
        let list: List = decode(response.body())?;
        let next = list.next()?;
        comments.extend(list.results);
        if let Some(cursor) = next {
            return Ok(PageStep::Next(PageRequest::Comments {
                cursor: Some(cursor),
                comments,
                until,
            }));
        }
        let folder = format!("{}/comments", self.path());
        let mut taken = BTreeSet::new();
        for object in &comments {
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
            taken.insert(id);
        }
        self.cx.dir(&folder, ListingWrite::Complete)?;
        for id in listed(self.cx, &folder)?.difference(&taken) {
            self.cx.remove(format!("{folder}/{id}"))?;
        }
        Ok(PageStep::Done(until))
    }

    /// The removal of the page, with the token of a removed page.
    fn removal(&self) -> Result<Feed> {
        self.cx.remove(self.path())?;
        Ok(Feed::part(encode(&None::<PageToken>)?).into())
    }
}

// ===========================================================================
// The feed of a data source
// ===========================================================================

impl DataSource {
    /// The feed of a data source: the data source by its ID, its facts with
    /// its folder joining `databases/`, or its removal for `404` or in the
    /// trash.
    pub(crate) async fn feed(cx: FeedCx<DataSourceKey>) -> Result<Feed> {
        let id = &cx.key().id;
        let response = cx
            .endpoint(NotionApi)
            .get(format!("/v1/data_sources/{id}"))
            .send()
            .await?;
        let object: Box<RawValue> = match response.status() {
            200 => decode(response.body())?,
            404 => return data_source_removal(&cx, id),
            _ => return Err(status_error(&response)),
        };
        let found: Found = decode(object.get().as_bytes())?;
        if found.in_trash {
            return data_source_removal(&cx, id);
        }
        write_data_source(&cx, id, &object, ListingWrite::Join)?;
        let token = DataSourceToken {
            last_edited_time: found.last_edited_time,
        };
        Ok(Feed::part(encode(&Some(token))?).into())
    }
}

/// A part of the feed of a page with `last_edited_time` and the next request,
/// followed by more parts while there is one, its facts counting as lacking
/// after `until`.
fn page_part(
    last_edited_time: OffsetDateTime,
    next: Option<PageRequest>,
    until: Option<OffsetDateTime>,
) -> Result<Feed> {
    let more = next.is_some();
    let token = PageToken {
        last_edited_time,
        next,
    };
    let part = Feed::part(encode(&Some(token))?).more(more);
    Ok(match until {
        Some(until) => part.until(SystemTime::from(until)),
        None => part,
    }
    .into())
}

/// The removal of a data source, with the token of a removed data source.
fn data_source_removal(cx: &FeedCx<DataSourceKey>, id: &Id) -> Result<Feed> {
    cx.remove(data_source_path(id))?;
    Ok(Feed::part(encode(&None::<DataSourceToken>)?).into())
}
