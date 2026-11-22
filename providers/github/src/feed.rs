//! The feeds of the issues, pull requests, and comments of a repository.
//!
//! The feed of a repository takes its first full listing a part for each of
//! its requests: the rows of its issues and pull requests by `updated_at`,
//! then its comments by `updated_at`, then the GitHub GraphQL counts of the
//! comments of each item, with the IDs of an item with another count. After
//! it, each answer is a check, a part for each of its requests: the rows
//! since the time of the last row, the comments since the time of the last
//! comment, and the counts of the issues and of the pull requests with a
//! page of the counts of the comments of the items. Before the first full
//! listing ends, the feed of an item answers for that item from
//! `GET /repos/{owner}/{repo}/issues/{number}`, a part for each of its
//! requests.
//!
//! A page of rows or of comments after one whose last row moved the time of
//! its list asks for the rows since that time, so that a row that changes
//! between two pages moves no other row past a page.
//!
//! The facts of an issue or a pull request live once, under its folder in
//! `all/`, which its entry in `open/` shares while it is open.

use std::collections::{BTreeMap, BTreeSet};

use omnifs_sdk::prelude::*;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::format_description::well_known::Rfc3339;
use time::macros::format_description;
use time::{OffsetDateTime, UtcOffset};

use crate::api::GitHubApi;
use crate::item::{IssueKey, ItemKind, PullKey, RepoKey};
use crate::objects::{Comment, Issue, ItemData, PullRequest, Repo};
use crate::{RepoId, StateFilter};

/// The rows, comments, and GraphQL nodes of a page.
const PAGE: usize = 100;

// ===========================================================================
// Tokens
// ===========================================================================

/// The token of the feed of a repository: the list of its rows, the list of
/// its comments, the page of the counts of the comments of its items that
/// the next request of them asks for, and, while its first full listing or a
/// check is unfinished, its stage.
#[derive(Serialize, Deserialize)]
struct RepoToken {
    rows: Since,
    comments: Since,
    counts: CountsPage,
    stage: Option<Stage>,
}

/// The request of a page of a list of the repository in the order of
/// `updated_at`.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PageRequest {
    /// The rows since the time of the list.
    Since,
    /// The page at a link `next`, after a page whose rows end at the time
    /// they were asked since, which asks no further.
    Link(String),
}

/// A list of the repository in the order of `updated_at`: the `updated_at`
/// of its last row, and the `ETag` of the answer with each of its rows since
/// that time.
#[derive(Clone, Default, Serialize, Deserialize)]
struct Since {
    #[serde(with = "time::serde::rfc3339::option")]
    time: Option<OffsetDateTime>,
    etag: Option<String>,
}

/// A page of the counts of the comments of the items of a connection, after
/// a cursor.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
struct CountsPage {
    connection: ItemKind,
    cursor: Option<String>,
}

impl CountsPage {
    const FIRST: Self = Self {
        connection: ItemKind::Issues,
        cursor: None,
    };

    /// The page after this one, whose cursor `next` is: the first page of
    /// the pull requests after the last of the issues, and the first page
    /// again after the last of the pull requests.
    fn after(&self, next: Option<String>) -> Self {
        match (next, self.connection) {
            (Some(cursor), connection) => Self {
                connection,
                cursor: Some(cursor),
            },
            (None, ItemKind::Issues) => Self {
                connection: ItemKind::Pulls,
                cursor: None,
            },
            (None, ItemKind::Pulls) => Self::FIRST,
        }
    }
}

/// The stage of an unfinished first full listing or check of a repository.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Stage {
    /// A page of the rows of the first full listing.
    Rows(PageRequest),
    /// A page of the comments of the first full listing.
    Comments(PageRequest),
    /// The counts of the first full listing, at their page of the token.
    Counts,
    /// The IDs of the items of a page of the counts with another count than
    /// their `comments/`, the first of `items` at a time; then the next page
    /// of the counts, or the end of the first full listing after their last
    /// page, when the page of the token is the first again.
    Ids {
        kind: ItemKind,
        items: BTreeSet<u64>,
        ids: Ids,
    },
    /// A check, at its next request.
    Check(Box<Check>),
}

/// An unfinished check: its next request, each changed item by its number,
/// the counts of GitHub GraphQL, the items that GitHub GraphQL removed, and
/// the kinds whose numbers it took.
#[derive(Serialize, Deserialize)]
struct Check {
    next: CheckRequest,
    changed: BTreeMap<u64, Changed>,
    totals: Option<Totals>,
    removed: BTreeSet<ItemRef>,
    numbered: BTreeSet<ItemKind>,
}

/// A changed item of a check: its kind, the count of its comments, and its
/// IDs.
#[derive(Serialize, Deserialize)]
struct Changed {
    kind: ItemKind,
    comments: usize,
    ids: Ids,
}

impl Changed {
    fn new(kind: ItemKind, comments: usize) -> Self {
        Self {
            kind,
            comments,
            ids: Ids::new(BTreeSet::new(), None),
        }
    }
}

/// The counts `totalCount` of the issues and of the pull requests.
#[derive(Clone, Copy, Serialize, Deserialize)]
struct Totals {
    issues: usize,
    pulls: usize,
}

/// The next request of a check.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CheckRequest {
    /// A page of the rows.
    Rows(PageRequest),
    /// A page of the comments.
    Comments(PageRequest),
    /// The `POST` of GitHub GraphQL with the counts, while the check lacks
    /// them, and the first page of the IDs of the next changed items.
    Counts,
    /// The next request of the IDs of a changed item.
    Ids { number: u64 },
    /// A page of the numbers of a kind, with the numbers before it.
    Numbers {
        kind: ItemKind,
        after: Option<String>,
        numbers: BTreeSet<u64>,
    },
    /// The first of the items of a kind that its numbers brought and that
    /// neither the projection nor the rows of the check have, by its number,
    /// with the ones after it: an item made after the rows of the check.
    Items {
        kind: ItemKind,
        numbers: BTreeSet<u64>,
    },
}

/// The token of the feed of an item: the `ETag` of its last answer, none for
/// an item from a page of rows, its `updated_at`, its count `comments`, and
/// the next request of an unfinished answer. The token of a removed item is
/// `null`.
#[derive(Serialize, Deserialize)]
struct ItemToken {
    etag: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    updated_at: OffsetDateTime,
    comments: usize,
    next: Option<ItemRequest>,
}

/// The next request of the comments of an item.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ItemRequest {
    /// A page of its comments updated since a time, at a link `next`, else
    /// the first, with the IDs of the comments before it.
    Since {
        #[serde(with = "time::serde::rfc3339")]
        since: OffsetDateTime,
        link: Option<String>,
        brought: BTreeSet<u64>,
    },
    /// A page of all of its comments, at a link `next`, else the first, with
    /// the IDs of the comments before it.
    Comments {
        link: Option<String>,
        brought: BTreeSet<u64>,
    },
    /// The next request of its IDs.
    Ids(Ids),
}

