//! Path keys, object loads, collection methods, and live faces for the GitHub
//! provider.

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use omnifs_core::ContentType;
use omnifs_sdk::prelude::*;

use crate::api::GitHubApi;
use crate::objects::{
    ChangedFile, Comment, Issue, Notification, Owner, PullRequest, Repo, Review, ReviewComment,
};
use crate::{
    FILE_PAGE_SIZE, FilePath, NOTIFICATION_PAGE_SIZE, OwnerKind, OwnerName, REVIEW_PAGE_SIZE,
    RepoId, RepoName, StateFilter, ThreadId,
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

// ===========================================================================
// Keys
// ===========================================================================

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
    pub(crate) filter: Facet<StateFilter>,
    pub(crate) number: u64,
}

#[omnifs_sdk::path_captures]
pub(crate) struct ChangedFileKey {
    pub(crate) owner: OwnerName,
    pub(crate) repo: RepoName,
    #[expect(
        dead_code,
        reason = "a key of the route binds its `{filter}` capture, which its identity excludes"
    )]
    pub(crate) filter: Facet<StateFilter>,
    pub(crate) number: u64,
    pub(crate) path: FilePath,
}

#[omnifs_sdk::path_captures]
pub(crate) struct ReviewKey {
    pub(crate) owner: OwnerName,
    pub(crate) repo: RepoName,
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

// ===========================================================================
// Object loads (forwarded to by the #[object] macro)
// ===========================================================================

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

impl ChangedFile {
    pub(crate) async fn load(
        cx: &Cx,
        key: &ChangedFileKey,
        _since: Option<Validator>,
    ) -> Result<Load<Self>> {
        let filename = key.path.decoded()?;
        let Some((file, raw)) = find_pull_file(cx, key, &filename).await? else {
            return Ok(Load::NotFound);
        };
        Ok(Load::fresh(
            file,
            Canonical::new(raw.get().as_bytes(), None),
        ))
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
        cx.endpoint(GitHubApi)
            .get(format!("/repos/{repo}/pulls/comments/{}", key.comment_id))
            .maybe_if_none_match(since.as_ref())
            .load_with(decode_json::<Self>)
            .await
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

// ===========================================================================
// Collections
// ===========================================================================

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

impl PullRequest {
    pub(crate) async fn files(
        key: PullKey,
        cx: ListCx<PageCursor>,
    ) -> Result<Collection<ChangedFile, PageCursor>> {
        let page = cx.cursor().map_or(1, |c| c.0);
        let answer = cx
            .endpoint(GitHubApi)
            .get(pull_files_path(&key.owner, &key.repo, key.number, page))
            .maybe_if_none_match(cx.validator())
            .page::<Vec<Box<RawValue>>>()
            .await?;
        let more = next_page(&answer);
        let PageAnswer::Fresh {
            value: files,
            validator,
            ..
        } = answer
        else {
            return unchanged(&cx);
        };
        let entries = fresh(files, |file: &ChangedFile| {
            Ok(ChangedFileKey {
                owner: key.owner.clone(),
                repo: key.repo.clone(),
                filter: key.filter,
                number: key.number,
                path: parsed(&FilePath::encode(&file.filename))?,
            })
        })?;
        Ok(with_page_validator(
            page_or_complete(entries, more, PageCursor(page + 1)),
            page_etag(validator)?,
        ))
    }

    pub(crate) async fn reviews(
        key: PullKey,
        cx: ListCx<PageCursor>,
    ) -> Result<Collection<Review, PageCursor>> {
        let page = cx.cursor().map_or(1, |c| c.0);
        let repo = RepoId::new(&key.owner, &key.repo);
        let answer = cx
            .endpoint(GitHubApi)
            .get(format!(
                "/repos/{repo}/pulls/{}/reviews?per_page={REVIEW_PAGE_SIZE}&page={page}",
                key.number
            ))
            .maybe_if_none_match(cx.validator())
            .page::<Vec<Box<RawValue>>>()
            .await?;
        let more = next_page(&answer);
        let PageAnswer::Fresh {
            value: reviews,
            validator,
            ..
        } = answer
        else {
            return unchanged(&cx);
        };
        let entries = fresh(reviews, |review: &Review| {
            Ok(ReviewKey {
                owner: key.owner.clone(),
                repo: key.repo.clone(),
                filter: key.filter,
                number: key.number,
                review_id: review.id,
            })
        })?;
        Ok(with_page_validator(
            page_or_complete(entries, more, PageCursor(page + 1)),
            page_etag(validator)?,
        ))
    }
}

impl Review {
    pub(crate) async fn comments(
        key: ReviewKey,
        cx: ListCx<PageCursor>,
    ) -> Result<Collection<ReviewComment, PageCursor>> {
        let page = cx.cursor().map_or(1, |c| c.0);
        let repo = RepoId::new(&key.owner, &key.repo);
        let answer = cx
            .endpoint(GitHubApi)
            .get(format!(
                "/repos/{repo}/pulls/{}/reviews/{}/comments?per_page={REVIEW_PAGE_SIZE}&page={page}",
                key.number, key.review_id
            ))
            .maybe_if_none_match(cx.validator())
            .page::<Vec<Box<RawValue>>>()
            .await?;
        let more = next_page(&answer);
        let PageAnswer::Fresh {
            value: comments,
            validator,
            ..
        } = answer
        else {
            return unchanged(&cx);
        };
        let entries = fresh(comments, |comment: &ReviewComment| {
            Ok(ReviewCommentKey {
                owner: key.owner.clone(),
                repo: key.repo.clone(),
                filter: key.filter,
                number: key.number,
                review_id: key.review_id,
                comment_id: comment.id,
            })
        })?;
        Ok(with_page_validator(
            page_or_complete(entries, more, PageCursor(page + 1)),
            page_etag(validator)?,
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

// ===========================================================================
// Live faces (blob, tree, direct)
// ===========================================================================

impl PullRequest {
    /// The diff of the pull request between its base and its head, which its
    /// node in the projection names: the host holds the diff of each pair of
    /// commits and asks GitHub for it once. The read of `diff.patch` follows
    /// the lookup of its pull request, whose answer wrote that node, so a pull
    /// request that the projection lacks is not found.
    pub(crate) async fn diff(cx: Cx, key: PullKey) -> Result<BlobFile<Diff>> {
        let Some(pull) = cx.object::<Self>(&key)? else {
            return Err(ProviderError::not_found(format!(
                "the projection has no pull request {}",
                key.number
            )));
        };
        let path = format!(
            "/repos/{}/pulls/{}",
            RepoId::new(&key.owner, &key.repo),
            key.number
        );
        let version = format!("{}..{}", pull.base_ref_oid, pull.head_ref_oid);
        let blob = cx
            .endpoint(GitHubApi)
            .get(path)
            .header("Accept", "application/vnd.github.diff")
            .into_blob()
            .version(version.as_str())
            .fetch()
            .await?;
        Ok(BlobFile::new(blob.id)
            .size(FileSize::Exact(blob.size))
            .stability(Stability::Dynamic)
            .version(version))
    }
}

/// The format of a diff.
pub(crate) struct Diff;

impl Format for Diff {
    const CT: ContentType = ContentType::Custom("text/x-diff");
}

impl Repo {
    /// The tree of the last commit of the default branch. `commits/HEAD` with
    /// the `sha` media type answers that commit and takes the commit of the
    /// tree in quotes as its `ETag`: `304` keeps the tree, and `200` names the
    /// commit the clone checks out.
    pub(crate) async fn tree(cx: Cx, key: RepoKey) -> Result<Tree> {
        let repo_id = RepoId::new(&key.owner, &key.repo);
        let etag = cx
            .version()
            .map(|commit| VersionToken::from(format!("\"{}\"", commit.as_str())));
        let head = cx
            .endpoint(GitHubApi)
            .get(format!("/repos/{repo_id}/commits/HEAD"))
            .header("Accept", "application/vnd.github.sha")
            .maybe_if_none_match(etag.as_ref())
            .send_checked()
            .await?;
        if head.status() == 304 {
            return Ok(Tree::unchanged());
        }
        let commit = std::str::from_utf8(head.body())
            .map_err(|_| ProviderError::invalid_input("the commit of HEAD is not UTF-8"))?;
        let opened = cx
            .git()
            .open_repo(format!("git@github.com:{repo_id}.git"))
            .commit(commit)
            .await?;
        Ok(Tree::at(TreeRef::new(opened.tree), commit))
    }
}

// ===========================================================================
// Helpers
// ===========================================================================

/// The entries of the rows of a page, each with its bytes as the canonical
/// bytes of its object: a row of a list is the object that its own request
/// answers. `key` names the entry of a row.
fn fresh<T: omnifs_sdk::object::Object + serde::de::DeserializeOwned>(
    rows: Vec<Box<RawValue>>,
    key: impl Fn(&T) -> Result<T::Key>,
) -> Result<Vec<CollectionEntry<T>>> {
    rows.into_iter()
        .map(|row| {
            let value: T = serde_json::from_str(row.get())
                .map_err(|error| ProviderError::invalid_input(format!("json decode: {error}")))?;
            Ok(CollectionEntry::fresh(
                key(&value)?,
                value,
                Canonical::new(row.get().as_bytes(), None),
            ))
        })
        .collect()
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

fn pull_files_path(owner: &OwnerName, repo: &RepoName, number: u64, page: u64) -> String {
    let repo = RepoId::new(owner, repo);
    format!("/repos/{repo}/pulls/{number}/files?per_page={FILE_PAGE_SIZE}&page={page}")
}

/// The changed file of a pull request named `filename`, from the pages of
/// its files by their link `next`.
async fn find_pull_file(
    cx: &Cx,
    key: &ChangedFileKey,
    filename: &str,
) -> Result<Option<(ChangedFile, Box<RawValue>)>> {
    for page in 1.. {
        let answer = cx
            .endpoint(GitHubApi)
            .get(pull_files_path(&key.owner, &key.repo, key.number, page))
            .page::<Vec<Box<RawValue>>>()
            .await?;
        let more = next_page(&answer);
        let PageAnswer::Fresh { value: rows, .. } = answer else {
            return Err(ProviderError::internal(
                "the files answered 304 to a request without an ETag",
            ));
        };
        for raw in rows {
            let file = serde_json::from_str::<ChangedFile>(raw.get())
                .map_err(|err| ProviderError::invalid_input(format!("json decode: {err}")))?;
            if file.filename == filename {
                return Ok(Some((file, raw)));
            }
        }
        if !more {
            break;
        }
    }
    Ok(None)
}
