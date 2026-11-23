//! Path keys, object loads, collection methods, and live faces for the GitHub
//! provider.

use serde::{Deserialize, Serialize};

use omnifs_core::ContentType;
use omnifs_sdk::prelude::*;

use crate::api::GitHubApi;
use crate::objects::{
    Comment, Issue, Notification, Owner, PullRequest, Repo, Review, ReviewComment,
};
use crate::{
    NOTIFICATION_PAGE_SIZE, OwnerKind, OwnerName, RepoId, RepoName, StateFilter, ThreadId,
};

/// Identity discriminator for the comment anchor's `{item_kind}` segment. A
/// plain `PathSegment` capture (NOT a facet): an issue comment and a pull
/// comment are distinct objects, so `item_kind` is part of identity.
#[omnifs_sdk::path_segment]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ItemKind {
    #[strum(serialize = "issues")]
    Issues,
    #[strum(serialize = "pulls")]
    Pulls,
}

#[omnifs_sdk::path_captures]
pub(crate) struct OwnerKey {
    pub(crate) owner: OwnerName,
}

#[omnifs_sdk::path_captures]
pub(crate) struct RepoKey {
    pub(crate) owner: OwnerName,
    pub(crate) repo: RepoName,
}

#[omnifs_sdk::path_captures]
pub(crate) struct IssueKey {
    pub(crate) owner: OwnerName,
    pub(crate) repo: RepoName,
    #[expect(
        dead_code,
        reason = "a key of the route binds its `{filter}` capture, which its identity excludes"
    )]
    pub(crate) filter: Facet<StateFilter>,
    pub(crate) number: u64,
}

#[omnifs_sdk::path_captures]
pub(crate) struct PullKey {
    pub(crate) owner: OwnerName,
    pub(crate) repo: RepoName,
    #[expect(
        dead_code,
        reason = "a key of the route binds its `{filter}` capture, which its identity excludes"
    )]
    pub(crate) filter: Facet<StateFilter>,
    pub(crate) number: u64,
}

#[omnifs_sdk::path_captures]
pub(crate) struct ReviewKey {
    pub(crate) owner: OwnerName,
    pub(crate) repo: RepoName,
    #[expect(
        dead_code,
        reason = "a key of the route binds its `{filter}` capture, which its identity excludes"
    )]
    pub(crate) filter: Facet<StateFilter>,
    pub(crate) number: u64,
    pub(crate) review_id: u64,
}

#[omnifs_sdk::path_captures]
pub(crate) struct ReviewCommentKey {
    pub(crate) owner: OwnerName,
    pub(crate) repo: RepoName,
    #[expect(
        dead_code,
        reason = "a key of the route binds its `{filter}` capture, which its identity excludes"
    )]
    pub(crate) filter: Facet<StateFilter>,
    pub(crate) number: u64,
    pub(crate) review_id: u64,
    pub(crate) comment_id: u64,
}

#[omnifs_sdk::path_captures]
pub(crate) struct CommentKey {
    pub(crate) owner: OwnerName,
    pub(crate) repo: RepoName,
    pub(crate) item_kind: ItemKind,
    #[expect(
        dead_code,
        reason = "a key of the route binds its `{filter}` capture, which its identity excludes"
    )]
    pub(crate) filter: Facet<StateFilter>,
    pub(crate) number: u64,
    pub(crate) comment_id: u64,
}

#[omnifs_sdk::path_captures]
pub(crate) struct NotificationKey {
    pub(crate) thread_id: ThreadId,
}

impl Owner {
    pub(crate) async fn load(
        cx: &Cx,
        key: &OwnerKey,
        since: Option<Validator>,
    ) -> Result<Load<Self>> {
        // `/users/{login}` answers the profile of an organization too.
        cx.endpoint(GitHubApi)
            .get(format!("/users/{}", key.owner))
            .maybe_if_none_match(since.as_ref())
            .load_with(decode_json::<Self>)
            .await
    }
}

impl Repo {
    /// A repository that moved answers `301` at its old path, which is not
    /// followed: the repository is not at this path.
    pub(crate) async fn load(
        cx: &Cx,
        key: &RepoKey,
        since: Option<Validator>,
    ) -> Result<Load<Self>> {
        let response = cx
            .endpoint(GitHubApi)
            .get(format!("/repos/{}/{}", key.owner, key.repo))
            .maybe_if_none_match(since.as_ref())
            .send()
            .await?;
        moved_or_load(&response)
    }
}

