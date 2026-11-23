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

use objects::{
    ChangedFile, Comment, Issue, Notification, Owner, PullRequest, Repo, Review, ReviewComment,
};

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
    projection = 6,
    capabilities(
        domain(
            "api.github.com",
            "Fetch GitHub API resources such as repository metadata, issues, pull requests, and notifications."
        ),
        git_repo(
            "git@github.com:*",
            "Clone repository contents over SSH when browsing repo paths."
        ),
    ),
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
            o.dir("repo").tree(Repo::tree)?;
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
            o.file("diff.patch").blob(PullRequest::diff)?;
            o.dir("comments").fed()?;
            o.dir("files").collection(PullRequest::files)?;
            o.dir("reviews").collection(PullRequest::reviews)?;
            o.feed(PullRequest::feed)
                .starts(["", "title", "body", "state", "user", "comments"])?;
            Ok(())
        })?;

        r.object::<ChangedFile>(
            "/{owner}/{repo}/pulls/{filter}/{number}/files/{path}",
            |o| {
                o.dynamic();
                o.file("file.json").canonical::<Json>()?;
                o.file("file.md").representation::<Markdown>()?;
                o.file("filename").computed(ChangedFile::filename)?;
                o.file("status").computed(ChangedFile::status)?;
                o.file("patch.patch").lazy().computed(ChangedFile::patch)?;
                Ok(())
            },
        )?;

        r.object::<Review>(
            "/{owner}/{repo}/pulls/{filter}/{number}/reviews/{review_id}",
            |o| {
                o.dynamic();
                o.file("review.json").canonical::<Json>()?;
                o.file("review.md").representation::<Markdown>()?;
                o.file("state").computed(Review::state)?;
                o.file("user").computed(Review::user)?;
                o.file("body.md").lazy().computed(Review::body_md)?;
                o.dir("comments").collection(Review::comments)?;
                Ok(())
            },
        )?;

        r.object::<ReviewComment>(
            "/{owner}/{repo}/pulls/{filter}/{number}/reviews/{review_id}/comments/{comment_id}",
            |o| {
                o.dynamic();
                o.file("comment.json").canonical::<Json>()?;
                o.file("comment.md").representation::<Markdown>()?;
                o.file("body.md").lazy().computed(ReviewComment::body_md)?;
                o.file("author").computed(ReviewComment::author)?;
                o.file("path").computed(ReviewComment::path)?;
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

#[omnifs_sdk::path_segment(validate = FilePath::is_valid_segment)]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct FilePath(String);

impl FilePath {
    pub(crate) fn decoded(&self) -> Result<String> {
        let bytes = Self::decode_bytes(&self.0)?;
        String::from_utf8(bytes)
            .map_err(|err| ProviderError::invalid_input(format!("file path is not utf-8: {err}")))
    }

    pub(crate) fn is_valid_segment(s: &str) -> bool {
        if s.is_empty() || s == "." || s == ".." || s.as_bytes().contains(&b'/') {
            return false;
        }
        let bytes = s.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'%' => {
                    if i + 2 >= bytes.len()
                        || !bytes[i + 1].is_ascii_hexdigit()
                        || !bytes[i + 2].is_ascii_hexdigit()
                    {
                        return false;
                    }
                    i += 3;
                },
                b if Self::is_unreserved_path_byte(b) => i += 1,
                _ => return false,
            }
        }
        true
    }

    /// The segment of a path of a file of GitHub: each byte other than an
    /// unreserved one as `%XX`.
    pub(crate) fn encode(path: &str) -> String {
        let mut encoded = String::with_capacity(path.len());
        for byte in path.bytes() {
            if Self::is_unreserved_path_byte(byte) {
                encoded.push(char::from(byte));
            } else {
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                encoded.push('%');
                encoded.push(char::from(HEX[(byte >> 4) as usize]));
                encoded.push(char::from(HEX[(byte & 0x0F) as usize]));
            }
        }
        if encoded == "." {
            "%2E".to_string()
        } else if encoded == ".." {
            "%2E%2E".to_string()
        } else {
            encoded
        }
    }

    fn decode_bytes(s: &str) -> Result<Vec<u8>> {
        let bytes = s.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' {
                let hi = Self::hex_value(bytes[i + 1])?;
                let lo = Self::hex_value(bytes[i + 2])?;
                out.push((hi << 4) | lo);
                i += 3;
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        Ok(out)
    }

    const fn is_unreserved_path_byte(byte: u8) -> bool {
        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~')
    }

    fn hex_value(byte: u8) -> Result<u8> {
        match byte {
            b'0'..=b'9' => Ok(byte - b'0'),
            b'a'..=b'f' => Ok(byte - b'a' + 10),
            b'A'..=b'F' => Ok(byte - b'A' + 10),
            _ => Err(ProviderError::invalid_input(
                "bad percent-encoded file path",
            )),
        }
    }
}

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

#[derive(Clone, Debug, Deserialize)]
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

pub(crate) const FILE_PAGE_SIZE: u64 = 100;
pub(crate) const REVIEW_PAGE_SIZE: u64 = 100;
pub(crate) const NOTIFICATION_PAGE_SIZE: u64 = 50;
