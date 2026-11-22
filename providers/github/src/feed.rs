//! The feeds of the issues, pull requests, and comments of a repository.
//!
//! The issues and the pull requests come from GitHub GraphQL, each kind in
//! the order of `updatedAt` by its cursor, which names the last node that an
//! answer took: a check asks for the nodes after it, so that it takes each
//! changed item once and none twice, without a time. The comments come from
//! the REST list of the comments of the repository in the order of
//! `updated_at`.
//!
//! The first full listing takes the issues, then the pull requests, then
//! every comment, then the GitHub GraphQL counts of the comments of each item,
//! with the IDs of an item with another count, then the REST count of the
//! comments of the repository. After it, each answer, which the host asks
//! for once in each sync, is a check:
//!
//! - one `POST` with the nodes of each kind after its cursor and the counts of
//!   the issues and of the pull requests;
//! - the REST count of the comments of the repository with its newest
//!   comment;
//! - the comments updated since a second before the last one, so that the
//!   check takes each change whether GitHub's `since` includes its second or
//!   not;
//! - the IDs of each changed item whose comments are other than its count;
//! - the numbers of a kind whose count is other than the projection's with
//!   the changes of the check, which remove each item that left and bring
//!   each item that the projection lacks;
//! - the counts of the comments of the items, when the REST count is other
//!   than the count the projection holds with the changes of the check, until
//!   each item whose comments changed unseen, as by a deletion, took its IDs.
//!
//! Before the first full listing ends, the feed of an item answers for that
//! item. Each request is a part. An item whose answer names another number
//! or another repository, as an issue that moved, left this repository.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use omnifs_sdk::prelude::*;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;
use serde_json::value::RawValue;
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};

use crate::api::GitHubApi;
use crate::item::{IssueKey, ItemKind, PullKey, RepoKey};
use crate::objects::{Comment, Issue, ItemData, PullRequest, Repo};
use crate::{RepoId, StateFilter};

/// The nodes and the rows of a page.
const PAGE: usize = 100;

// ===========================================================================
// Tokens
// ===========================================================================

/// The token of the feed of a repository: the cursor of the last issue and
/// of the last pull request that it took, the list of its comments, the REST
/// count of the comments of the repository that the projection holds, and,
/// while its first full listing or a check is unfinished, its stage.
#[derive(Serialize, Deserialize)]
struct RepoToken {
    issues: Option<String>,
    pulls: Option<String>,
    comments: Since,
    total: Option<u64>,
    stage: Option<Stage>,
}

/// The list of the comments of the repository: the `updated_at` of its last
/// row, and the `ETag` of the answer since that time that had no page after
/// it.
#[derive(Clone, Default, Serialize, Deserialize)]
struct Since {
    #[serde(with = "time::serde::rfc3339::option")]
    time: Option<OffsetDateTime>,
    etag: Option<String>,
}

/// A page of the counts of the comments of the items of a kind, after a
/// cursor of GitHub GraphQL.
#[derive(Clone, Serialize, Deserialize)]
struct CountsAt {
    kind: ItemKind,
    after: Option<String>,
}

impl CountsAt {
    /// The page after this one, whose cursor is `next`: the first page of the
    /// pull requests after the last of the issues, none after the last of the
    /// pull requests.
    fn after(self, next: Option<String>) -> Option<Self> {
        match (next, self.kind) {
            (Some(after), kind) => Some(Self {
                kind,
                after: Some(after),
            }),
            (None, ItemKind::Issues) => Some(Self {
                kind: ItemKind::Pulls,
                after: None,
            }),
            (None, ItemKind::Pulls) => None,
        }
    }
}

/// The stage of an unfinished first full listing or check.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Stage {
    /// The next page of the items of a kind of the first full listing.
    Items { kind: ItemKind },
    /// A page of the comments of the first full listing, at a link `next`.
    Comments { link: Option<String> },
    /// A page of the counts of the first full listing.
    Counts(CountsAt),
    /// The IDs of the items of a page of the counts with another count than
    /// their `comments/`, the first of `items` at a time, then the page of
    /// the counts `then`, or the REST count without one.
    Ids {
        kind: ItemKind,
        items: BTreeSet<u64>,
        ids: Ids,
        then: Option<CountsAt>,
    },
    /// The REST count of the comments at the end of the first full listing.
    Total,
    /// A check, at its next request.
    Check(Box<Check>),
}

/// An unfinished check: its next request, each item that it wrote by its
/// number, the counts of the last page of each kind, the items of the
/// projection that it removed, the kinds whose numbers it took, the changes
/// of the comments that the projection holds, the REST count, and the scan
/// of the counts of the comments.
#[derive(Serialize, Deserialize)]
struct Check {
    next: CheckRequest,
    changed: BTreeMap<u64, Changed>,
    totals: BTreeMap<ItemKind, u64>,
    removed: BTreeSet<ItemRef>,
    numbered: BTreeSet<ItemKind>,
    ledger: Ledger,
    remote: Option<Remote>,
    scan: Scan,
}

/// An item that a check wrote: its kind, the count of its comments, whether
/// the projection lacked it, whether it left since, and its IDs.
#[derive(Serialize, Deserialize)]
struct Changed {
    kind: ItemKind,
    comments: u64,
    new: bool,
    gone: bool,
    ids: Ids,
}

/// The comments that a check added to the projection and removed from it,
/// each by its ID with the number of its item.
#[derive(Default, Serialize, Deserialize)]
struct Ledger {
    added: BTreeMap<u64, u64>,
    removed: BTreeMap<u64, u64>,
}

/// The REST count of the comments of the repository, with the ID of its
/// newest comment: a comment with a greater ID came after the count.
#[derive(Clone, Copy, Serialize, Deserialize)]
struct Remote {
    total: u64,
    newest: Option<u64>,
}

/// The scan of the counts of the comments of the items of a check: not
/// begun, at its next page, or at its end.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Scan {
    Idle,
    At(CountsAt),
    Done,
}

/// The next request of a check.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CheckRequest {
    /// The `POST` of the nodes of `kinds` after their cursors.
    Rows { kinds: BTreeSet<ItemKind> },
    /// The REST count of the comments.
    Remote,
    /// A page of the comments updated since `since`, at a link `next`, with
    /// the IDs of the rows before it and the `updated_at` of the last row.
    Comments {
        #[serde(with = "time::serde::rfc3339::option")]
        since: Option<OffsetDateTime>,
        link: Option<String>,
        seen: BTreeSet<u64>,
        #[serde(with = "time::serde::rfc3339::option")]
        latest: Option<OffsetDateTime>,
    },
    /// The `POST` of the first page of the IDs of the next changed items.
    Counts,
    /// The next request of the IDs of a changed item.
    Ids { number: u64 },
    /// A page of the numbers of a kind, with the numbers before it as bits.
    Numbers {
        kind: ItemKind,
        after: Option<String>,
        seen: Vec<u64>,
    },
    /// The `POST` of the items of a kind that the projection lacks, by their
    /// numbers, a page at a time.
    Items {
        kind: ItemKind,
        numbers: BTreeSet<u64>,
    },
    /// A page of the counts of the comments of the items, for a REST count
    /// other than the count the projection holds with the changes of the
    /// check.
    Scan(CountsAt),
}

