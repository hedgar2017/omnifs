//! github-provider: GitHub virtual filesystem provider for omnifs, a
//! component of the target `wasm32-wasip2` alone.
#![cfg(target_arch = "wasm32")]

use omnifs_sdk::prelude::*;
use serde::{Deserialize, Serialize};

pub(crate) use omnifs_sdk::prelude::Result;

mod api;
mod feed;
mod item;
mod objects;

use objects::{Comment, Issue, Notification, Owner, PullRequest, Repo, Review, ReviewComment};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OwnerKind {
    User,
    Org,
}

impl OwnerKind {
    /// The REST scope of the owner's resources.
    pub(crate) const fn scope(self) -> &'static str {
        match self {
            Self::User => "users",
            Self::Org => "orgs",
        }
    }
}

/// State filter for resources. `all` comes first: the facts of an issue or a
/// pull request live under its folder in `all/`, which `open/` shares.
#[omnifs_sdk::path_segment]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[strum(serialize_all = "snake_case")]
pub enum StateFilter {
    All,
    Open,
}

#[omnifs_sdk::provider(
    id = "github",
    display_name = "GitHub",
    description = "repos, issues, and pull requests",
    mount = "github",
    projection = 8,
    capabilities(domain(
        "api.github.com",
        "Fetch GitHub API resources such as repository metadata, issues, pull requests, and notifications."
    ),),
    auth = r#"{"default":"device","schemes":[{"staticToken":{"key":"pat","valuePrefix":"Bearer ","description":"GitHub personal access token","injectDomains":["api.github.com"],"creationUrl":"https://github.com/settings/tokens/new?scopes=read:user&description=omnifs","validation":{"method":"GET","url":"https://api.github.com/user","expectStatus":200,"extract":{"identity":"/login"}},"ambientSources":[{"kind":{"envVar":{"name":"GITHUB_TOKEN"}}},{"kind":{"command":{"argv":["gh","auth","token"]}},"note":"gh CLI login"}]}},{"oauth":{"key":"device","displayName":"GitHub OAuth device flow","authorizationEndpoint":"https://github.com/login/oauth/authorize","tokenEndpoint":"https://github.com/login/oauth/access_token","defaultClientId":"Ov23licogxMDzS47s9sF","flow":{"deviceCode":{"deviceAuthorizationEndpoint":"https://github.com/login/device/code","devicePollCompat":"errorInOkBody"}},"injectDomains":["api.github.com"],"injectValuePrefix":"Bearer "}}],"guidance":{"pat":{"summary":"A classic personal access token; the create link pre-selects the read:user scope.","setupSteps":["Add the repo scope as well if you want to browse private repositories and their issues or pull requests."],"docsUrl":"https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/managing-your-personal-access-tokens"},"device":{"summary":"Approve a one-time code at github.com/login/device using omnifs's GitHub app; nothing to copy back."}}}"#
)]
impl GithubProvider {
    fn start(r: &mut Router) -> Result<()> {
        r.object::<Owner>("/{owner}", |o| {
            o.dynamic();
            o.file("owner.json").canonical::<Json>()?;
            o.file("profile.md").representation::<Markdown>()?;
            o.dir("{repo}").collection(Owner::repos)?;
            Ok(())
        })?;

        r.object::<Repo>("/{owner}/{repo}", |o| {
            o.dynamic();
            o.file("repo.json").canonical::<Json>()?;
            o.dir("issues")
                .choices(StateFilter::choices().expect("StateFilter has finite choices"))?;
            o.dir("issues/{filter}").fed()?;
            o.dir("pulls")
                .choices(StateFilter::choices().expect("StateFilter has finite choices"))?;
            o.dir("pulls/{filter}").fed()?;
            o.feed(Repo::feed)
                .starts(["issues/{filter}", "pulls/{filter}"])?;
            Ok(())
        })?;

        r.object::<Issue>("/{owner}/{repo}/issues/{filter}/{number}", |o| {
            o.dynamic();
            o.fed();
            o.file("item.json").canonical::<Json>()?;
            o.file("item.md").representation::<Markdown>()?;
            o.file("title").fed()?;
            o.file("state").fed()?;
            o.file("user").fed()?;
            o.file("body").fed()?;
            o.dir("comments").fed()?;
            o.feed(Issue::feed)
                .starts(["", "title", "body", "state", "user", "comments"])?;
            Ok(())
        })?;

        r.object::<PullRequest>("/{owner}/{repo}/pulls/{filter}/{number}", |o| {
            o.dynamic();
            o.fed();
            o.file("item.json").canonical::<Json>()?;
            o.file("item.md").representation::<Markdown>()?;
            o.file("title").fed()?;
            o.file("state").fed()?;
            o.file("user").fed()?;
            o.file("body").fed()?;
            o.dir("comments").fed()?;
            o.dir("reviews").fed()?;
            o.feed(PullRequest::feed)
                .starts(["", "title", "body", "state", "user", "comments", "reviews"])?;
            Ok(())
        })?;

        r.object::<Review>(
            "/{owner}/{repo}/pulls/{filter}/{number}/reviews/{review_id}",
            |o| {
                o.dynamic();
                o.fed();
                o.file("review.json").canonical::<Json>()?;
                o.file("review.md").representation::<Markdown>()?;
                o.file("state").fed()?;
                o.file("user").fed()?;
                o.file("body.md").fed()?;
                o.dir("comments").fed()?;
                Ok(())
            },
        )?;

        r.object::<ReviewComment>(
            "/{owner}/{repo}/pulls/{filter}/{number}/reviews/{review_id}/comments/{comment_id}",
            |o| {
                o.dynamic();
                o.fed();
                o.file("comment.json").canonical::<Json>()?;
                o.file("comment.md").representation::<Markdown>()?;
                o.file("body.md").fed()?;
                o.file("author").fed()?;
                o.file("path").fed()?;
                Ok(())
            },
        )?;

        r.object::<Comment>(
            "/{owner}/{repo}/{item_kind}/{filter}/{number}/comments/{comment_id}",
            |o| {
                o.dynamic();
                o.fed();
                o.file("comment.json").canonical::<Json>()?;
                o.file("comment.md").representation::<Markdown>()?;
                o.file("body.md").fed()?;
                o.file("author").fed()?;
                Ok(())
            },
        )?;

        r.dir("/notifications").handler(Notification::list)?;
        r.object::<Notification>("/notifications/thread-{thread_id}", |o| {
            o.dynamic();
            o.file("notification.json").canonical::<Json>()?;
            o.file("item.md").representation::<Markdown>()?;
            o.file("reason").computed(Notification::reason)?;
            o.file("subject").computed(Notification::subject)?;
            Ok(())
        })?;

        Ok(())
    }
}