impl Issue {
    pub(crate) async fn load(
        cx: &Cx,
        key: &IssueKey,
        _since: Option<Validator>,
    ) -> Result<Load<Self>> {
        let repo = RepoId::new(&key.owner, &key.repo);
        crate::feed::load_item(cx, &repo, ItemKind::Issues, key.number).await
    }
}

impl PullRequest {
    pub(crate) async fn load(
        cx: &Cx,
        key: &PullKey,
        _since: Option<Validator>,
    ) -> Result<Load<Self>> {
        let repo = RepoId::new(&key.owner, &key.repo);
        crate::feed::load_item(cx, &repo, ItemKind::Pulls, key.number).await
    }
}

impl Review {
    pub(crate) async fn load(
        cx: &Cx,
        key: &ReviewKey,
        since: Option<Validator>,
    ) -> Result<Load<Self>> {
        let repo = RepoId::new(&key.owner, &key.repo);
        cx.endpoint(GitHubApi)
            .get(format!(
                "/repos/{repo}/pulls/{}/reviews/{}",
                key.number, key.review_id
            ))
            .maybe_if_none_match(since.as_ref())
            .load_with(decode_json::<Self>)
            .await
    }
}

impl ReviewComment {
    pub(crate) async fn load(
        cx: &Cx,
        key: &ReviewCommentKey,
        since: Option<Validator>,
    ) -> Result<Load<Self>> {
        let repo = RepoId::new(&key.owner, &key.repo);
        let load = cx
            .endpoint(GitHubApi)
            .get(format!("/repos/{repo}/pulls/comments/{}", key.comment_id))
            .maybe_if_none_match(since.as_ref())
            .load_with(decode_json::<Self>)
            .await?;
        // Its canonical bytes are its fields alone: the row of GitHub has the
        // code beside the comment.
        Ok(match load {
            Load::Fresh { value, .. } => {
                let bytes = serde_json::to_vec(&value)
                    .map_err(|error| ProviderError::internal(error.to_string()))?;
                Load::fresh(value, Canonical::new(bytes, None))
            },
            other => other,
        })
    }
}

impl Comment {
    pub(crate) async fn load(
        cx: &Cx,
        key: &CommentKey,
        since: Option<Validator>,
    ) -> Result<Load<Self>> {
        let repo = RepoId::new(&key.owner, &key.repo);
        let response = cx
            .endpoint(GitHubApi)
            .get(format!("/repos/{repo}/issues/comments/{}", key.comment_id))
            .maybe_if_none_match(since.as_ref())
            .send()
            .await?;
        moved_or_load(&response)
    }
}

impl Notification {
    pub(crate) async fn load(
        cx: &Cx,
        key: &NotificationKey,
        since: Option<Validator>,
    ) -> Result<Load<Self>> {
        cx.endpoint(GitHubApi)
            .get(format!("/notifications/threads/{}", key.thread_id))
            .maybe_if_none_match(since.as_ref())
            .load_with(decode_json::<Self>)
            .await
    }
}

#[derive(Debug, Deserialize)]
struct RepoListing {
    name: String,
}

/// The cursor of a page of `Owner::repos`: the owner kind, which selects the
/// REST scope, and the page number.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct RepoCursor {
    kind: OwnerKind,
    page: u64,
}

impl omnifs_sdk::collection::Cursor for RepoCursor {
    fn encode(&self) -> String {
        serde_json::to_string(self).expect("repo cursor serialization is infallible")
    }

    fn decode(token: &str) -> Result<Self> {
        serde_json::from_str(token).map_err(|_| ProviderError::invalid_input("invalid repo cursor"))
    }
}

/// The validator of a page of `Owner::repos`: the owner kind and the `ETag`
/// of the page, so that the collection asks for a page again in a sync with
/// one conditional request and no classification of the owner.
#[derive(Debug, Deserialize, Serialize)]
struct RepoValidator {
    kind: OwnerKind,
    etag: String,
}