/// The token of the feed of an item: its `updatedAt`, the count of its
/// comments, and the next request of an unfinished answer. The token of a
/// removed item is `null`.
#[derive(Serialize, Deserialize)]
struct ItemToken {
    #[serde(with = "time::serde::rfc3339")]
    updated_at: OffsetDateTime,
    comments: u64,
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
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
struct ItemRef {
    kind: ItemKind,
    number: u64,
}

// ===========================================================================
// REST
// ===========================================================================

/// A row of a REST list of comments of the repository.
struct CommentRow {
    comment: Comment,
    bytes: Box<RawValue>,
    item: ItemRef,
    updated_at: OffsetDateTime,
}

/// The fields of a comment beside those of `Comment` that a list reads.
#[derive(Deserialize)]
struct CommentFields {
    html_url: String,
    #[serde(with = "time::serde::rfc3339")]
    updated_at: OffsetDateTime,
}

impl CommentRow {
    fn of(bytes: Box<RawValue>) -> Result<Self> {
        let comment: Comment = decode_json(bytes.get())?;
        let fields: CommentFields = decode_json(bytes.get())?;
        let item = comment_item(&fields.html_url)?;
        Ok(Self {
            comment,
            bytes,
            item,
            updated_at: fields.updated_at,
        })
    }
}

/// The item of a comment, from the kind and the number of its `html_url`,
/// `https://github.com/{owner}/{repo}/{issues|pull}/{number}#issuecomment-{id}`.
fn comment_item(html_url: &str) -> Result<ItemRef> {
    let invalid = || ProviderError::invalid_input(format!("comment at {html_url}"));
    let (path, _) = html_url.split_once('#').ok_or_else(invalid)?;
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

fn decode_json<T: DeserializeOwned>(text: &str) -> Result<T> {
    serde_json::from_str(text)
        .map_err(|error| ProviderError::invalid_input(format!("json decode: {error}")))
}

/// The answer of a REST page.
enum RestPage {
    /// `304`: the page of the `ETag` stands.
    Unchanged,
    /// `200`: its rows, its `ETag`, and its link `next` as a path of the API.
    Rows {
        rows: Vec<Box<RawValue>>,
        etag: Option<String>,
        next: Option<String>,
        last: Option<String>,
    },
    /// `301`, `404`, or `410`: its resource is not in the repository.
    Gone,
}

/// Ask for the REST page at `path`, with `If-None-Match` and `etag`. The
/// redirect of a resource that moved, as a transferred issue or a renamed
/// repository, is not followed: what answers at another URL is not of this
/// repository.
async fn rest_page(cx: &Cx, path: &str, etag: Option<&str>) -> Result<RestPage> {
    let request = cx.endpoint(GitHubApi).get(path);
    let request = match etag {
        Some(etag) => request.header("If-None-Match", etag),
        None => request,
    };
    let response = request.send().await?;
    match response.status() {
        200 => Ok(RestPage::Rows {
            rows: decode_json(
                std::str::from_utf8(response.body())
                    .map_err(|error| ProviderError::invalid_input(format!("utf-8: {error}")))?,
            )?,
            etag: response.header("etag").map(ToOwned::to_owned),
            next: link(response.header("link"), "next")?,
            last: link(response.header("link"), "last")?,
        }),
        304 => Ok(RestPage::Unchanged),
        301 | 404 | 410 => Ok(RestPage::Gone),
        status => Err(error_of(&response, status)),
    }
}

fn error_of(response: &HttpResponse, status: u16) -> ProviderError {
    ProviderError::from_http_answer(
        status,
        response
            .headers()
            .iter()
            .filter_map(|(name, value)| Some((name.as_str(), value.to_str().ok()?))),
    )
}

/// The link of relation `rel` of a header `Link`, as a path of the API.
fn link(header: Option<&str>, rel: &str) -> Result<Option<String>> {
    let wanted = format!(r#"rel="{rel}""#);
    let Some(url) = header.and_then(|links| {
        links.split(',').find_map(|link| {
            let (url, params) = link.split_once(';')?;
            params
                .split(';')
                .any(|param| param.trim() == wanted)
                .then(|| url.trim().trim_start_matches('<').trim_end_matches('>'))
        })
    }) else {
        return Ok(None);
    };
    url.strip_prefix(GitHubApi.base())
        .map(|path| Some(path.to_owned()))
        .ok_or_else(|| ProviderError::invalid_input(format!("link {rel} {url} leaves the API")))
}

/// The repository as gone from its path: it moved, or GitHub refuses it.
fn moved(repo: &RepoId) -> ProviderError {
    ProviderError::not_found(format!("{repo} is not at this path"))
}

/// The REST count of the comments of the repository with its newest
/// comment: one row a page, the newest first, whose last page is the count.
async fn remote(cx: &Cx, repo: &RepoId) -> Result<Remote> {
    #[derive(Deserialize)]
    struct Id {
        id: u64,
    }
    let path = format!("/repos/{repo}/issues/comments?sort=created&direction=desc&per_page=1");
    let RestPage::Rows {
        rows, next, last, ..
    } = rest_page(cx, &path, None).await?
    else {
        return Err(moved(repo));
    };
    let newest = match rows.first() {
        Some(row) => Some(decode_json::<Id>(row.get())?.id),
        None => None,
    };
    let total = match (next, last) {
        (None, None) => rows.len() as u64,
        (Some(_), Some(last)) => page_of(&last)?,
        (_, last) => {
            return Err(ProviderError::invalid_input(format!(
                "the comments of {repo} have the link last {last:?} with another link next"
            )));
        },
    };
    Ok(Remote { total, newest })
}

/// The value of the query parameter `page` of a path.
fn page_of(path: &str) -> Result<u64> {
    path.split_once('?')
        .into_iter()
        .flat_map(|(_, query)| query.split('&'))
        .find_map(|pair| pair.strip_prefix("page="))
        .and_then(|page| page.parse().ok())
        .ok_or_else(|| ProviderError::invalid_input(format!("{path} names no page")))
}

/// The query pair `since` of a second before `time`. GitHub gives each time
/// to the second, and `since` takes a time to the second: the second before
/// that of `time` is the one unit of GitHub's own precision that makes the
/// rows of the second of `time` come whether `since` includes its second or
/// not.
fn since_pair(time: Option<OffsetDateTime>) -> Result<String> {
    match time {
        Some(time) => {
            let since = time - Duration::SECOND;
            let text = since
                .format(&Rfc3339)
                .map_err(|error| ProviderError::internal(format!("time {since}: {error}")))?;
            Ok(format!("&since={text}"))
        },
        None => Ok(String::new()),
    }
}

// ===========================================================================
// GitHub GraphQL
// ===========================================================================

#[derive(Deserialize)]
struct GraphQl {
    data: Option<GraphQlData>,
    errors: Option<Vec<GraphQlError>>,
}

#[derive(Deserialize)]
struct GraphQlError {
    #[serde(rename = "type")]
    kind: Option<String>,
    message: String,
}

/// The data of an answer: the repository, whose fields the query selects.
#[derive(Deserialize)]
struct GraphQlData {
    repository: Option<Box<RawValue>>,
}

/// The fields of a repository by their aliases, `null` for an item that
/// GitHub GraphQL cannot resolve.
type Aliased = BTreeMap<String, Option<Box<RawValue>>>;

/// The node under `alias` of an answer, `None` for `null`.
fn aliased<T: DeserializeOwned>(fields: &mut Aliased, alias: &str) -> Result<Option<T>> {
    fields
        .remove(alias)
        .ok_or_else(|| ProviderError::invalid_input(format!("GitHub GraphQL lacks {alias}")))?
        .map(|node| decode_json(node.get()))
        .transpose()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Connection<T> {
    nodes: Vec<T>,
    page_info: PageInfo,
}

/// A page of the nodes of a kind in the order of `updatedAt`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Rows {
    total_count: u64,
    nodes: Vec<Box<RawValue>>,
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
    fn next(&self) -> Result<Option<String>> {
        if !self.has_next_page {
            return Ok(None);
        }
        self.end_cursor.clone().map(Some).ok_or_else(|| {
            ProviderError::invalid_input("a GraphQL page before another has no cursor")
        })
    }
}

/// A page of a connection under the alias `page`.
#[derive(Deserialize)]
struct Paged<T> {
    page: Connection<T>,
}

#[derive(Deserialize)]
struct Counted {
    number: u64,
    comments: crate::objects::Total,
}

#[derive(Deserialize)]
struct Numbered {
    number: u64,
}

/// The number and the repository of a node, which tell an item of this
/// repository from one that GitHub answers for its number from elsewhere.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Identity {
    number: u64,
    repository: Named,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Named {
    name_with_owner: String,
}

impl Identity {
    fn of(&self, repo: &RepoId, number: u64) -> bool {
        self.number == number
            && self
                .repository
                .name_with_owner
                .eq_ignore_ascii_case(&repo.to_string())
    }
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
    #[serde(flatten)]
    identity: Identity,
    comments: Connection<CommentId>,
}

/// The kind of a node of `issueOrPullRequest`.
#[derive(Deserialize)]
struct Typed {
    #[serde(rename = "__typename")]
    typename: String,
    #[serde(flatten)]
    identity: Identity,
}

impl ItemKind {
    /// The GraphQL connection of the items of the kind.
    const fn connection(self) -> &'static str {
        match self {
            Self::Issues => "issues",
            Self::Pulls => "pullRequests",
        }
    }

    /// The type of the nodes of the kind.
    const fn typename(self) -> &'static str {
        match self {
            Self::Issues => "Issue",
            Self::Pulls => "PullRequest",
        }
    }
}

/// Ask GitHub GraphQL `selection` of the repository, with the variables
/// `variables` declared as `declared` beside `$owner` and `$name`. An item
/// that GitHub GraphQL cannot resolve, an error of the type `NOT_FOUND`, is
/// `null` in the data; a repository that it cannot resolve or that has
/// another name is not at its path; an answer with another error fails.
async fn graphql<T: DeserializeOwned>(
    cx: &Cx,
    repo: &RepoId,
    declared: &str,
    selection: &str,
    mut variables: serde_json::Value,
) -> Result<T> {
    let query = format!(
        "query($owner: String!, $name: String!{declared}) {{ \
         repository(owner: $owner, name: $name) {{ nameWithOwner {selection} }} }}"
    );
    variables["owner"] = repo.owner.to_string().into();
    variables["name"] = repo.repo.to_string().into();
    let response = cx
        .endpoint(GitHubApi)
        .post("/graphql")
        .body_json(&json!({ "query": query, "variables": variables }))
        .send()
        .await?;
    if response.status() != 200 {
        return Err(error_of(&response, response.status()));
    }
    let answer: GraphQl = serde_json::from_slice(response.body())
        .map_err(|error| ProviderError::invalid_input(format!("GitHub GraphQL: {error}")))?;
    let errors = answer.errors.iter().flatten().collect::<Vec<_>>();
    if let Some(error) = errors
        .iter()
        .find(|error| error.kind.as_deref() != Some("NOT_FOUND"))
    {
        return Err(ProviderError::internal(format!(
            "GitHub GraphQL: {}",
            error.message
        )));
    }
    let repository = answer
        .data
        .and_then(|data| data.repository)
        .ok_or_else(|| moved(repo))?;
    let named: Named = decode_json(repository.get())?;
    if !named
        .name_with_owner
        .eq_ignore_ascii_case(&repo.to_string())
    {
        return Err(moved(repo));
    }
    decode_json(repository.get())
}

/// The selection of an issue: the canonical bytes of `Issue`.
const ITEM: &str = "number title body state author { login } updatedAt comments { totalCount }";

/// The selection of a pull request: the canonical bytes of `PullRequest`,
/// with the commits of its base and of its head, which version its diff.
const PULL: &str = "number title body state author { login } updatedAt comments { totalCount } baseRefOid headRefOid";

impl ItemKind {
    /// The selection of a node of the kind.
    const fn selection(self) -> &'static str {
        match self {
            Self::Issues => ITEM,
            Self::Pulls => PULL,
        }
    }
}