/// The IDs of the comments of an item, a request at a time: each page of
/// them, and each comment by its ID that a page brought, that `brought`, the
/// comments the lists of the answer brought, lacks, and that the projection
/// lacks or that was updated at or after `since`. After the last page, each
/// comment of the projection and of `brought` that `seen`, the IDs of the
/// pages, lacks leaves, and the `comments/` of the item is complete with the
/// last comment by its ID.
#[derive(Serialize, Deserialize)]
struct Ids {
    page: IdsPage,
    seen: BTreeSet<u64>,
    brought: BTreeSet<u64>,
    #[serde(with = "time::serde::rfc3339::option")]
    since: Option<OffsetDateTime>,
    fetch: BTreeSet<u64>,
}

/// The next page of the IDs of an item.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum IdsPage {
    First,
    After(String),
    End,
}

/// An issue or a pull request of the repository of a feed.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct ItemRef {
    kind: ItemKind,
    number: u64,
}

fn encode<T: Serialize>(token: &T) -> Result<String> {
    serde_json::to_string(token)
        .map_err(|error| ProviderError::internal(format!("feed token: {error}")))
}

fn decode<T: DeserializeOwned>(token: &str) -> Result<T> {
    serde_json::from_str(token)
        .map_err(|error| ProviderError::invalid_input(format!("feed token: {error}")))
}

// ===========================================================================
// Rows
// ===========================================================================

/// A row of a list in the order of `updated_at`.
trait Dated {
    fn updated_at(&self) -> OffsetDateTime;
}

/// A row of `GET /repos/{owner}/{repo}/issues`, or the answer of
/// `GET /repos/{owner}/{repo}/issues/{number}`.
#[derive(Deserialize)]
struct ItemRow {
    #[serde(flatten)]
    data: ItemData,
    #[serde(with = "time::serde::rfc3339")]
    updated_at: OffsetDateTime,
    comments: usize,
}

impl ItemRow {
    fn item(&self) -> ItemRef {
        ItemRef {
            kind: if self.data.is_pull_request() {
                ItemKind::Pulls
            } else {
                ItemKind::Issues
            },
            number: self.data.number,
        }
    }

    fn token(&self, etag: Option<String>) -> ItemToken {
        ItemToken {
            etag,
            updated_at: self.updated_at,
            comments: self.comments,
            next: None,
        }
    }
}

impl Dated for ItemRow {
    fn updated_at(&self) -> OffsetDateTime {
        self.updated_at
    }
}

/// A row of `GET /repos/{owner}/{repo}/issues/comments`.
#[derive(Deserialize)]
struct CommentRow {
    #[serde(flatten)]
    comment: Comment,
    html_url: String,
    #[serde(with = "time::serde::rfc3339")]
    updated_at: OffsetDateTime,
}

impl CommentRow {
    /// The item of the comment, from the kind and the number of its
    /// `html_url`, `https://github.com/{owner}/{repo}/{issues|pull}/{number}#issuecomment-{id}`.
    fn item(&self) -> Result<ItemRef> {
        let invalid = || ProviderError::invalid_input(format!("comment at {}", self.html_url));
        let (path, _) = self.html_url.split_once('#').ok_or_else(invalid)?;
        let (rest, number) = path.rsplit_once('/').ok_or_else(invalid)?;
        let kind = match rest.rsplit_once('/').ok_or_else(invalid)?.1 {
            "issues" => ItemKind::Issues,
            "pull" => ItemKind::Pulls,
            _ => return Err(invalid()),
        };
        Ok(ItemRef {
            kind,
            number: number.parse().map_err(|_| invalid())?,
        })
    }
}

impl Dated for CommentRow {
    fn updated_at(&self) -> OffsetDateTime {
        self.updated_at
    }
}

/// A page of a list in the order of `updated_at`: its rows, and the request
/// of the page after it, none after the last page.
struct Page<T> {
    rows: Vec<T>,
    next: Option<PageRequest>,
}

/// Ask for a page of the list at `list` in the order of `updated_at`, the
/// rows since the time of `since` with its `ETag`. `None` for `304`: its
/// rows stand. `since` takes the time of the last row of the page, and the
/// `ETag` of an answer since that time with no page after it.
async fn page_since<K, T: DeserializeOwned + Dated>(
    cx: &FeedCx<K>,
    list: &str,
    since: &mut Since,
    page: PageRequest,
) -> Result<Option<Page<T>>> {
    let asked_since = matches!(page, PageRequest::Since);
    let request = match page {
        PageRequest::Link(link) => cx.endpoint(GitHubApi).get(link),
        PageRequest::Since => cx
            .endpoint(GitHubApi)
            .get(format!("{list}{}", since_pair(since.time)?))
            .maybe_if_none_match(since.etag.clone().map(VersionToken).as_ref()),
    };
    let answer = request.page::<Vec<T>>().await?;
    let next = next_link(&answer)?;
    let PageAnswer::Fresh {
        value: rows,
        validator,
        ..
    } = answer
    else {
        return Ok(None);
    };
    let time = rows.last().map(Dated::updated_at).or(since.time);
    let moved = time != since.time;
    since.etag = validator
        .filter(|_| asked_since && !moved && next.is_none())
        .map(|etag| etag.0);
    since.time = time;
    Ok(Some(Page {
        rows,
        next: next.map(|link| {
            if moved {
                PageRequest::Since
            } else {
                PageRequest::Link(link)
            }
        }),
    }))
}