impl Owner {
    /// Anchor-topology collection: the repo names listed under `/{owner}` ARE
    /// the child `Repo` anchors (`/{owner}/{repo}`). The repo listing carries
    /// only the name, not the single-repo canonical, so entries are `key`.
    ///
    /// A page with a cursor or a validator asks the repos of their owner
    /// kind. The first page without them asks the repos of the type of the
    /// owner, by the profile that the listing of the owner loaded in the same
    /// operation: one request.
    pub(crate) async fn repos(
        key: OwnerKey,
        cx: ListCx<RepoCursor>,
    ) -> Result<Collection<Repo, RepoCursor>> {
        let since: Option<RepoValidator> = cx.validator().map(decode_validator).transpose()?;
        let (kind, page) = match (cx.cursor(), &since) {
            (Some(cursor), _) => (cursor.kind, cursor.page),
            (None, Some(since)) => (since.kind, 1),
            (None, None) => {
                let owner = cx.object::<crate::objects::Owner>(&key)?.ok_or_else(|| {
                    ProviderError::internal(format!(
                        "the listing of {} has no profile of its owner",
                        key.owner
                    ))
                })?;
                let kind = if owner.kind == "Organization" {
                    OwnerKind::Org
                } else {
                    OwnerKind::User
                };
                (kind, 1)
            },
        };
        let etag = since.map(|since| VersionToken(since.etag));
        let repos = |kind: OwnerKind| {
            cx.endpoint(GitHubApi)
                .get(format!(
                    "/{}/{}/repos?sort=full_name&direction=asc&per_page=100&page={page}",
                    kind.scope(),
                    key.owner
                ))
                .maybe_if_none_match(etag.as_ref())
                .page::<Vec<RepoListing>>()
        };
        let answer = repos(kind).await?;
        let more = next_page(&answer);
        let PageAnswer::Fresh {
            value: repos,
            validator,
            ..
        } = answer
        else {
            return unchanged(&cx);
        };
        let entries = repos
            .into_iter()
            .map(|row| {
                Ok(CollectionEntry::key(RepoKey {
                    owner: key.owner.clone(),
                    repo: parsed(&row.name)?,
                }))
            })
            .collect::<Result<Vec<_>>>()?;
        let collection = page_or_complete(
            entries,
            more,
            RepoCursor {
                kind,
                page: page + 1,
            },
        );
        Ok(with_page_validator(
            collection,
            encode_validator(&RepoValidator {
                kind,
                etag: page_etag(validator)?.0,
            }),
        ))
    }
}

impl Notification {
    pub(crate) async fn list(cx: DirCx) -> Result<DirListing> {
        match cx.intent() {
            DirIntent::Lookup { child } => {
                let entries = child
                    .strip_prefix("thread-")
                    .and_then(|id| id.parse::<ThreadId>().ok())
                    .map(|_| Entry::dir(child.clone()));
                Ok(DirListing::exhaustive(entries))
            },
            DirIntent::ReadFile { .. } => Ok(DirListing::exhaustive([])),
            DirIntent::List { .. } => {
                let page = cx.page_cursor(1);
                let answer = cx
                    .endpoint(GitHubApi)
                    .get(format!(
                        "/notifications?per_page={NOTIFICATION_PAGE_SIZE}&page={page}"
                    ))
                    .maybe_if_none_match(cx.version())
                    .page::<Vec<Notification>>()
                    .await?;
                // GitHub asks a client to poll notifications no more often than
                // the seconds of `X-Poll-Interval`, on each answer.
                let interval = answer
                    .header("x-poll-interval")
                    .map(|seconds| {
                        seconds.trim().parse::<u32>().map_err(|error| {
                            ProviderError::invalid_input(format!(
                                "X-Poll-Interval {seconds:?}: {error}"
                            ))
                        })
                    })
                    .transpose()?;
                let more = next_page(&answer);
                let PageAnswer::Fresh {
                    value: notifications,
                    validator,
                    ..
                } = answer
                else {
                    let token = cx.version().cloned().ok_or_else(|| {
                        ProviderError::internal(
                            "the notifications answered 304 to a request without an ETag",
                        )
                    })?;
                    return Ok(DirListing::unchanged(PageValidator { token, interval }));
                };
                let entries = notifications
                    .iter()
                    .map(|notification| {
                        parsed::<ThreadId>(&notification.id)
                            .map(|_| Entry::dir(format!("thread-{}", notification.id)))
                    })
                    .collect::<Result<Vec<_>>>()?;
                let mut listing = if more {
                    DirListing::paged(entries, Cursor::Page(page + 1))
                } else {
                    DirListing::exhaustive(entries)
                };
                listing = listing.with_validator(PageValidator {
                    token: page_etag(validator)?,
                    interval,
                });
                for notification in notifications {
                    listing = listing.preload_file(
                        format!("thread-{}/item.md", notification.id),
                        FileProjection::inline(notification.item_markdown())
                            .content_type(ContentType::Markdown)
                            .dynamic()
                            .build(),
                    );
                }
                Ok(listing)
            },
        }
    }
}