/// The selection of the identity of a node.
const IDENTITY: &str = "number repository { nameWithOwner }";

/// `selection` on a node of `issueOrPullRequest`, of either kind.
fn on_items(selection: &str) -> String {
    format!("... on Issue {{ {selection} }} ... on PullRequest {{ {selection} }}")
}

/// The selection of the page after `$after` of the nodes of a connection,
/// each with the selection `node`.
fn page_selection(alias: &str, kind: ItemKind, after: &str, node: &str, ordered: bool) -> String {
    let order = if ordered {
        ", orderBy: { field: UPDATED_AT, direction: ASC }"
    } else {
        ""
    };
    format!(
        "{alias}: {}(first: {PAGE}, after: ${after}{order}) {{ totalCount nodes {{ {node} }} \
         pageInfo {{ hasNextPage endCursor }} }}",
        kind.connection()
    )
}

/// The alias of a kind in a `POST` of rows.
const fn rows_alias(kind: ItemKind) -> &'static str {
    match kind {
        ItemKind::Issues => "issues",
        ItemKind::Pulls => "pulls",
    }
}

/// A page of the numbers or of the counts of the comments of the items of a
/// kind, `node` the selection of each.
async fn scan_page<T: DeserializeOwned>(
    cx: &Cx,
    repo: &RepoId,
    at: &CountsAt,
    node: &str,
) -> Result<Connection<T>> {
    let Paged { page } = graphql::<Paged<T>>(
        cx,
        repo,
        ", $after: String",
        &page_selection("page", at.kind, "after", node, false),
        json!({ "after": at.after }),
    )
    .await?;
    Ok(page)
}

/// An item of an answer of GitHub GraphQL: its data, with the bytes of its
/// node.
struct Node {
    data: ItemData,
    bytes: Box<RawValue>,
}

impl Node {
    fn of(bytes: Box<RawValue>) -> Result<Self> {
        Ok(Self {
            data: decode_json(bytes.get())?,
            bytes,
        })
    }
}

/// What GitHub GraphQL answers for the number of an item.
enum ByNumber {
    /// An item of this repository with that number, of its kind.
    Found(ItemKind, Node),
    /// No item of this repository has the number.
    Gone,
}