/// The link `next` of the header `Link` of an answer, as a path of the API.
fn next_link<T>(answer: &PageAnswer<T>) -> Result<Option<String>> {
    let Some(url) = answer.header("link").and_then(|links| {
        links.split(',').find_map(|link| {
            let (url, params) = link.split_once(';')?;
            params
                .split(';')
                .any(|param| param.trim() == r#"rel="next""#)
                .then(|| url.trim().trim_start_matches('<').trim_end_matches('>'))
        })
    }) else {
        return Ok(None);
    };
    url.strip_prefix(GitHubApi.base())
        .map(|path| Some(path.to_owned()))
        .ok_or_else(|| ProviderError::invalid_input(format!("link next {url} leaves the API")))
}

/// The answer of `GET /repos/{owner}/{repo}/issues/{number}`.
enum ItemAnswer {
    /// `304`: the item of the token stands.
    Unchanged,
    /// `301`, `404`, or `410`: the item left the repository.
    Removed,
    /// `200`: the item, with the `ETag` of the answer.
    Found(ItemRow, Option<String>),
}

/// Ask for an item, with `If-None-Match` and the `ETag` of its token, or
/// with `If-Modified-Since` and the HTTP date of the `updated_at` of a token
/// without an `ETag`.
async fn item_answer<K>(
    cx: &FeedCx<K>,
    repo: &RepoId,
    item: ItemRef,
    token: Option<&ItemToken>,
) -> Result<ItemAnswer> {
    let request = cx
        .endpoint(GitHubApi)
        .get(format!("/repos/{repo}/issues/{}", item.number));
    let request = match token {
        None => request,
        Some(ItemToken {
            etag: Some(etag), ..
        }) => request.header("If-None-Match", etag.as_str()),
        Some(ItemToken {
            etag: None,
            updated_at,
            ..
        }) => request.header("If-Modified-Since", http_date(*updated_at)?),
    };
    let response = request.send().await?;
    match response.status() {
        200 => Ok(ItemAnswer::Found(
            serde_json::from_slice(response.body())
                .map_err(|error| ProviderError::invalid_input(format!("json decode: {error}")))?,
            response.header("etag").map(ToOwned::to_owned),
        )),
        304 => Ok(ItemAnswer::Unchanged),
        301 | 404 | 410 => Ok(ItemAnswer::Removed),
        status => Err(ProviderError::from_http_status(status)),
    }
}

/// A page of the comments of an item: the page at `link`, else the first of
/// its comments updated since a time, with the link `next` of the page.
async fn item_comments<K>(
    cx: &FeedCx<K>,
    repo: &RepoId,
    item: ItemRef,
    since: Option<OffsetDateTime>,
    link: Option<String>,
) -> Result<(Vec<Comment>, Option<String>)> {
    let path = match link {
        Some(link) => link,
        None => format!(
            "/repos/{repo}/issues/{}/comments?per_page={PAGE}{}",
            item.number,
            since_pair(since)?
        ),
    };
    let answer = cx.endpoint(GitHubApi).get(path).page().await?;
    let next = next_link(&answer)?;
    match answer {
        PageAnswer::Fresh { value, .. } => Ok((value, next)),
        PageAnswer::Unchanged { .. } => Err(unconditional_304("the comments")),
    }
}

/// The query pair `since` of a time, none without a time.
fn since_pair(time: Option<OffsetDateTime>) -> Result<String> {
    match time {
        Some(time) => Ok(format!("&since={}", rfc3339(time)?)),
        None => Ok(String::new()),
    }
}

fn rfc3339(time: OffsetDateTime) -> Result<String> {
    time.format(&Rfc3339)
        .map_err(|error| ProviderError::internal(format!("time {time}: {error}")))
}

fn http_date(time: OffsetDateTime) -> Result<String> {
    time.to_offset(UtcOffset::UTC)
        .format(format_description!(
            "[weekday repr:short], [day] [month repr:short] [year] [hour]:[minute]:[second] GMT"
        ))
        .map_err(|error| ProviderError::internal(format!("time {time}: {error}")))
}

// ===========================================================================
// GitHub GraphQL
// ===========================================================================

/// An answer of GitHub GraphQL: its data, with its errors.
#[derive(Deserialize)]
struct GraphQl<T> {
    data: Option<GraphQlData<T>>,
    errors: Option<Vec<GraphQlError>>,
}

#[derive(Deserialize)]
struct GraphQlError {
    #[serde(rename = "type")]
    kind: Option<String>,
    message: String,
}

/// The data of an answer of GitHub GraphQL about a repository.
#[derive(Deserialize)]
struct GraphQlData<T> {
    repository: T,
}

/// A page of a connection under the alias `page`.
#[derive(Deserialize)]
struct Paged<T> {
    page: Connection<T>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Connection<T> {
    nodes: Vec<T>,
    page_info: PageInfo,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

impl PageInfo {
    /// The cursor of the next page, `None` after the last page.
    fn next(self) -> Result<Option<String>> {
        if !self.has_next_page {
            return Ok(None);
        }
        self.end_cursor.map(Some).ok_or_else(|| {
            ProviderError::invalid_input("a GraphQL page before another has no cursor")
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Total {
    total_count: usize,
}

/// An item with the count of its comments.
#[derive(Deserialize)]
struct Counted {
    number: u64,
    comments: Total,
}

#[derive(Deserialize)]
struct Numbered {
    number: u64,
}

/// The id of a comment, with the time of its last update.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommentId {
    database_id: u64,
    #[serde(with = "time::serde::rfc3339")]
    updated_at: OffsetDateTime,
}

/// An item with a page of the ids of its comments.
#[derive(Deserialize)]
struct Commented {
    comments: Connection<CommentId>,
}

/// The answer of the query of the ids of an item, `null` for an item that
/// left the repository.
#[derive(Deserialize)]
struct Item {
    item: Option<Commented>,
}

/// The answer of the first query of a check: the counts of the issues and of
/// the pull requests, a page of the counts of the comments of the items, and
/// each changed item under its alias, `null` for an item that left the
/// repository.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Counts {
    issues: Total,
    pull_requests: Total,
    page: Connection<Counted>,
    #[serde(flatten)]
    items: BTreeMap<String, Option<Commented>>,
}

impl ItemKind {
    /// The GraphQL connection of the items of the kind.
    const fn connection(self) -> &'static str {
        match self {
            Self::Issues => "issues",
            Self::Pulls => "pullRequests",
        }
    }
}

/// Ask GitHub GraphQL `query` about the repository, with `variables` and the
/// repository as `$owner` and `$name`. An item that GitHub GraphQL cannot
/// resolve, an error of the type `NOT_FOUND`, is `null` in the data; an
/// answer with another error fails.
async fn graphql<T: DeserializeOwned>(
    cx: &Cx,
    repo: &RepoId,
    query: &str,
    mut variables: serde_json::Value,
) -> Result<T> {
    variables["owner"] = repo.owner.to_string().into();
    variables["name"] = repo.repo.to_string().into();
    let response = cx
        .endpoint(GitHubApi)
        .post("/graphql")
        .body_json(&json!({ "query": query, "variables": variables }))
        .send()
        .await?;
    if response.status() != 200 {
        return Err(ProviderError::from_http_status(response.status()));
    }
    let answer: GraphQl<T> = serde_json::from_slice(response.body())
        .map_err(|error| ProviderError::invalid_input(format!("GitHub GraphQL: {error}")))?;
    let errors = answer.errors.iter().flatten().collect::<Vec<_>>();
    let message = || {
        format!(
            "GitHub GraphQL: {}",
            errors
                .iter()
                .map(|error| error.message.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        )
    };
    if errors
        .iter()
        .any(|error| error.kind.as_deref() != Some("NOT_FOUND"))
    {
        return Err(ProviderError::internal(message()));
    }
    answer
        .data
        .map(|data| data.repository)
        .ok_or_else(|| ProviderError::internal(message()))
}

/// The selection of the ids of the comments of an item, with `arguments`.
fn comment_ids(arguments: &str) -> String {
    let comments = format!(
        "comments({arguments}) {{ nodes {{ databaseId updatedAt }} pageInfo {{ hasNextPage endCursor }} }}"
    );
    format!("... on Issue {{ {comments} }} ... on PullRequest {{ {comments} }}")
}

/// The selection of a node of a page of a connection with the counts of its
/// comments.
const COUNTED: &str = "number comments { totalCount }";

/// The selection of a node of a page of a connection with its number.
const NUMBERED: &str = "number";

/// The selection of the counts of the issues and of the pull requests.
const TOTALS: &str = "issues { totalCount } pullRequests { totalCount }";

/// The selection of the page after `$after` of the items of a connection,
/// each with the selection `node`.
fn page_selection(connection: ItemKind, node: &str) -> String {
    format!(
        "page: {}(first: {PAGE}, after: $after) {{ \
         nodes {{ {node} }} pageInfo {{ hasNextPage endCursor }} }}",
        connection.connection()
    )
}

/// A page of the items of a connection, each with the selection `node`.
fn page_query(connection: ItemKind, node: &str) -> String {
    format!(
        "query($owner: String!, $name: String!, $after: String) {{ \
         repository(owner: $owner, name: $name) {{ {} }} }}",
        page_selection(connection, node)
    )
}

/// A page of the ids of the comments of the item `$number`.
fn ids_query() -> String {
    format!(
        "query($owner: String!, $name: String!, $number: Int!, $after: String) {{ \
         repository(owner: $owner, name: $name) {{ item: issueOrPullRequest(number: $number) {{ {} }} }} }}",
        comment_ids(&format!("first: {PAGE}, after: $after"))
    )
}

/// A query of a check: the first page of the ids of the comments of each
/// item of `items` under its alias, and, for the first query, the counts of
/// the issues and of the pull requests with the page after `$after` of the
/// counts of the comments of the items of the connection `counts`.
fn check_query(counts: Option<ItemKind>, items: &[ItemRef]) -> String {
    let items = items
        .iter()
        .map(|item| {
            format!(
                "{}: issueOrPullRequest(number: {}) {{ {} }}",
                alias(*item),
                item.number,
                comment_ids(&format!("first: {PAGE}"))
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    match counts {
        Some(connection) => format!(
            "query($owner: String!, $name: String!, $after: String) {{ \
             repository(owner: $owner, name: $name) {{ {TOTALS} {} {items} }} }}",
            page_selection(connection, COUNTED)
        ),
        None => format!(
            "query($owner: String!, $name: String!) {{ \
             repository(owner: $owner, name: $name) {{ {items} }} }}"
        ),
    }
}

/// The alias of an item in the query of a check.
fn alias(item: ItemRef) -> String {
    format!("i{}", item.number)
}

/// Take a comment of an item by its ID with
/// `GET /repos/{owner}/{repo}/issues/comments/{id}`: the comment, or the
/// removal of a comment that left its item, `404`, with its `comments/`
/// joined as `write` says.
async fn fetch_comment<K>(
    cx: &FeedCx<K>,
    repo: &RepoId,
    item: ItemRef,
    id: u64,
    write: ListingWrite,
) -> Result<()> {
    let response = cx
        .endpoint(GitHubApi)
        .get(format!("/repos/{repo}/issues/comments/{id}"))
        .send()
        .await?;
    match response.status() {
        200 => {
            let comment = serde_json::from_slice(response.body())
                .map_err(|error| ProviderError::invalid_input(format!("json decode: {error}")))?;
            write_comment(cx, repo, item, write, &comment)
        },
        404 => {
            let comments = comments_path(repo, item);
            cx.remove(format!("{comments}/{id}"))?;
            cx.dir(&comments, write)
        },
        status => Err(ProviderError::from_http_status(status)),
    }
}

impl Ids {
    /// The IDs of an item from their first page, with the comments that the
    /// lists of the answer brought, and the time a comment of the projection
    /// is taken again from.
    fn new(brought: BTreeSet<u64>, since: Option<OffsetDateTime>) -> Self {
        Self {
            page: IdsPage::First,
            seen: BTreeSet::new(),
            brought,
            since,
            fetch: BTreeSet::new(),
        }
    }

    /// Whether a request of the IDs remains after their first page.
    fn pending(&self) -> bool {
        !self.fetch.is_empty() || matches!(self.page, IdsPage::After(_))
    }

    /// Whether the IDs ended: no page and no comment by its ID remains.
    fn ended(&self) -> bool {
        self.fetch.is_empty() && matches!(self.page, IdsPage::End)
    }

    /// Take a page of the IDs of `item`.
    fn take<K>(
        &mut self,
        cx: &FeedCx<K>,
        repo: &RepoId,
        item: ItemRef,
        page: Connection<CommentId>,
    ) -> Result<()> {
        let comments = comments_path(repo, item);
        let known = listed(cx, &comments)?;
        for id in page.nodes {
            self.seen.insert(id.database_id);
            if !self.brought.contains(&id.database_id)
                && (!known.contains(&id.database_id)
                    || self.since.is_some_and(|since| id.updated_at >= since))
            {
                self.fetch.insert(id.database_id);
            }
        }
        if let Some(after) = page.page_info.next()? {
            self.page = IdsPage::After(after);
            return Ok(());
        }
        self.page = IdsPage::End;
        for id in known
            .iter()
            .chain(&self.brought)
            .filter(|id| !self.seen.contains(id))
        {
            cx.remove(format!("{comments}/{id}"))?;
        }
        if self.fetch.is_empty() {
            cx.dir(&comments, ListingWrite::Complete)?;
        }
        Ok(())
    }

    /// The next request of the IDs of `item`: a comment by its ID, else a
    /// page of the IDs. `false` for an item that GitHub GraphQL cannot
    /// resolve: it left the repository.
    async fn step<K>(&mut self, cx: &FeedCx<K>, repo: &RepoId, item: ItemRef) -> Result<bool> {
        if let Some(id) = self.fetch.pop_first() {
            let write = if self.ended() {
                ListingWrite::Complete
            } else {
                ListingWrite::Partial
            };
            fetch_comment(cx, repo, item, id, write).await?;
            return Ok(true);
        }
        let after = match std::mem::replace(&mut self.page, IdsPage::End) {
            IdsPage::First => None,
            IdsPage::After(after) => Some(after),
            IdsPage::End => {
                return Err(ProviderError::invalid_input(format!(
                    "the IDs of the item {} ended",
                    item.number
                )));
            },
        };
        let answer: Item = graphql(
            cx,
            repo,
            &ids_query(),
            json!({ "number": item.number, "after": after }),
        )
        .await?;
        match answer.item {
            Some(commented) => {
                self.take(cx, repo, item, commented.comments)?;
                Ok(true)
            },
            None => Ok(false),
        }
    }
}

// ===========================================================================
// Facts
// ===========================================================================

fn folder(repo: &RepoId, kind: ItemKind, filter: StateFilter) -> String {
    format!("/{repo}/{kind}/{filter}")
}

/// The folder of an item in `filter`: in `all/`, with its facts; in `open/`,
/// the entry that shares them.
fn item_path(repo: &RepoId, item: ItemRef, filter: StateFilter) -> String {
    format!("{}/{}", folder(repo, item.kind, filter), item.number)
}

/// The folder `comments/` of an item, under its folder in `all/`.
fn comments_path(repo: &RepoId, item: ItemRef) -> String {
    format!("{}/comments", item_path(repo, item, StateFilter::All))
}

/// Write the facts of an item from its row: `title`, `body`, `state`, and
/// `user` under `{kind}/all/{number}/`, with its entry in `{kind}/open/` for
/// the state `open`, else the removal of that entry; its folder `comments/`,
/// without comments for a count `comments` of `0`; and its entries in the
/// listings of its folders, which they join as `folders` says.
fn write_item<K>(
    cx: &FeedCx<K>,
    repo: &RepoId,
    row: &ItemRow,
    folders: ListingWrite,
) -> Result<()> {
    let item = row.item();
    cx.dir(folder(repo, item.kind, StateFilter::All), folders)?;
    let at = item_path(repo, item, StateFilter::All);
    cx.dir(&at, ListingWrite::Partial)?;
    cx.file(format!("{at}/title"), row.data.title.as_bytes())?;
    cx.file(format!("{at}/body"), row.data.body_text().as_bytes())?;
    cx.file(format!("{at}/state"), row.data.state.as_bytes())?;
    cx.file(format!("{at}/user"), row.data.login().as_bytes())?;
    let comments = comments_path(repo, item);
    if row.comments == 0 {
        no_comments(cx, &comments)?;
    } else {
        cx.dir(&comments, ListingWrite::Partial)?;
    }
    if row.data.state == "open" {
        cx.dir(folder(repo, item.kind, StateFilter::Open), folders)?;
        cx.dir(
            item_path(repo, item, StateFilter::Open),
            ListingWrite::Partial,
        )?;
    } else {
        cx.remove(item_path(repo, item, StateFilter::Open))?;
    }
    Ok(())
}

/// The folder `comments/` of an item without comments: emptied and complete,
/// as its list would be empty.
fn no_comments<K>(cx: &FeedCx<K>, comments: &str) -> Result<()> {
    for id in listed(cx, comments)? {
        cx.remove(format!("{comments}/{id}"))?;
    }
    cx.dir(comments, ListingWrite::Complete)
}

/// Write the facts of a comment: `author` and `body.md` under
/// `comments/{id}/` of its item, its `comments/` joined as `write` says.
fn write_comment<K>(
    cx: &FeedCx<K>,
    repo: &RepoId,
    item: ItemRef,
    write: ListingWrite,
    comment: &Comment,
) -> Result<()> {
    let comments = comments_path(repo, item);
    cx.dir(&comments, write)?;
    let at = format!("{comments}/{}", comment.id);
    cx.dir(&at, ListingWrite::Partial)?;
    cx.file(format!("{at}/author"), comment.login().as_bytes())?;
    cx.file(format!("{at}/body.md"), comment.body_text().as_bytes())?;
    Ok(())
}

/// Remove an item that left the repository from its folders.
fn remove_item<K>(cx: &FeedCx<K>, repo: &RepoId, item: ItemRef) -> Result<()> {
    for filter in [StateFilter::All, StateFilter::Open] {
        cx.remove(item_path(repo, item, filter))?;
    }
    Ok(())
}

/// The count of the entries the projection lists at `path`, none without a
/// listing.
fn count<K>(cx: &FeedCx<K>, path: &str) -> Result<Option<usize>> {
    let Some(listed) = cx.count(path)? else {
        return Ok(None);
    };
    usize::try_from(listed.count)
        .map(Some)
        .map_err(|error| ProviderError::internal(format!("the listing of {path}: {error}")))
}

/// The numbers the projection lists at `path`, none without a listing.
fn listed<K>(cx: &FeedCx<K>, path: &str) -> Result<BTreeSet<u64>> {
    Ok(cx
        .names(path)?
        .map(|listed| listed.names.into_iter().collect())
        .unwrap_or_default())
}

// ===========================================================================
// The feed of a repository
// ===========================================================================

/// One call of the feed of a repository: its context, the repository, and
/// the lists and the page of the counts of its token, which its requests
/// move.
struct RepoFeed<'a> {
    cx: &'a FeedCx<RepoKey>,
    repo: RepoId,
    rows: Since,
    comments: Since,
    counts: CountsPage,
}

impl Repo {
    /// The feed of the issues, pull requests, and comments of a repository.
    pub(crate) async fn feed(cx: FeedCx<RepoKey>) -> Result<Feed> {
        let repo = RepoId::new(&cx.key().owner, &cx.key().repo);
        let Some(RepoToken {
            rows,
            comments,
            counts,
            stage,
        }) = cx.raw_token().map(decode).transpose()?
        else {
            let mut feed = RepoFeed {
                cx: &cx,
                repo,
                rows: Since::default(),
                comments: Since::default(),
                counts: CountsPage::FIRST,
            };
            return feed.listing_rows(PageRequest::Since).await;
        };
        let mut feed = RepoFeed {
            cx: &cx,
            repo,
            rows,
            comments,
            counts,
        };
        match stage {
            Some(Stage::Rows(page)) => feed.listing_rows(page).await,
            Some(Stage::Comments(page)) => feed.listing_comments(page).await,
            Some(Stage::Counts) => feed.listing_counts().await,
            Some(Stage::Ids { kind, items, ids }) => feed.listing_ids(kind, items, ids).await,
            Some(Stage::Check(check)) => feed.check_step(*check).await,
            None => {
                let check = Check {
                    next: CheckRequest::Rows(PageRequest::Since),
                    changed: BTreeMap::new(),
                    totals: None,
                    removed: BTreeSet::new(),
                    numbered: BTreeSet::new(),
                };
                feed.check_step(check).await
            },
        }
    }
}

impl RepoFeed<'_> {
    /// A page of the rows, `None` when the rows since the time of their list
    /// stand.
    async fn rows_page(&mut self, page: PageRequest) -> Result<Option<Page<ItemRow>>> {
        let list = format!(
            "/repos/{}/issues?state=all&sort=updated&direction=asc&per_page={PAGE}",
            self.repo
        );
        page_since(self.cx, &list, &mut self.rows, page).await
    }

    /// A page of the comments of the repository, `None` when the comments
    /// since the time of their list stand.
    async fn comments_page(&mut self, page: PageRequest) -> Result<Option<Page<CommentRow>>> {
        let list = format!(
            "/repos/{}/issues/comments?sort=updated&direction=asc&per_page={PAGE}",
            self.repo
        );
        page_since(self.cx, &list, &mut self.comments, page).await
    }

    /// A page of the rows of the first full listing, with the inner token of
    /// each item.
    async fn listing_rows(&mut self, page: PageRequest) -> Result<Feed> {
        let page = self
            .rows_page(page)
            .await?
            .ok_or_else(|| unconditional_304("the rows"))?;
        for row in &page.rows {
            write_item(self.cx, &self.repo, row, ListingWrite::Partial)?;
        }
        let stage = match page.next {
            Some(next) => Stage::Rows(next),
            None => Stage::Comments(PageRequest::Since),
        };
        let mut part = self.part(Some(stage))?;
        let key = self.cx.key();
        for row in &page.rows {
            let token = encode(&row.token(None))?;
            let number = row.data.number;
            part = match row.item().kind {
                ItemKind::Issues => part.raw_inner::<Issue>(
                    &IssueKey {
                        owner: key.owner.clone(),
                        repo: key.repo.clone(),
                        filter: Facet(StateFilter::All),
                        number,
                    },
                    token,
                ),
                ItemKind::Pulls => part.raw_inner::<PullRequest>(
                    &PullKey {
                        owner: key.owner.clone(),
                        repo: key.repo.clone(),
                        filter: Facet(StateFilter::All),
                        number,
                    },
                    token,
                ),
            };
        }
        Ok(part.into())
    }

    /// Write the comment of a row under its item in the projection, its
    /// `comments/` as complete as it was; a comment of an item outside the
    /// projection waits for its item.
    fn take_comment(&self, item: ItemRef, comment: &Comment) -> Result<()> {
        if count(self.cx, &comments_path(&self.repo, item))?.is_none() {
            return Ok(());
        }
        write_comment(self.cx, &self.repo, item, ListingWrite::Join, comment)
    }

    /// A page of the comments of the first full listing.
    async fn listing_comments(&mut self, page: PageRequest) -> Result<Feed> {
        let page = self
            .comments_page(page)
            .await?
            .ok_or_else(|| unconditional_304("the comments"))?;
        for row in &page.rows {
            self.take_comment(row.item()?, &row.comment)?;
        }
        let stage = match page.next {
            Some(next) => Stage::Comments(next),
            None => Stage::Counts,
        };
        Ok(self.part(Some(stage))?.into())
    }

    /// A page of the counts of the first full listing: the `comments/` of an
    /// item with the count of its `comments/` in the projection becomes
    /// complete, and the items with another count take their IDs next.
    async fn listing_counts(&mut self) -> Result<Feed> {
        let connection = self.counts.connection;
        let Paged::<Counted> { page } = graphql(
            self.cx,
            &self.repo,
            &page_query(connection, COUNTED),
            json!({ "after": self.counts.cursor }),
        )
        .await?;
        let mut items = BTreeSet::new();
        for node in page.nodes {
            let item = ItemRef {
                kind: connection,
                number: node.number,
            };
            let comments = comments_path(&self.repo, item);
            // An item made after the rows is outside the projection.
            let Some(count) = count(self.cx, &comments)? else {
                continue;
            };
            if count == node.comments.total_count {
                self.cx.dir(&comments, ListingWrite::Complete)?;
            } else {
                items.insert(node.number);
            }
        }
        self.counts = self.counts.after(page.page_info.next()?);
        self.listing_ids_part(connection, items)
    }

    /// The next request of the IDs of the first of `items` in the first full
    /// listing, without the comments by their IDs that an answer of the item
    /// brought since their page; an item that GitHub GraphQL cannot resolve
    /// leaves the repository.
    async fn listing_ids(
        &mut self,
        kind: ItemKind,
        mut items: BTreeSet<u64>,
        mut ids: Ids,
    ) -> Result<Feed> {
        let number = *items.first().ok_or_else(|| {
            ProviderError::invalid_input("the first full listing takes the IDs of no item")
        })?;
        let item = ItemRef { kind, number };
        let comments = comments_path(&self.repo, item);
        let known = listed(self.cx, &comments)?;
        ids.fetch.retain(|id| !known.contains(id));
        if ids.ended() {
            self.cx.dir(&comments, ListingWrite::Complete)?;
        } else if !ids.step(self.cx, &self.repo, item).await? {
            remove_item(self.cx, &self.repo, item)?;
        }
        if !ids.ended() {
            return Ok(self.part(Some(Stage::Ids { kind, items, ids }))?.into());
        }
        items.remove(&number);
        self.listing_ids_part(kind, items)
    }

    /// A part of the first full listing before the IDs of `items`, from
    /// their first page; without items, before the next page of the counts;
    /// after the last page of the counts, the last part, which completes the
    /// four folders.
    fn listing_ids_part(&self, kind: ItemKind, items: BTreeSet<u64>) -> Result<Feed> {
        let stage = if !items.is_empty() {
            Some(Stage::Ids {
                kind,
                items,
                ids: Ids::new(BTreeSet::new(), None),
            })
        } else if self.counts == CountsPage::FIRST {
            for kind in [ItemKind::Issues, ItemKind::Pulls] {
                for filter in [StateFilter::All, StateFilter::Open] {
                    self.cx
                        .dir(folder(&self.repo, kind, filter), ListingWrite::Complete)?;
                }
            }
            None
        } else {
            Some(Stage::Counts)
        };
        Ok(self.part(stage)?.into())
    }

    /// The next request of a check, a part each; a `304` of the rows or of
    /// the comments goes on to the request after it.
    async fn check_step(&mut self, mut check: Check) -> Result<Feed> {
        loop {
            match std::mem::replace(&mut check.next, CheckRequest::Counts) {
                CheckRequest::Rows(page) => {
                    let Some(page) = self.rows_page(page).await? else {
                        check.next = CheckRequest::Comments(PageRequest::Since);
                        continue;
                    };
                    for row in &page.rows {
                        write_item(self.cx, &self.repo, row, ListingWrite::Complete)?;
                        let item = row.item();
                        check
                            .changed
                            .insert(item.number, Changed::new(item.kind, row.comments));
                    }
                    check.next = match page.next {
                        Some(next) => CheckRequest::Rows(next),
                        None => CheckRequest::Comments(PageRequest::Since),
                    };
                    return self.check_part(check);
                },
                CheckRequest::Comments(page) => {
                    let Some(page) = self.comments_page(page).await? else {
                        continue;
                    };
                    for row in &page.rows {
                        let item = row.item()?;
                        match check.changed.get_mut(&item.number) {
                            // Its row emptied its `comments/`: the comment
                            // changed the item after the row, so the next
                            // check takes it with the IDs of the item.
                            Some(Changed { comments: 0, .. }) => {},
                            Some(changed) => {
                                write_comment(
                                    self.cx,
                                    &self.repo,
                                    item,
                                    ListingWrite::Partial,
                                    &row.comment,
                                )?;
                                changed.ids.brought.insert(row.comment.id);
                            },
                            None => self.take_comment(item, &row.comment)?,
                        }
                    }
                    if let Some(next) = page.next {
                        check.next = CheckRequest::Comments(next);
                        return self.check_part(check);
                    }
                },
                CheckRequest::Counts => self.take_counts(&mut check).await?,
                CheckRequest::Ids { number } => {
                    let changed = check.changed.get_mut(&number).ok_or_else(|| {
                        ProviderError::invalid_input(format!("item {number} is not a changed item"))
                    })?;
                    let item = ItemRef {
                        kind: changed.kind,
                        number,
                    };
                    if !changed.ids.step(self.cx, &self.repo, item).await? {
                        self.remove_changed(&mut check, item)?;
                    }
                },
                CheckRequest::Numbers {
                    kind,
                    after,
                    mut numbers,
                } => {
                    let Paged::<Numbered> { page } = graphql(
                        self.cx,
                        &self.repo,
                        &page_query(kind, NUMBERED),
                        json!({ "after": after }),
                    )
                    .await?;
                    numbers.extend(page.nodes.iter().map(|node| node.number));
                    if let Some(after) = page.page_info.next()? {
                        check.next = CheckRequest::Numbers {
                            kind,
                            after: Some(after),
                            numbers,
                        };
                        return self.check_part(check);
                    }
                    check.numbered.insert(kind);
                    let listed = listed(self.cx, &folder(&self.repo, kind, StateFilter::All))?;
                    let changed = check
                        .changed
                        .iter()
                        .filter(|(_, changed)| changed.kind == kind)
                        .map(|(number, _)| *number);
                    for number in listed.iter().copied().chain(changed) {
                        if !numbers.contains(&number) {
                            remove_item(self.cx, &self.repo, ItemRef { kind, number })?;
                        }
                    }
                    numbers.retain(|number| {
                        !listed.contains(number) && !check.changed.contains_key(number)
                    });
                    if !numbers.is_empty() {
                        check.next = CheckRequest::Items { kind, numbers };
                        return self.check_part(check);
                    }
                },
                CheckRequest::Items { kind, mut numbers } => {
                    let number = numbers.pop_first().ok_or_else(|| {
                        ProviderError::invalid_input("the check takes no item by its number")
                    })?;
                    let item = ItemRef { kind, number };
                    match item_answer(self.cx, &self.repo, item, None).await? {
                        ItemAnswer::Found(row, _) => {
                            write_item(self.cx, &self.repo, &row, ListingWrite::Complete)?;
                            check
                                .changed
                                .insert(number, Changed::new(kind, row.comments));
                        },
                        // It left the repository after its number.
                        ItemAnswer::Removed => {},
                        ItemAnswer::Unchanged => return Err(unconditional_304("the item")),
                    }
                    if !numbers.is_empty() {
                        check.next = CheckRequest::Items { kind, numbers };
                        return self.check_part(check);
                    }
                },
            }
            break;
        }
        match self.after(&check)? {
            Some(next) => {
                check.next = next;
                self.check_part(check)
            },
            None => Ok(self.part(None)?.into()),
        }
    }

    /// The `POST` of a check: the first page of the IDs of the next changed
    /// items with comments whose IDs it lacks, a page of items, and, while
    /// the check lacks the counts, the counts with the page of the counts of
    /// the comments of the items. Each asked item takes its page of IDs, or
    /// leaves when GitHub GraphQL cannot resolve it.
    async fn take_counts(&mut self, check: &mut Check) -> Result<()> {
        let asked = check
            .changed
            .iter()
            .filter(|(_, changed)| {
                changed.comments != 0 && matches!(changed.ids.page, IdsPage::First)
            })
            .take(PAGE)
            .map(|(number, changed)| ItemRef {
                kind: changed.kind,
                number: *number,
            })
            .collect::<Vec<_>>();
        let mut items = if check.totals.is_some() {
            graphql::<BTreeMap<String, Option<Commented>>>(
                self.cx,
                &self.repo,
                &check_query(None, &asked),
                json!({}),
            )
            .await?
        } else {
            let counts: Counts = graphql(
                self.cx,
                &self.repo,
                &check_query(Some(self.counts.connection), &asked),
                json!({ "after": self.counts.cursor }),
            )
            .await?;
            check.totals = Some(Totals {
                issues: counts.issues.total_count,
                pulls: counts.pull_requests.total_count,
            });
            self.take_counted(check, counts.page)?;
            counts.items
        };
        for item in asked {
            match items.remove(&alias(item)).ok_or_else(|| {
                ProviderError::invalid_input(format!("the counts lack the item {}", item.number))
            })? {
                Some(commented) => {
                    let changed = check.changed.get_mut(&item.number).ok_or_else(|| {
                        ProviderError::internal(format!(
                            "item {} is not a changed item",
                            item.number
                        ))
                    })?;
                    changed
                        .ids
                        .take(self.cx, &self.repo, item, commented.comments)?;
                },
                None => self.remove_changed(check, item)?,
            }
        }
        Ok(())
    }

    /// The page of the counts of the comments of the items of a check: an
    /// item of the projection outside the changed items with another count
    /// than its `comments/` becomes a changed item, which takes its IDs, so
    /// that a comment that left an item without a change of its row leaves.
    /// The next check compares the page after it.
    fn take_counted(&mut self, check: &mut Check, page: Connection<Counted>) -> Result<()> {
        let kind = self.counts.connection;
        for node in page.nodes {
            if check.changed.contains_key(&node.number) {
                continue;
            }
            let item = ItemRef {
                kind,
                number: node.number,
            };
            let comments = comments_path(&self.repo, item);
            // An item outside the projection comes with the numbers of its
            // kind.
            let Some(count) = count(self.cx, &comments)? else {
                continue;
            };
            if count == node.comments.total_count {
                continue;
            }
            if node.comments.total_count == 0 {
                no_comments(self.cx, &comments)?;
            }
            check
                .changed
                .insert(node.number, Changed::new(kind, node.comments.total_count));
        }
        self.counts = self.counts.after(page.page_info.next()?);
        Ok(())
    }

    /// The removal of a changed item that GitHub GraphQL cannot resolve.
    fn remove_changed(&self, check: &mut Check, item: ItemRef) -> Result<()> {
        remove_item(self.cx, &self.repo, item)?;
        check.removed.insert(item);
        let changed = check.changed.get_mut(&item.number).ok_or_else(|| {
            ProviderError::internal(format!("item {} is not a changed item", item.number))
        })?;
        changed.ids.page = IdsPage::End;
        Ok(())
    }

    /// The request of a check after its rows and comments: the next request
    /// of the IDs of a changed item, the counts while the check lacks them
    /// or the first page of the IDs of a changed item with comments, the
    /// numbers of a kind with another count, or none at its end.
    fn after(&self, check: &Check) -> Result<Option<CheckRequest>> {
        if let Some((number, _)) = check
            .changed
            .iter()
            .find(|(_, changed)| changed.ids.pending())
        {
            return Ok(Some(CheckRequest::Ids { number: *number }));
        }
        if check.totals.is_none()
            || check
                .changed
                .values()
                .any(|changed| changed.comments != 0 && matches!(changed.ids.page, IdsPage::First))
        {
            return Ok(Some(CheckRequest::Counts));
        }
        self.numbers(check)
    }

    /// The first page of the numbers of the first kind, of those whose
    /// numbers the check did not take, whose count `totalCount` is other
    /// than the count of `{kind}/all/` in the projection with the new items
    /// of the check and without the items that GitHub GraphQL removed; none
    /// for the same counts.
    fn numbers(&self, check: &Check) -> Result<Option<CheckRequest>> {
        let totals = check
            .totals
            .ok_or_else(|| ProviderError::internal("a check compares counts without them"))?;
        for kind in [ItemKind::Issues, ItemKind::Pulls] {
            if check.numbered.contains(&kind) {
                continue;
            }
            let all = folder(&self.repo, kind, StateFilter::All);
            let listed = count(self.cx, &all)?.ok_or_else(|| {
                ProviderError::internal(format!("the projection has no listing of {all}"))
            })?;
            let mut new = 0;
            for (number, changed) in &check.changed {
                let item = ItemRef {
                    kind: changed.kind,
                    number: *number,
                };
                if changed.kind == kind
                    && count(self.cx, &item_path(&self.repo, item, StateFilter::All))?.is_none()
                {
                    new += 1;
                }
            }
            let gone = check
                .removed
                .iter()
                .filter(|item| item.kind == kind)
                .count();
            let total = match kind {
                ItemKind::Issues => totals.issues,
                ItemKind::Pulls => totals.pulls,
            };
            if listed + new - gone != total {
                return Ok(Some(CheckRequest::Numbers {
                    kind,
                    after: None,
                    numbers: BTreeSet::new(),
                }));
            }
        }
        Ok(None)
    }

    fn check_part(&self, check: Check) -> Result<Feed> {
        Ok(self.part(Some(Stage::Check(Box::new(check))))?.into())
    }

    /// A part of the feed with the lists and the page of the counts as the
    /// requests of the call left them and `stage`, followed by more parts
    /// while a stage remains.
    fn part(&self, stage: Option<Stage>) -> Result<FeedPart> {
        let more = stage.is_some();
        Ok(Feed::raw_part(encode(&RepoToken {
            rows: self.rows.clone(),
            comments: self.comments.clone(),
            counts: self.counts.clone(),
            stage,
        })?)
        .more(more))
    }
}

fn unconditional_304(what: &str) -> ProviderError {
    ProviderError::internal(format!("{what} answered 304 to a request without an ETag"))
}

// ===========================================================================
// The feed of an item
// ===========================================================================

impl Issue {
    /// The feed of an issue before the first full listing of its repository
    /// ends.
    pub(crate) async fn feed(cx: FeedCx<IssueKey>) -> Result<Feed> {
        let key = cx.key();
        let item = ItemRef {
            kind: ItemKind::Issues,
            number: key.number,
        };
        item_feed(&cx, &RepoId::new(&key.owner, &key.repo), item).await
    }
}

impl PullRequest {
    /// The feed of a pull request before the first full listing of its
    /// repository ends.
    pub(crate) async fn feed(cx: FeedCx<PullKey>) -> Result<Feed> {
        let key = cx.key();
        let item = ItemRef {
            kind: ItemKind::Pulls,
            number: key.number,
        };
        item_feed(&cx, &RepoId::new(&key.owner, &key.repo), item).await
    }
}

/// The feed of an item: the next request of an unfinished answer; else, from
/// its `comments/`, the pages of its comments; else its answer, and for a
/// change of an item with its comments in the projection and a count
/// `comments` other than `0`, the comments since the `updated_at` of its token
/// and its IDs. Its first answer, which the lookup of its folder starts as
/// each operation resolves its path from the root, takes its answer alone.
/// Each answer makes a part of each request.
async fn item_feed<K>(cx: &FeedCx<K>, repo: &RepoId, item: ItemRef) -> Result<Feed> {
    let token = cx
        .raw_token()
        .map(decode::<Option<ItemToken>>)
        .transpose()?
        .flatten();
    if let Some(mut token) = token {
        if let Some(next) = token.next.take() {
            return item_step(cx, repo, item, token, next).await;
        }
        if cx.start() == Some("comments") {
            return item_comments_of(cx, repo, item, token).await;
        }
        return match item_answer(cx, repo, item, Some(&token)).await? {
            ItemAnswer::Unchanged => Ok(Feed::unchanged()),
            ItemAnswer::Removed => removal(cx, repo, item),
            ItemAnswer::Found(row, etag) => {
                write_found(cx, repo, item, &row)?;
                let mut next = row.token(etag);
                let complete = cx
                    .count(comments_path(repo, item))?
                    .is_some_and(|listed| listed.complete);
                if row.comments != 0 && complete {
                    next.next = Some(ItemRequest::Since {
                        since: token.updated_at,
                        link: None,
                        brought: BTreeSet::new(),
                    });
                }
                item_part(&next)
            },
        };
    }
    match item_answer(cx, repo, item, None).await? {
        ItemAnswer::Found(row, etag) => {
            write_found(cx, repo, item, &row)?;
            item_part(&row.token(etag))
        },
        ItemAnswer::Removed => removal(cx, repo, item),
        ItemAnswer::Unchanged => Err(unconditional_304("the item")),
    }
}

/// The comments of an item from its `comments/`: none for the count
/// `comments` `0`, else the pages of its comments, a part each.
async fn item_comments_of<K>(
    cx: &FeedCx<K>,
    repo: &RepoId,
    item: ItemRef,
    token: ItemToken,
) -> Result<Feed> {
    if token.comments == 0 {
        no_comments(cx, &comments_path(repo, item))?;
        return item_part(&token);
    }
    let next = ItemRequest::Comments {
        link: None,
        brought: BTreeSet::new(),
    };
    item_step(cx, repo, item, token, next).await
}

/// One request of an unfinished answer of an item, a part with the next
/// request in its token, or its last part.
async fn item_step<K>(
    cx: &FeedCx<K>,
    repo: &RepoId,
    item: ItemRef,
    mut token: ItemToken,
    request: ItemRequest,
) -> Result<Feed> {
    match take_item_request(cx, repo, item, &token, request).await? {
        ItemStep::Next(request) => {
            token.next = Some(request);
            item_part(&token)
        },
        ItemStep::Done => item_part(&token),
        ItemStep::Left => removal(cx, repo, item),
    }
}

/// What a request of the comments of an item leaves: its next request, its
/// end, or the item gone from the repository.
enum ItemStep {
    Next(ItemRequest),
    Done,
    Left,
}

/// One request of the comments of an item: a page of its comments since a
/// time, then its IDs; a page of all of its comments, a complete
/// `comments/` without the comments they lack for the count of its token,
/// else its IDs; a request of its IDs.
async fn take_item_request<K>(
    cx: &FeedCx<K>,
    repo: &RepoId,
    item: ItemRef,
    token: &ItemToken,
    request: ItemRequest,
) -> Result<ItemStep> {
    let comments = comments_path(repo, item);
    match request {
        ItemRequest::Since {
            since,
            link,
            mut brought,
        } => {
            let (rows, next) = item_comments(cx, repo, item, Some(since), link).await?;
            for comment in &rows {
                write_comment(cx, repo, item, ListingWrite::Partial, comment)?;
                brought.insert(comment.id);
            }
            Ok(ItemStep::Next(match next {
                Some(link) => ItemRequest::Since {
                    since,
                    link: Some(link),
                    brought,
                },
                None => ItemRequest::Ids(Ids::new(brought, Some(since))),
            }))
        },
        ItemRequest::Comments { link, mut brought } => {
            let (rows, next) = item_comments(cx, repo, item, None, link).await?;
            for comment in &rows {
                write_comment(cx, repo, item, ListingWrite::Partial, comment)?;
                brought.insert(comment.id);
            }
            if let Some(link) = next {
                return Ok(ItemStep::Next(ItemRequest::Comments {
                    link: Some(link),
                    brought,
                }));
            }
            if brought.len() != token.comments {
                return Ok(ItemStep::Next(ItemRequest::Ids(Ids::new(
                    brought,
                    Some(token.updated_at),
                ))));
            }
            cx.dir(&comments, ListingWrite::Complete)?;
            for id in listed(cx, &comments)?.difference(&brought) {
                cx.remove(format!("{comments}/{id}"))?;
            }
            Ok(ItemStep::Done)
        },
        ItemRequest::Ids(mut ids) => {
            if !ids.step(cx, repo, item).await? {
                return Ok(ItemStep::Left);
            }
            Ok(if ids.ended() {
                ItemStep::Done
            } else {
                ItemStep::Next(ItemRequest::Ids(ids))
            })
        },
    }
}

/// A part of the feed of an item with `token`, followed by more parts while
/// its answer has a next request.
fn item_part(token: &ItemToken) -> Result<Feed> {
    Ok(Feed::raw_part(encode(&Some(token))?)
        .more(token.next.is_some())
        .into())
}

/// The facts of the answer of an item, not found for an item of the other
/// kind.
fn write_found<K>(cx: &FeedCx<K>, repo: &RepoId, item: ItemRef, row: &ItemRow) -> Result<()> {
    if row.item() != item {
        return Err(ProviderError::not_found(format!(
            "{}#{} is not in {}",
            repo, item.number, item.kind
        )));
    }
    write_item(cx, repo, row, ListingWrite::Partial)
}

/// The removal of an item from its folders, with the token of a removed
/// item.
fn removal<K>(cx: &FeedCx<K>, repo: &RepoId, item: ItemRef) -> Result<Feed> {
    remove_item(cx, repo, item)?;
    Ok(Feed::raw_part(encode(&None::<ItemToken>)?).into())
}
