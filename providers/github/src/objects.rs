//! GitHub object bodies and field projections.

use omnifs_sdk::prelude::*;
use omnifs_sdk::repr::{Markdown, Representable};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::User;

/// An issue or a pull request as a node of GitHub GraphQL with the selection
/// that the feeds ask for: the canonical bytes of `Issue` and `PullRequest`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ItemData {
    pub(crate) number: u64,
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) state: String,
    /// The author, `null` for an account that GitHub deleted.
    pub(crate) author: Option<User>,
    #[serde(with = "time::serde::rfc3339")]
    pub(crate) updated_at: OffsetDateTime,
    pub(crate) comments: Total,
}

/// The `totalCount` of a connection of GitHub GraphQL.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Total {
    pub(crate) total_count: u64,
}

impl ItemData {
    /// The login of the author, empty for an item without one.
    pub(crate) fn login(&self) -> &str {
        self.author.as_ref().map_or("", |u| u.login.as_str())
    }

    pub(crate) fn markdown(&self) -> Vec<u8> {
        format!(
            "# {}\n\n- Number: {}\n- State: {}\n- User: {}\n\n{}\n",
            self.title,
            self.number,
            self.state,
            self.login(),
            self.body
        )
        .into_bytes()
    }
}

#[omnifs_sdk::object(kind = "github.issue", key = crate::item::IssueKey)]
#[derive(Clone, Debug, Deserialize)]
#[serde(transparent)]
pub(crate) struct Issue(pub(crate) ItemData);

impl Representable<Markdown> for Issue {
    fn represent(&self) -> Vec<u8> {
        self.0.markdown()
    }
}

/// A pull request as a node of GitHub GraphQL: its item.
#[omnifs_sdk::object(kind = "github.pull", key = crate::item::PullKey)]
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PullRequest {
    #[serde(flatten)]
    pub(crate) data: ItemData,
}

impl Representable<Markdown> for PullRequest {
    fn represent(&self) -> Vec<u8> {
        self.data.markdown()
    }
}

#[omnifs_sdk::object(kind = "github.review", key = crate::item::ReviewKey)]
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Review {
    pub(crate) id: u64,
    pub(crate) user: Option<User>,
    pub(crate) body: String,
    pub(crate) state: String,
    pub(crate) submitted_at: Option<String>,
}

impl Review {
    /// The login of the author, empty for a review without one.
    pub(crate) fn login(&self) -> &str {
        self.user.as_ref().map_or("", |u| u.login.as_str())
    }
}

impl Representable<Markdown> for Review {
    fn represent(&self) -> Vec<u8> {
        let user = self.login();
        let Self {
            id, state, body, ..
        } = self;
        let submitted = self
            .submitted_at
            .as_ref()
            .map(|submitted| format!("- Submitted: {submitted}\n"))
            .unwrap_or_default();
        format!("# Review {id}\n\n- State: {state}\n- User: {user}\n{submitted}\n{body}\n")
            .into_bytes()
    }
}

/// An inline comment of a review, without the code that GitHub shows
/// beside it.
#[omnifs_sdk::object(kind = "github.review_comment", key = crate::item::ReviewCommentKey)]
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ReviewComment {
    pub(crate) id: u64,
    pub(crate) user: Option<User>,
    pub(crate) body: String,
    pub(crate) path: String,
}

impl ReviewComment {
    /// The login of the author, empty for a comment without one.
    pub(crate) fn login(&self) -> &str {
        self.user.as_ref().map_or("", |u| u.login.as_str())
    }
}

impl Representable<Markdown> for ReviewComment {
    fn represent(&self) -> Vec<u8> {
        let user = self.login();
        let Self { path, body, .. } = self;
        format!("{user} on `{path}`:\n\n{body}\n").into_bytes()
    }
}

/// A GitHub owner (user or organization) profile. The upstream profile JSON is
/// the canonical payload; `profile.md` renders it.
#[omnifs_sdk::object(kind = "github.owner", key = crate::item::OwnerKey)]
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Owner {
    pub(crate) login: String,
    #[serde(rename = "type")]
    pub(crate) kind: String,
    pub(crate) name: Option<String>,
    pub(crate) bio: Option<String>,
}

impl Representable<Markdown> for Owner {
    fn represent(&self) -> Vec<u8> {
        let Self { login, kind, .. } = self;
        let name = self
            .name
            .as_ref()
            .map(|name| format!("- Name: {name}\n"))
            .unwrap_or_default();
        let bio = self
            .bio
            .as_ref()
            .map(|bio| format!("\n{bio}\n"))
            .unwrap_or_default();
        format!("# {login}\n\n- Type: {kind}\n{name}{bio}").into_bytes()
    }
}

#[omnifs_sdk::object(kind = "github.repo", key = crate::item::RepoKey)]
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Repo {}

/// One issue or pull request comment, keyed by its own `comment_id`.
#[omnifs_sdk::object(kind = "github.comment", key = crate::item::CommentKey)]
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Comment {
    pub(crate) id: u64,
    /// The author, `null` for an account that GitHub deleted.
    pub(crate) user: Option<User>,
    pub(crate) body: String,
}

impl Comment {
    /// The login of the author, empty for a comment without one.
    pub(crate) fn login(&self) -> &str {
        self.user.as_ref().map_or("", |user| user.login.as_str())
    }
}

impl Representable<Markdown> for Comment {
    fn represent(&self) -> Vec<u8> {
        format!("{}:\n{}\n", self.login(), self.body).into_bytes()
    }
}

#[omnifs_sdk::object(kind = "github.notification", key = crate::item::NotificationKey)]
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Notification {
    pub(crate) id: String,
    pub(crate) unread: bool,
    pub(crate) reason: String,
    pub(crate) updated_at: String,
    pub(crate) subject: NotificationSubject,
    pub(crate) repository: NotificationRepo,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct NotificationSubject {
    pub(crate) title: String,
    #[serde(rename = "type")]
    pub(crate) kind: String,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct NotificationRepo {
    pub(crate) full_name: String,
}

impl Notification {
    pub(crate) fn reason(&self, _key: &crate::item::NotificationKey) -> Result<FileProjection> {
        Ok(FileProjection::text(self.reason.clone(), TextFormat::Raw).build())
    }

    pub(crate) fn subject(&self, _key: &crate::item::NotificationKey) -> Result<FileProjection> {
        Ok(FileProjection::text(self.subject.title.clone(), TextFormat::Raw).build())
    }

    pub(crate) fn item_markdown(&self) -> Vec<u8> {
        format!(
            "# {}\n\n- Type: {}\n- Repository: {}\n- Reason: {}\n- Unread: {}\n- Updated: {}\n",
            self.subject.title,
            self.subject.kind,
            self.repository.full_name,
            self.reason,
            self.unread,
            self.updated_at
        )
        .into_bytes()
    }
}

impl Representable<Markdown> for Notification {
    fn represent(&self) -> Vec<u8> {
        self.item_markdown()
    }
}