/// Ask GitHub GraphQL for the items of `numbers`, each by its number.
async fn by_numbers(cx: &Cx, repo: &RepoId, numbers: &[u64]) -> Result<Vec<(u64, ByNumber)>> {
    let selection = numbers
        .iter()
        .map(|number| {
            format!(
                "t{number}: issueOrPullRequest(number: {number}) {{ __typename {} }} \
                 n{number}: issueOrPullRequest(number: {number}) \
                 {{ ... on Issue {{ {ITEM} }} ... on PullRequest {{ {PULL} }} }}",
                on_items(IDENTITY),
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    let mut answer: Aliased = graphql(cx, repo, "", &selection, json!({})).await?;
    let mut items = Vec::new();
    for number in numbers {
        let typed = aliased::<Typed>(&mut answer, &format!("t{number}"))?;
        let node = aliased::<Box<RawValue>>(&mut answer, &format!("n{number}"))?;
        let item = match (typed, node) {
            (Some(typed), Some(node)) => {
                let kind = [ItemKind::Issues, ItemKind::Pulls]
                    .into_iter()
                    .find(|kind| kind.typename() == typed.typename)
                    .ok_or_else(|| {
                        ProviderError::invalid_input(format!(
                            "the item {number} is a {}",
                            typed.typename
                        ))
                    })?;
                if typed.identity.of(repo, *number) {
                    ByNumber::Found(kind, Node::of(node)?)
                } else {
                    ByNumber::Gone
                }
            },
            _ => ByNumber::Gone,
        };
        items.push((*number, item));
    }
    Ok(items)
}

/// The item of `kind` with `number` as its object loads it: its node.
pub(crate) async fn load_item<T: DeserializeOwned>(
    cx: &Cx,
    repo: &RepoId,
    kind: ItemKind,
    number: u64,
) -> Result<Load<T>> {
    let mut items = by_numbers(cx, repo, &[number]).await?;
    Ok(match items.pop() {
        Some((_, ByNumber::Found(found, node))) if found == kind => Load::fresh(
            decode_json(node.bytes.get())?,
            Canonical::new(node.bytes.get().as_bytes(), None),
        ),
        _ => Load::NotFound,
    })
}

/// A page of the ids of the comments of an item, none for an item that is not
/// in the repository.
async fn ids_page(
    cx: &Cx,
    repo: &RepoId,
    item: ItemRef,
    after: Option<String>,
) -> Result<Option<Connection<CommentId>>> {
    #[derive(Deserialize)]
    struct Item {
        item: Option<Commented>,
    }
    let selection = format!(
        "item: issueOrPullRequest(number: $number) {{ {} }}",
        on_items(&format!(
            "{IDENTITY} {}",
            comment_ids("first: 100, after: $after")
        ))
    );
    let answer: Item = graphql(
        cx,
        repo,
        ", $number: Int!, $after: String",
        &selection,
        json!({ "number": item.number, "after": after }),
    )
    .await?;
    Ok(answer
        .item
        .filter(|commented| commented.identity.of(repo, item.number))
        .map(|commented| commented.comments))
}

/// The selection of the ids of the comments of an item, with `arguments`.
fn comment_ids(arguments: &str) -> String {
    format!(
        "comments({arguments}) {{ nodes {{ databaseId updatedAt }} pageInfo {{ hasNextPage endCursor }} }}"
    )
}

/// The alias of an item in a `POST` of the first pages of ids.
fn alias(item: ItemRef) -> String {
    format!("i{}", item.number)
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

fn comments_path(repo: &RepoId, item: ItemRef) -> String {
    format!("{}/comments", item_path(repo, item, StateFilter::All))
}

/// The writer of the facts of a call: with a ledger in a check, which counts
/// each comment that it adds to the projection or removes from it.
struct Writer<'a, K> {
    cx: &'a FeedCx<K>,
    repo: &'a RepoId,
    ledger: Option<&'a mut Ledger>,
    published: HashMap<ItemRef, BTreeSet<u64>>,
}

impl<'a, K> Writer<'a, K> {
    fn new(cx: &'a FeedCx<K>, repo: &'a RepoId, ledger: Option<&'a mut Ledger>) -> Self {
        Self {
            cx,
            repo,
            ledger,
            published: HashMap::new(),
        }
    }

    /// The IDs of the comments of `item` in the projection.
    fn published(&mut self, item: ItemRef) -> Result<&BTreeSet<u64>> {
        if !self.published.contains_key(&item) {
            let names = self
                .cx
                .names::<u64>(comments_path(self.repo, item))?
                .map(|listed| listed.names.into_iter().collect())
                .unwrap_or_default();
            self.published.insert(item, names);
        }
        Ok(&self.published[&item])
    }

    /// Whether the projection has `item`.
    fn has_item(&self, item: ItemRef) -> Result<bool> {
        Ok(self
            .cx
            .count(item_path(self.repo, item, StateFilter::All))?
            .is_some())
    }

    /// The facts of an item from its node: `title`, `body`, `state`, `user`,
    /// and its canonical bytes under `{kind}/all/{number}/`, with its entry in
    /// `{kind}/open/` for the state `OPEN`, else the removal of that entry;
    /// its folder `comments/`, without comments for a count of `0`; and its
    /// entries in the listings of its folders, which they join as `folders`
    /// says.
    fn item(&mut self, kind: ItemKind, node: &Node, folders: ListingWrite) -> Result<()> {
        let data = &node.data;
        let item = ItemRef {
            kind,
            number: data.number,
        };
        let cx = self.cx;
        cx.dir(folder(self.repo, kind, StateFilter::All), folders)?;
        let at = item_path(self.repo, item, StateFilter::All);
        cx.dir(&at, ListingWrite::Partial)?;
        cx.file(format!("{at}/title"), data.title.as_bytes())?;
        cx.file(format!("{at}/body"), data.body.as_bytes())?;
        cx.file(format!("{at}/state"), data.state.as_bytes())?;
        cx.file(format!("{at}/user"), data.login().as_bytes())?;
        let canonical = Canonical::new(node.bytes.get().as_bytes(), None);
        match kind {
            ItemKind::Issues => cx.canonical::<Issue>(&at, canonical),
            ItemKind::Pulls => cx.canonical::<PullRequest>(&at, canonical),
        }
        if data.comments.total_count == 0 {
            self.no_comments(item)?;
        } else {
            cx.dir(comments_path(self.repo, item), ListingWrite::Partial)?;
        }
        if data.state == "OPEN" {
            cx.dir(folder(self.repo, kind, StateFilter::Open), folders)?;
            cx.dir(
                item_path(self.repo, item, StateFilter::Open),
                ListingWrite::Partial,
            )?;
        } else {
            cx.remove(item_path(self.repo, item, StateFilter::Open))?;
        }
        Ok(())
    }

    /// The facts of a comment: `author`, `body.md`, and its canonical bytes,
    /// the row of its list, under `comments/{id}/` of its item, its
    /// `comments/` joined as `write` says.
    fn comment(
        &mut self,
        item: ItemRef,
        write: ListingWrite,
        comment: &Comment,
        bytes: &RawValue,
    ) -> Result<()> {
        let comments = comments_path(self.repo, item);
        self.cx.dir(&comments, write)?;
        let at = format!("{comments}/{}", comment.id);
        self.cx.dir(&at, ListingWrite::Partial)?;
        self.cx
            .file(format!("{at}/author"), comment.login().as_bytes())?;
        self.cx
            .file(format!("{at}/body.md"), comment.body.as_bytes())?;
        self.cx
            .canonical::<Comment>(&at, Canonical::new(bytes.get().as_bytes(), None));
        if self.ledger.is_some() {
            let published = self.published(item)?.contains(&comment.id);
            let ledger = self.ledger.as_deref_mut().expect("the ledger is some");
            if ledger.removed.remove(&comment.id).is_none() && !published {
                ledger.added.insert(comment.id, item.number);
            }
        }
        Ok(())
    }

    /// The removal of a comment that left its item.
    fn remove_comment(&mut self, item: ItemRef, id: u64) -> Result<()> {
        self.cx
            .remove(format!("{}/{id}", comments_path(self.repo, item)))?;
        if self.ledger.is_some() {
            let published = self.published(item)?.contains(&id);
            let ledger = self.ledger.as_deref_mut().expect("the ledger is some");
            if ledger.added.remove(&id).is_none() && published {
                ledger.removed.insert(id, item.number);
            }
        }
        Ok(())
    }

    /// The folder `comments/` of an item without comments: emptied and
    /// complete, as its list would be empty.
    fn no_comments(&mut self, item: ItemRef) -> Result<()> {
        for id in self.known(item)? {
            self.remove_comment(item, id)?;
        }
        self.cx
            .dir(comments_path(self.repo, item), ListingWrite::Complete)
    }

    /// The comments of `item` in the projection and those that the ledger
    /// added to it.
    fn known(&mut self, item: ItemRef) -> Result<BTreeSet<u64>> {
        let mut known = self.published(item)?.clone();
        if let Some(ledger) = self.ledger.as_deref() {
            known.extend(
                ledger
                    .added
                    .iter()
                    .filter(|(_, number)| **number == item.number)
                    .map(|(id, _)| *id),
            );
            known.retain(|id| !ledger.removed.contains_key(id));
        }
        Ok(known)
    }

    /// The removal of an item that left the repository, with its comments.
    fn remove_item(&mut self, item: ItemRef) -> Result<()> {
        for id in self.known(item)? {
            self.remove_comment(item, id)?;
        }
        for filter in [StateFilter::All, StateFilter::Open] {
            self.cx.remove(item_path(self.repo, item, filter))?;
        }
        Ok(())
    }

    /// Take a comment of an item by its ID with
    /// `GET /repos/{owner}/{repo}/issues/comments/{id}`: the comment, or the
    /// removal of a comment that is not of the repository, with its
    /// `comments/` joined as `write` says.
    async fn fetch_comment(&mut self, item: ItemRef, id: u64, write: ListingWrite) -> Result<()> {
        let response = self
            .cx
            .endpoint(GitHubApi)
            .get(format!("/repos/{}/issues/comments/{id}", self.repo))
            .send()
            .await?;
        match response.status() {
            200 => {
                let text = std::str::from_utf8(response.body())
                    .map_err(|error| ProviderError::invalid_input(format!("utf-8: {error}")))?;
                let bytes: Box<RawValue> = decode_json(text)?;
                self.comment(item, write, &decode_json(bytes.get())?, &bytes)
            },
            301 | 404 | 410 => {
                self.remove_comment(item, id)?;
                self.cx.dir(comments_path(self.repo, item), write)
            },
            status => Err(error_of(&response, status)),
        }
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

    /// IDs that need no request.
    fn ended_already() -> Self {
        Self {
            page: IdsPage::End,
            ..Self::new(BTreeSet::new(), None)
        }
    }

    /// Whether a request of the IDs remains after their first page.
    fn pending(&self) -> bool {
        !self.fetch.is_empty() || matches!(self.page, IdsPage::After(_))
    }

    /// Whether no page and no comment by its ID remains.
    fn ended(&self) -> bool {
        self.fetch.is_empty() && matches!(self.page, IdsPage::End)
    }

    /// Take a page of the IDs of `item`.
    fn take<K>(
        &mut self,
        w: &mut Writer<'_, K>,
        item: ItemRef,
        page: Connection<CommentId>,
    ) -> Result<()> {
        let known = w.known(item)?;
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
        for id in known.iter().chain(&self.brought) {
            if !self.seen.contains(id) {
                w.remove_comment(item, *id)?;
            }
        }
        if self.fetch.is_empty() {
            w.cx.dir(comments_path(w.repo, item), ListingWrite::Complete)?;
        }
        Ok(())
    }

    /// The next request of the IDs of `item`: a comment by its ID, else a
    /// page of the IDs. `false` for an item that is not in the repository.
    async fn step<K>(&mut self, w: &mut Writer<'_, K>, item: ItemRef) -> Result<bool> {
        if let Some(id) = self.fetch.pop_first() {
            let write = if self.ended() {
                ListingWrite::Complete
            } else {
                ListingWrite::Partial
            };
            w.fetch_comment(item, id, write).await?;
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
        match ids_page(w.cx, w.repo, item, after).await? {
            Some(page) => {
                self.take(w, item, page)?;
                Ok(true)
            },
            None => Ok(false),
        }
    }
}

// ===========================================================================
// The feed of a repository
// ===========================================================================

/// One call of the feed of a repository: its context, the repository, and
/// its token, which its requests move.
struct RepoFeed<'a> {
    cx: &'a FeedCx<RepoKey>,
    repo: RepoId,
    issues: Option<String>,
    pulls: Option<String>,
    comments: Since,
    total: Option<u64>,
}

impl Repo {
    /// The feed of the issues, pull requests, and comments of a repository.
    pub(crate) async fn feed(cx: FeedCx<RepoKey>) -> Result<Feed> {
        let repo = RepoId::new(&cx.key().owner, &cx.key().repo);
        let token = cx.token::<RepoToken>()?;
        let mut feed = RepoFeed {
            cx: &cx,
            repo,
            issues: None,
            pulls: None,
            comments: Since::default(),
            total: None,
        };
        let Some(token) = token else {
            return feed.listing_items(ItemKind::Issues).await;
        };
        feed.issues = token.issues;
        feed.pulls = token.pulls;
        feed.comments = token.comments;
        feed.total = token.total;
        match token.stage {
            Some(Stage::Items { kind }) => feed.listing_items(kind).await,
            Some(Stage::Comments { link }) => feed.listing_comments(link).await,
            Some(Stage::Counts(at)) => feed.listing_counts(at).await,
            Some(Stage::Ids {
                kind,
                items,
                ids,
                then,
            }) => feed.listing_ids(kind, items, ids, then).await,
            Some(Stage::Total) => feed.listing_total().await,
            Some(Stage::Check(check)) => feed.check_step(*check).await,
            None => {
                let check = Check {
                    next: CheckRequest::Rows {
                        kinds: BTreeSet::from([ItemKind::Issues, ItemKind::Pulls]),
                    },
                    changed: BTreeMap::new(),
                    totals: BTreeMap::new(),
                    removed: BTreeSet::new(),
                    numbered: BTreeSet::new(),
                    ledger: Ledger::default(),
                    remote: None,
                    scan: Scan::Idle,
                };
                feed.check_step(check).await
            },
        }
    }
}

impl RepoFeed<'_> {
    fn cursor(&mut self, kind: ItemKind) -> &mut Option<String> {
        match kind {
            ItemKind::Issues => &mut self.issues,
            ItemKind::Pulls => &mut self.pulls,
        }
    }

    /// The `POST` of the next page of the nodes of each of `kinds` after its
    /// cursor, in the order of `updatedAt`, which moves each cursor to the
    /// last node of its page: each page by its kind.
    async fn rows(&mut self, kinds: &BTreeSet<ItemKind>) -> Result<BTreeMap<ItemKind, Rows>> {
        let mut declared = Vec::new();
        let mut selection = Vec::new();
        let mut variables = json!({});
        for kind in kinds {
            let alias = rows_alias(*kind);
            declared.push(format!(", ${alias}: String"));
            selection.push(page_selection(alias, *kind, alias, kind.selection(), true));
            variables[alias] = self.cursor(*kind).clone().into();
        }
        let mut answer: Aliased = graphql(
            self.cx,
            &self.repo,
            &declared.concat(),
            &selection.join(" "),
            variables,
        )
        .await?;
        let mut pages = BTreeMap::new();
        for kind in kinds {
            let rows: Rows = aliased(&mut answer, rows_alias(*kind))?.ok_or_else(|| {
                ProviderError::invalid_input(format!("GitHub GraphQL has no {kind}"))
            })?;
            if let Some(end) = &rows.page_info.end_cursor {
                *self.cursor(*kind) = Some(end.clone());
            }
            pages.insert(*kind, rows);
        }
        Ok(pages)
    }

    /// A page of the comments of the repository in the order of `updated_at`:
    /// the page at `link`, else the comments since a second before `since`,
    /// with the `ETag` of their list for `since` the time of the list.
    async fn comments_page(
        &self,
        since: Option<OffsetDateTime>,
        link: Option<&str>,
    ) -> Result<RestPage> {
        let page = if let Some(link) = link {
            rest_page(self.cx, link, None).await?
        } else {
            let path = format!(
                "/repos/{}/issues/comments?sort=updated&direction=asc&per_page={PAGE}{}",
                self.repo,
                since_pair(since)?
            );
            let etag = self
                .comments
                .etag
                .as_deref()
                .filter(|_| since == self.comments.time);
            rest_page(self.cx, &path, etag).await?
        };
        match page {
            RestPage::Gone => Err(moved(&self.repo)),
            page => Ok(page),
        }
    }

    /// A page of the items of a kind of the first full listing, with the
    /// inner token of each item.
    async fn listing_items(&mut self, kind: ItemKind) -> Result<Feed> {
        let mut pages = self.rows(&BTreeSet::from([kind])).await?;
        let page = pages.remove(&kind).expect("the rows of the asked kind");
        let nodes = page
            .nodes
            .into_iter()
            .map(Node::of)
            .collect::<Result<Vec<_>>>()?;
        let mut w = Writer::new(self.cx, &self.repo, None);
        for node in &nodes {
            w.item(kind, node, ListingWrite::Partial)?;
        }
        let stage = match (page.page_info.next()?, kind) {
            (Some(_), kind) => Stage::Items { kind },
            (None, ItemKind::Issues) => Stage::Items {
                kind: ItemKind::Pulls,
            },
            (None, ItemKind::Pulls) => Stage::Comments { link: None },
        };
        let mut part = self.part(Some(stage))?;
        let key = self.cx.key();
        for node in &nodes {
            let token = ItemToken {
                updated_at: node.data.updated_at,
                comments: node.data.comments.total_count,
                next: None,
            };
            let number = node.data.number;
            part = match kind {
                ItemKind::Issues => part.inner::<Issue>(
                    &IssueKey {
                        owner: key.owner.clone(),
                        repo: key.repo.clone(),
                        filter: Facet(StateFilter::All),
                        number,
                    },
                    &Some(token),
                )?,
                ItemKind::Pulls => part.inner::<PullRequest>(
                    &PullKey {
                        owner: key.owner.clone(),
                        repo: key.repo.clone(),
                        filter: Facet(StateFilter::All),
                        number,
                    },
                    &Some(token),
                )?,
            };
        }
        Ok(part.into())
    }

    /// A page of the comments of the first full listing: each comment of an
    /// item of the projection joins its `comments/` as complete as it was; a
    /// comment of an item made after the items waits for its item.
    async fn listing_comments(&mut self, link: Option<String>) -> Result<Feed> {
        let RestPage::Rows { rows, next, .. } = self.comments_page(None, link.as_deref()).await?
        else {
            return Err(ProviderError::internal(
                "the comments answered 304 to a request without an ETag",
            ));
        };
        let mut w = Writer::new(self.cx, &self.repo, None);
        for bytes in rows {
            let row = CommentRow::of(bytes)?;
            self.comments.time = Some(row.updated_at);
            if w.has_item(row.item)? {
                w.comment(row.item, ListingWrite::Join, &row.comment, &row.bytes)?;
            }
        }
        let stage = match next {
            Some(link) => Stage::Comments { link: Some(link) },
            None => Stage::Counts(CountsAt {
                kind: ItemKind::Issues,
                after: None,
            }),
        };
        Ok(self.part(Some(stage))?.into())
    }

    /// A page of the counts of the first full listing: the `comments/` of an
    /// item with the count of its `comments/` in the projection becomes
    /// complete, and the items with another count take their IDs next.
    async fn listing_counts(&mut self, at: CountsAt) -> Result<Feed> {
        let page = scan_page::<Counted>(self.cx, &self.repo, &at, "number comments { totalCount }")
            .await?;
        let mut items = BTreeSet::new();
        for node in &page.nodes {
            let item = ItemRef {
                kind: at.kind,
                number: node.number,
            };
            let comments = comments_path(&self.repo, item);
            // An item made after the items is outside the projection.
            let Some(listed) = self.cx.count(&comments)? else {
                continue;
            };
            if listed.count == node.comments.total_count {
                self.cx.dir(&comments, ListingWrite::Complete)?;
            } else {
                items.insert(node.number);
            }
        }
        let kind = at.kind;
        let then = at.after(page.page_info.next()?);
        Ok(self.part(Some(listing_after(kind, items, then)))?.into())
    }

    /// The next request of the IDs of the first of `items` in the first full
    /// listing, without the comments by their IDs that an answer of the item
    /// brought since their page; an item that is not in the repository
    /// leaves it.
    async fn listing_ids(
        &mut self,
        kind: ItemKind,
        mut items: BTreeSet<u64>,
        mut ids: Ids,
        then: Option<CountsAt>,
    ) -> Result<Feed> {
        let number = *items.first().ok_or_else(|| {
            ProviderError::invalid_input("the first full listing takes the IDs of no item")
        })?;
        let item = ItemRef { kind, number };
        let mut w = Writer::new(self.cx, &self.repo, None);
        let known = w.known(item)?;
        ids.fetch.retain(|id| !known.contains(id));
        if ids.ended() {
            self.cx
                .dir(comments_path(&self.repo, item), ListingWrite::Complete)?;
        } else if !ids.step(&mut w, item).await? {
            w.remove_item(item)?;
            ids = Ids::ended_already();
        }
        if !ids.ended() {
            return Ok(self
                .part(Some(Stage::Ids {
                    kind,
                    items,
                    ids,
                    then,
                }))?
                .into());
        }
        items.remove(&number);
        Ok(self.part(Some(listing_after(kind, items, then)))?.into())
    }

    /// The last part of the first full listing: the REST count of the
    /// comments, with the four folders complete.
    async fn listing_total(&mut self) -> Result<Feed> {
        self.total = Some(remote(self.cx, &self.repo).await?.total);
        for kind in [ItemKind::Issues, ItemKind::Pulls] {
            for filter in [StateFilter::All, StateFilter::Open] {
                self.cx
                    .dir(folder(&self.repo, kind, filter), ListingWrite::Complete)?;
            }
        }
        Ok(self.part(None)?.into())
    }

    /// The next request of a check, a part each, and the request after it.
    async fn check_step(&mut self, mut check: Check) -> Result<Feed> {
        let request = std::mem::replace(&mut check.next, CheckRequest::Counts);
        let next = match request {
            CheckRequest::Rows { kinds } => Some(self.check_rows(&mut check, kinds).await?),
            CheckRequest::Remote => {
                check.remote = Some(remote(self.cx, &self.repo).await?);
                Some(CheckRequest::Comments {
                    since: self.comments.time,
                    link: None,
                    seen: BTreeSet::new(),
                    latest: self.comments.time,
                })
            },
            CheckRequest::Comments {
                since,
                link,
                seen,
                latest,
            } => {
                self.check_comments(&mut check, since, link, seen, latest)
                    .await?
            },
            CheckRequest::Counts => {
                self.check_counts(&mut check).await?;
                None
            },
            CheckRequest::Ids { number } => {
                self.check_ids(&mut check, number).await?;
                None
            },
            CheckRequest::Numbers { kind, after, seen } => {
                self.check_numbers(&mut check, kind, after, seen).await?
            },
            CheckRequest::Items { kind, numbers } => {
                self.check_items(&mut check, kind, numbers).await?
            },
            CheckRequest::Scan(at) => {
                self.scan_comments(&mut check, at).await?;
                None
            },
        };
        let next = match next {
            Some(next) => Some(next),
            None => self.after(&mut check)?,
        };
        let Some(next) = next else {
            let remote = check.remote.ok_or_else(|| {
                ProviderError::internal("a check ends without the count of its comments")
            })?;
            self.total = Some(remote.total + late(&check.ledger, remote));
            return Ok(self.part(None)?.into());
        };
        check.next = next;
        Ok(self.part(Some(Stage::Check(Box::new(check))))?.into())
    }

    /// The next request of the IDs of a changed item.
    async fn check_ids(&mut self, check: &mut Check, number: u64) -> Result<()> {
        let Check {
            changed, ledger, ..
        } = &mut *check;
        let changed = changed.get_mut(&number).ok_or_else(|| {
            ProviderError::invalid_input(format!("item {number} is not a changed item"))
        })?;
        let item = ItemRef {
            kind: changed.kind,
            number,
        };
        let mut w = Writer::new(self.cx, &self.repo, Some(ledger));
        if changed.ids.step(&mut w, item).await? {
            return Ok(());
        }
        w.remove_item(item)?;
        gone(check, item);
        Ok(())
    }

    /// A `POST` of the rows of a check: the facts of each node, and the
    /// changed items.
    async fn check_rows(
        &mut self,
        check: &mut Check,
        kinds: BTreeSet<ItemKind>,
    ) -> Result<CheckRequest> {
        let pages = self.rows(&kinds).await?;
        let mut more = BTreeSet::new();
        let Check {
            changed,
            ledger,
            totals,
            ..
        } = check;
        let mut w = Writer::new(self.cx, &self.repo, Some(ledger));
        for (kind, page) in pages {
            totals.insert(kind, page.total_count);
            if page.page_info.next()?.is_some() {
                more.insert(kind);
            }
            for bytes in page.nodes {
                let node = Node::of(bytes)?;
                let item = ItemRef {
                    kind,
                    number: node.data.number,
                };
                let new = !w.has_item(item)?;
                w.item(kind, &node, ListingWrite::Complete)?;
                let comments = node.data.comments.total_count;
                changed.insert(
                    item.number,
                    Changed {
                        kind,
                        comments,
                        new,
                        gone: false,
                        ids: if comments == 0 {
                            Ids::ended_already()
                        } else {
                            Ids::new(BTreeSet::new(), None)
                        },
                    },
                );
            }
        }
        Ok(if more.is_empty() {
            CheckRequest::Remote
        } else {
            CheckRequest::Rows { kinds: more }
        })
    }

    /// A page of the comments of a check updated since a second before
    /// `since`. A row that came before in this pass, which an edit moved past
    /// a page while the pass paged, shifted the rows after it: the pass asks
    /// for the comments since `since` again, so that no row is lost to it.
    async fn check_comments(
        &mut self,
        check: &mut Check,
        since: Option<OffsetDateTime>,
        link: Option<String>,
        mut seen: BTreeSet<u64>,
        mut latest: Option<OffsetDateTime>,
    ) -> Result<Option<CheckRequest>> {
        let first = link.is_none();
        let RestPage::Rows {
            rows, etag, next, ..
        } = self.comments_page(since, link.as_deref()).await?
        else {
            return Ok(None);
        };
        let mut moved = false;
        let Check {
            changed, ledger, ..
        } = check;
        let mut w = Writer::new(self.cx, &self.repo, Some(ledger));
        for bytes in rows {
            let row = CommentRow::of(bytes)?;
            moved |= !seen.insert(row.comment.id);
            latest = latest.max(Some(row.updated_at));
            match changed.get_mut(&row.item.number) {
                Some(changed) if !changed.gone => {
                    w.comment(row.item, ListingWrite::Partial, &row.comment, &row.bytes)?;
                    changed.ids.brought.insert(row.comment.id);
                },
                Some(_) => {},
                // A comment of an item outside the projection waits for its
                // item, which the numbers of its kind bring.
                None => {
                    if w.has_item(row.item)? {
                        w.comment(row.item, ListingWrite::Join, &row.comment, &row.bytes)?;
                    }
                },
            }
        }
        if moved {
            return Ok(Some(CheckRequest::Comments {
                since,
                link: None,
                seen: BTreeSet::new(),
                latest,
            }));
        }
        if let Some(link) = next {
            return Ok(Some(CheckRequest::Comments {
                since,
                link: Some(link),
                seen,
                latest,
            }));
        }
        self.comments.etag = etag.filter(|_| first && latest == since);
        self.comments.time = latest;
        Ok(None)
    }

    /// The `POST` of the first pages of the IDs of the next changed items
    /// whose comments are other than their counts, a page of items; each
    /// takes its page, or leaves when it is not in the repository.
    async fn check_counts(&mut self, check: &mut Check) -> Result<()> {
        let asked = check
            .changed
            .iter()
            .filter(|(_, changed)| !changed.gone && matches!(changed.ids.page, IdsPage::First))
            .take(PAGE)
            .map(|(number, changed)| ItemRef {
                kind: changed.kind,
                number: *number,
            })
            .collect::<Vec<_>>();
        let selection = asked
            .iter()
            .map(|item| {
                format!(
                    "{}: issueOrPullRequest(number: {}) {{ {} }}",
                    alias(*item),
                    item.number,
                    on_items(&format!("{IDENTITY} {}", comment_ids("first: 100")))
                )
            })
            .collect::<Vec<_>>()
            .join(" ");
        let mut answer: Aliased = graphql(self.cx, &self.repo, "", &selection, json!({})).await?;
        let mut left = Vec::new();
        {
            let Check {
                changed, ledger, ..
            } = &mut *check;
            let mut w = Writer::new(self.cx, &self.repo, Some(ledger));
            for item in asked {
                let commented = aliased::<Commented>(&mut answer, &alias(item))?;
                let Some(commented) =
                    commented.filter(|commented| commented.identity.of(&self.repo, item.number))
                else {
                    w.remove_item(item)?;
                    left.push(item);
                    continue;
                };
                let changed = changed.get_mut(&item.number).ok_or_else(|| {
                    ProviderError::internal(format!("item {} is not a changed item", item.number))
                })?;
                changed.ids.take(&mut w, item, commented.comments)?;
            }
        }
        for item in left {
            gone(check, item);
        }
        Ok(())
    }

    /// A page of the numbers of a kind whose count is other than the
    /// projection's, as bits; after the last page, the removal of each item
    /// of the projection that they lack, and the items that they bring and
    /// that the projection lacks.
    async fn check_numbers(
        &mut self,
        check: &mut Check,
        kind: ItemKind,
        after: Option<String>,
        mut seen: Vec<u64>,
    ) -> Result<Option<CheckRequest>> {
        let at = CountsAt { kind, after };
        let page = scan_page::<Numbered>(self.cx, &self.repo, &at, "number").await?;
        for node in &page.nodes {
            let word = usize::try_from(node.number / 64)
                .map_err(|error| ProviderError::internal(format!("number: {error}")))?;
            if seen.len() <= word {
                seen.resize(word + 1, 0);
            }
            seen[word] |= 1 << (node.number % 64);
        }
        if let Some(after) = page.page_info.next()? {
            return Ok(Some(CheckRequest::Numbers {
                kind,
                after: Some(after),
                seen,
            }));
        }
        check.numbered.insert(kind);
        let has = |number: u64| {
            usize::try_from(number / 64)
                .ok()
                .and_then(|word| seen.get(word))
                .is_some_and(|bits| bits & (1 << (number % 64)) != 0)
        };
        let listed = self
            .cx
            .names::<u64>(folder(&self.repo, kind, StateFilter::All))?
            .map(|listed| listed.names)
            .unwrap_or_default();
        let mut left = Vec::new();
        {
            let Check {
                changed, ledger, ..
            } = &mut *check;
            let mut w = Writer::new(self.cx, &self.repo, Some(ledger));
            let written = changed
                .iter()
                .filter(|(_, changed)| changed.kind == kind && !changed.gone)
                .map(|(number, _)| *number)
                .collect::<Vec<_>>();
            for number in listed.iter().copied().chain(written) {
                if !has(number) && !left.contains(&ItemRef { kind, number }) {
                    w.remove_item(ItemRef { kind, number })?;
                    left.push(ItemRef { kind, number });
                }
            }
        }
        for item in left {
            gone(check, item);
        }
        let numbers = seen
            .iter()
            .enumerate()
            .flat_map(|(word, bits)| {
                (0..64u64)
                    .filter(move |bit| bits & (1 << bit) != 0)
                    .map(move |bit| word as u64 * 64 + bit)
            })
            .filter(|number| {
                listed.binary_search(number).is_err() && !check.changed.contains_key(number)
            })
            .collect::<BTreeSet<_>>();
        Ok((!numbers.is_empty()).then_some(CheckRequest::Items { kind, numbers }))
    }

    /// The `POST` of the next page of the items of a kind that the projection
    /// lacks, by their numbers: each written as a new changed item, unless it
    /// left the repository since its number.
    async fn check_items(
        &mut self,
        check: &mut Check,
        kind: ItemKind,
        mut numbers: BTreeSet<u64>,
    ) -> Result<Option<CheckRequest>> {
        let page = std::iter::from_fn(|| numbers.pop_first())
            .take(PAGE)
            .collect::<Vec<_>>();
        let found = by_numbers(self.cx, &self.repo, &page).await?;
        let Check {
            changed, ledger, ..
        } = &mut *check;
        let mut w = Writer::new(self.cx, &self.repo, Some(ledger));
        for (number, item) in found {
            if let ByNumber::Found(found, node) = item
                && found == kind
            {
                w.item(kind, &node, ListingWrite::Complete)?;
                let comments = node.data.comments.total_count;
                changed.insert(
                    number,
                    Changed {
                        kind,
                        comments,
                        new: true,
                        gone: false,
                        ids: if comments == 0 {
                            Ids::ended_already()
                        } else {
                            Ids::new(BTreeSet::new(), None)
                        },
                    },
                );
            }
        }
        Ok((!numbers.is_empty()).then_some(CheckRequest::Items { kind, numbers }))
    }

    /// The request of a check after the one before it: the next request of
    /// the IDs of a changed item; the first page of the IDs of the changed
    /// items whose comments are other than their counts; the numbers of a
    /// kind whose count is other than the projection's with the changes of
    /// the check; a page of the counts of the comments while the REST count
    /// is other than the count the projection holds with the changes of the
    /// check; or none at its end.
    fn after(&self, check: &mut Check) -> Result<Option<CheckRequest>> {
        if let Some((number, _)) = check
            .changed
            .iter()
            .find(|(_, changed)| !changed.gone && changed.ids.pending())
        {
            return Ok(Some(CheckRequest::Ids { number: *number }));
        }
        let Check {
            changed, ledger, ..
        } = &mut *check;
        let mut w = Writer::new(self.cx, &self.repo, Some(ledger));
        let mut first_pages = false;
        for (number, changed) in changed.iter_mut() {
            if changed.gone || !matches!(changed.ids.page, IdsPage::First) {
                continue;
            }
            let item = ItemRef {
                kind: changed.kind,
                number: *number,
            };
            let mut known = w.known(item)?;
            known.extend(&changed.ids.brought);
            if known.len() as u64 == changed.comments {
                self.cx
                    .dir(comments_path(&self.repo, item), ListingWrite::Complete)?;
                changed.ids.page = IdsPage::End;
            } else {
                first_pages = true;
            }
        }
        if first_pages {
            return Ok(Some(CheckRequest::Counts));
        }
        for kind in [ItemKind::Issues, ItemKind::Pulls] {
            if check.numbered.contains(&kind) {
                continue;
            }
            let total = *check.totals.get(&kind).ok_or_else(|| {
                ProviderError::internal(format!("a check compares the {kind} without their count"))
            })?;
            let all = folder(&self.repo, kind, StateFilter::All);
            let listed = self
                .cx
                .count(&all)?
                .ok_or_else(|| {
                    ProviderError::internal(format!("the projection has no listing of {all}"))
                })?
                .count;
            let new = check
                .changed
                .values()
                .filter(|changed| changed.kind == kind && changed.new && !changed.gone)
                .count() as u64;
            let removed = check
                .removed
                .iter()
                .filter(|item| item.kind == kind)
                .count() as u64;
            if listed + new != total + removed {
                return Ok(Some(CheckRequest::Numbers {
                    kind,
                    after: None,
                    seen: Vec::new(),
                }));
            }
        }
        let remote = check
            .remote
            .ok_or_else(|| ProviderError::internal("a check lacks the count of its comments"))?;
        let total = self
            .total
            .ok_or_else(|| ProviderError::internal("a check lacks the count it holds"))?;
        let expected = total + counted(&check.ledger, remote) - check.ledger.removed.len() as u64;
        if expected == remote.total {
            return Ok(None);
        }
        Ok(match std::mem::replace(&mut check.scan, Scan::Done) {
            Scan::Idle => Some(CheckRequest::Scan(CountsAt {
                kind: ItemKind::Issues,
                after: None,
            })),
            Scan::At(at) => Some(CheckRequest::Scan(at)),
            Scan::Done => None,
        })
    }

    /// A page of the counts of the comments of the items for a scan: each
    /// item of the projection that the check did not write, whose comments
    /// are other than its count, becomes a changed item, which takes its IDs.
    async fn scan_comments(&self, check: &mut Check, at: CountsAt) -> Result<()> {
        let page = scan_page::<Counted>(self.cx, &self.repo, &at, "number comments { totalCount }")
            .await?;
        let kind = at.kind;
        check.scan = match at.after(page.page_info.next()?) {
            Some(next) => Scan::At(next),
            None => Scan::Done,
        };
        let Check {
            changed, ledger, ..
        } = &mut *check;
        let mut w = Writer::new(self.cx, &self.repo, Some(ledger));
        for node in page.nodes {
            let item = ItemRef {
                kind,
                number: node.number,
            };
            if changed.contains_key(&node.number) || !w.has_item(item)? {
                continue;
            }
            let known = w.known(item)?;
            if known.len() as u64 != node.comments.total_count {
                let brought = known
                    .difference(w.published(item)?)
                    .copied()
                    .collect::<BTreeSet<_>>();
                changed.insert(
                    node.number,
                    Changed {
                        kind,
                        comments: node.comments.total_count,
                        new: false,
                        gone: false,
                        ids: Ids::new(brought, None),
                    },
                );
            }
        }
        Ok(())
    }

    /// A part of the feed with the token as the requests of the call left it
    /// and `stage`, followed by more parts while a stage remains.
    fn part(&self, stage: Option<Stage>) -> Result<FeedPart> {
        let more = stage.is_some();
        Ok(Feed::part(&RepoToken {
            issues: self.issues.clone(),
            pulls: self.pulls.clone(),
            comments: self.comments.clone(),
            total: self.total,
            stage,
        })?
        .more(more))
    }
}

/// The stage of the first full listing that takes the IDs of `items` from
/// their first page, then the page of the counts `then`, or the REST count
/// without one.
fn listing_after(kind: ItemKind, items: BTreeSet<u64>, then: Option<CountsAt>) -> Stage {
    if !items.is_empty() {
        return Stage::Ids {
            kind,
            items,
            ids: Ids::new(BTreeSet::new(), None),
            then,
        };
    }
    match then {
        Some(at) => Stage::Counts(at),
        None => Stage::Total,
    }
}

/// The record of a check of an item that left the repository.
fn gone(check: &mut Check, item: ItemRef) {
    match check.changed.get_mut(&item.number) {
        Some(changed) => {
            changed.gone = true;
            changed.ids = Ids::ended_already();
            if !changed.new {
                check.removed.insert(item);
            }
        },
        None => {
            check.removed.insert(item);
        },
    }
}

/// The comments that a check added and that the REST count counts: those
/// up to its newest comment.
fn counted(ledger: &Ledger, remote: Remote) -> u64 {
    ledger
        .added
        .keys()
        .filter(|id| remote.newest.is_some_and(|newest| **id <= newest))
        .count() as u64
}

/// The comments that a check added after the REST count.
fn late(ledger: &Ledger, remote: Remote) -> u64 {
    ledger.added.len() as u64 - counted(ledger, remote)
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
/// its `comments/`, the pages of its comments; else its node, and for a
/// change of an item with its comments in the projection and a count other
/// than `0`, the comments since its last `updatedAt` and its IDs. Its first
/// answer, which the lookup of its folder starts as each operation resolves
/// its path from the root, takes its node alone. Each request is a part.
async fn item_feed<K>(cx: &FeedCx<K>, repo: &RepoId, item: ItemRef) -> Result<Feed> {
    let token = match cx.token::<Option<ItemToken>>()?.flatten() {
        Some(mut token) => match token.next.take() {
            Some(next) => return item_step(cx, repo, item, token, next).await,
            None => Some(token),
        },
        None => None,
    };
    if let Some(token) = &token
        && cx.start() == Some("comments")
    {
        return item_comments_of(cx, repo, item, token).await;
    }
    let mut found = by_numbers(cx, repo, &[item.number]).await?;
    let Some((_, ByNumber::Found(kind, node))) = found.pop() else {
        return removal(cx, repo, item);
    };
    if kind != item.kind {
        return Err(ProviderError::not_found(format!(
            "{repo}#{} is not in {}",
            item.number, item.kind
        )));
    }
    let mut w = Writer::new(cx, repo, None);
    w.item(kind, &node, ListingWrite::Partial)?;
    let mut next = ItemToken {
        updated_at: node.data.updated_at,
        comments: node.data.comments.total_count,
        next: None,
    };
    if let Some(token) = token
        && next.comments != 0
        && (next.updated_at != token.updated_at || next.comments != token.comments)
        && cx
            .count(comments_path(repo, item))?
            .is_some_and(|listed| listed.complete)
    {
        next.next = Some(ItemRequest::Since {
            since: token.updated_at,
            link: None,
            brought: BTreeSet::new(),
        });
    }
    item_part(&next)
}

/// The comments of an item from its `comments/`: none for a count of `0`,
/// else the pages of its comments, a part each.
async fn item_comments_of<K>(
    cx: &FeedCx<K>,
    repo: &RepoId,
    item: ItemRef,
    token: &ItemToken,
) -> Result<Feed> {
    let token = ItemToken {
        updated_at: token.updated_at,
        comments: token.comments,
        next: None,
    };
    if token.comments == 0 {
        Writer::new(cx, repo, None).no_comments(item)?;
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

/// A page of the comments of an item: the page at `link`, else the first of
/// its comments, since a second before `since` with one; `None` for an item
/// that is not in the repository.
async fn item_comments<K>(
    cx: &FeedCx<K>,
    repo: &RepoId,
    item: ItemRef,
    since: Option<OffsetDateTime>,
    link: Option<String>,
) -> Result<Option<(Vec<(Comment, Box<RawValue>)>, Option<String>)>> {
    let path = match link {
        Some(link) => link,
        None => format!(
            "/repos/{repo}/issues/{}/comments?per_page={PAGE}{}",
            item.number,
            since_pair(since)?
        ),
    };
    match rest_page(cx, &path, None).await? {
        RestPage::Rows { rows, next, .. } => {
            let rows = rows
                .into_iter()
                .map(|row| Ok((decode_json(row.get())?, row)))
                .collect::<Result<Vec<_>>>()?;
            Ok(Some((rows, next)))
        },
        RestPage::Gone => Ok(None),
        RestPage::Unchanged => Err(ProviderError::internal(
            "the comments answered 304 to a request without an ETag",
        )),
    }
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
    let mut w = Writer::new(cx, repo, None);
    match request {
        ItemRequest::Since {
            since,
            link,
            mut brought,
        } => {
            let Some((rows, next)) = item_comments(cx, repo, item, Some(since), link).await? else {
                return Ok(ItemStep::Left);
            };
            for (comment, bytes) in &rows {
                w.comment(item, ListingWrite::Partial, comment, bytes)?;
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
            let Some((rows, next)) = item_comments(cx, repo, item, None, link).await? else {
                return Ok(ItemStep::Left);
            };
            for (comment, bytes) in &rows {
                w.comment(item, ListingWrite::Partial, comment, bytes)?;
                brought.insert(comment.id);
            }
            if let Some(link) = next {
                return Ok(ItemStep::Next(ItemRequest::Comments {
                    link: Some(link),
                    brought,
                }));
            }
            if brought.len() as u64 != token.comments {
                return Ok(ItemStep::Next(ItemRequest::Ids(Ids::new(
                    brought,
                    Some(token.updated_at),
                ))));
            }
            for id in w.known(item)?.difference(&brought) {
                cx.remove(format!("{}/{id}", comments_path(repo, item)))?;
            }
            cx.dir(comments_path(repo, item), ListingWrite::Complete)?;
            Ok(ItemStep::Done)
        },
        ItemRequest::Ids(mut ids) => {
            if !ids.step(&mut w, item).await? {
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
    Ok(Feed::part(&Some(token))?.more(token.next.is_some()).into())
}

/// The removal of an item from its folders, with the token of a removed
/// item.
fn removal<K>(cx: &FeedCx<K>, repo: &RepoId, item: ItemRef) -> Result<Feed> {
    Writer::new(cx, repo, None).remove_item(item)?;
    Ok(Feed::part(&None::<ItemToken>)?.into())
}