#[omnifs_sdk::path_segment(validate = is_safe_owner)]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OwnerName(String);

#[omnifs_sdk::path_segment(validate = is_safe_segment)]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepoName(String);

#[omnifs_sdk::path_segment(validate = is_safe_segment)]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ThreadId(String);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct RepoId {
    owner: OwnerName,
    repo: RepoName,
}

impl RepoId {
    pub(crate) fn new(owner: &OwnerName, repo: &RepoName) -> Self {
        Self {
            owner: owner.clone(),
            repo: repo.clone(),
        }
    }
}

impl std::fmt::Display for RepoId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.owner, self.repo)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct User {
    pub(crate) login: String,
}

/// Validates that a path segment is a safe GitHub repo or numeric ID.
/// GitHub permits leading dots in repo names (`.github` is the canonical
/// per-org community-config repo), so we only block the two path-traversal
/// cases.
pub fn is_safe_segment(s: &str) -> bool {
    if s.is_empty() || s == "." || s == ".." {
        return false;
    }
    s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}

/// Validates that a path segment is a safe GitHub owner (user or org).
/// Unlike repos, owners never start with a dot, so rejecting leading-dot
/// names keeps the `{owner}` capture from binding the host's mount-root
/// ignore files (`.gitignore`/`.ignore`/`.rgignore`) as phantom owner
/// directories, which would shadow them and defeat the ignore mechanism.
pub fn is_safe_owner(s: &str) -> bool {
    !s.eq_ignore_ascii_case("notifications") && !s.starts_with('.') && is_safe_segment(s)
}

pub(crate) const NOTIFICATION_PAGE_SIZE: u64 = 50;