/// A segment of a path from a name that GitHub gives, which is the error of
/// its answer when it is no segment.
fn parsed<T: std::str::FromStr>(name: &str) -> Result<T> {
    name.parse().map_err(|_| {
        ProviderError::invalid_input(format!("GitHub names {name:?}, no path segment"))
    })
}

/// Whether the header `Link` of a page answer has a page after it.
fn next_page<T>(answer: &PageAnswer<T>) -> bool {
    answer.header("link").is_some_and(|links| {
        links.split(',').any(|link| {
            link.split(';')
                .skip(1)
                .any(|param| param.trim() == r#"rel="next""#)
        })
    })
}

/// The load of an object from its answer: `301`, `404`, and `410` say that
/// it is not at this path, as a redirect that is not followed.
fn moved_or_load<T: serde::de::DeserializeOwned>(response: &HttpResponse) -> Result<Load<T>> {
    match response.status() {
        200 => Ok(Load::fresh(
            decode_json(response.body())?,
            Canonical::new(
                response.body(),
                response.header("etag").map(VersionToken::from),
            ),
        )),
        304 => Ok(Load::Unchanged),
        301 | 404 | 410 => Ok(Load::NotFound),
        status => Err(ProviderError::from_http_answer(
            status,
            response
                .headers()
                .iter()
                .filter_map(|(name, value)| Some((name.as_str(), value.to_str().ok()?))),
        )),
    }
}

/// The listing of a page: complete after the last page, else a page with
/// the cursor `next` of the page after it.
fn page_or_complete<T: omnifs_sdk::object::Object, C: omnifs_sdk::collection::Cursor>(
    entries: Vec<CollectionEntry<T>>,
    more: bool,
    next: C,
) -> Collection<T, C> {
    if more {
        Collection::page(entries).next(next)
    } else {
        Collection::complete(entries)
    }
}

/// The `ETag` of a page answer, which the host asks for the page again with.
fn page_etag(validator: Option<VersionToken>) -> Result<VersionToken> {
    validator.ok_or_else(|| ProviderError::internal("a page answered without an ETag"))
}

/// A page with `token` as its validator, the `ETag` of its answer or a
/// validator that carries it.
fn with_page_validator<T: omnifs_sdk::object::Object, C: omnifs_sdk::collection::Cursor>(
    collection: Collection<T, C>,
    token: VersionToken,
) -> Collection<T, C> {
    collection.with_validator(PageValidator {
        token,
        interval: None,
    })
}

/// The page of the validator of the call stands with that validator, which
/// the `304` of its request answered.
fn unchanged<T: omnifs_sdk::object::Object, C: omnifs_sdk::collection::Cursor>(
    cx: &ListCx<C>,
) -> Result<Collection<T, C>> {
    let token = cx.validator().cloned().ok_or_else(|| {
        ProviderError::internal("a page answered 304 to a request without an ETag")
    })?;
    Ok(Collection::unchanged(PageValidator {
        token,
        interval: None,
    }))
}

fn encode_validator<T: Serialize>(validator: &T) -> VersionToken {
    VersionToken(serde_json::to_string(validator).expect("validator serialization is infallible"))
}

fn decode_validator<T: serde::de::DeserializeOwned>(token: &VersionToken) -> Result<T> {
    serde_json::from_str(token.as_str())
        .map_err(|_| ProviderError::invalid_input("invalid page validator"))
}
