//! GitHub object bodies and field projections.

use omnifs_core::ContentType;
use omnifs_sdk::prelude::*;
use omnifs_sdk::repr::{Markdown, Representable};
use serde::Deserialize;
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

#[omnifs_sdk::object(kind = "github.pull", key = crate::item::PullKey)]
#[derive(Clone, Debug, Deserialize)]
#[serde(transparent)]
pub(crate) struct PullRequest(pub(crate) ItemData);

impl Representable<Markdown> for PullRequest {
    fn represent(&self) -> Vec<u8> {
        self.0.markdown()
    }
}

#[omnifs_sdk::object(kind = "github.pull_file", key = crate::item::ChangedFileKey)]
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct ChangedFile {
    pub(crate) filename: String,
    pub(crate) status: String,
    pub(crate) additions: u64,
    pub(crate) deletions: u64,
    pub(crate) changes: u64,
    pub(crate) patch: Option<String>,
    pub(crate) previous_filename: Option<String>,
}

impl ChangedFile {
    pub(crate) fn filename(&self, _key: &crate::item::ChangedFileKey) -> Result<FileProjection> {
        Ok(FileProjection::text(self.filename.clone(), TextFormat::Raw).build())
    }

    pub(crate) fn status(&self, _key: &crate::item::ChangedFileKey) -> Result<FileProjection> {
        Ok(FileProjection::text(self.status.clone(), TextFormat::Raw).build())
    }

    pub(crate) fn patch(&self, _key: &crate::item::ChangedFileKey) -> Result<FileProjection> {
        let patch = self.patch.as_deref().unwrap_or("");
        Ok(FileProjection::body(patch.to_owned())
            .content_type(ContentType::Custom("text/x-diff"))
            .build())
    }
}

impl Representable<Markdown> for ChangedFile {
    fn represent(&self) -> Vec<u8> {
        let previous = self
            .previous_filename
            .as_ref()
            .map_or(String::new(), |name| {
                format!("- Previous filename: {name}\n")
            });
        format!(
            "# {}\n\n- Status: {}\n- Additions: {}\n- Deletions: {}\n- Changes: {}\n{}",
            self.filename, self.status, self.additions, self.deletions, self.changes, previous
        )
        .into_bytes()
    }
}

#[omnifs_sdk::object(kind = "github.review", key = crate::item::ReviewKey)]
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Review {
    pub(crate) id: u64,
    pub(crate) user: Option<User>,
    pub(crate) body: String,
    pub(crate) state: String,
    pub(crate) submitted_at: Option<String>,
}

impl Review {
    pub(crate) fn state(&self, _key: &crate::item::ReviewKey) -> Result<FileProjection> {
        Ok(FileProjection::text(self.state.clone(), TextFormat::Raw).build())
    }

    pub(crate) fn user(&self, _key: &crate::item::ReviewKey) -> Result<FileProjection> {
        let login = self.user.as_ref().map_or("", |u| u.login.as_str());
        Ok(FileProjection::text(login.to_owned(), TextFormat::Raw).build())
    }

    pub(crate) fn body_md(&self, _key: &crate::item::ReviewKey) -> Result<FileProjection> {
        Ok(FileProjection::body(self.body.clone())
            .content_type(ContentType::Markdown)
            .build())
    }
}

impl Representable<Markdown> for Review {
    fn represent(&self) -> Vec<u8> {
        let user = self.user.as_ref().map_or("", |u| u.login.as_str());
        let Self {
            id, state, body, ..
        } = self;
        let submitted = self.submitted_at.as_deref().unwrap_or("");
        format!(
            "# Review {id}\n\n- State: {state}\n- User: {user}\n- Submitted: {submitted}\n\n{body}\n"
        )
        .into_bytes()
    }
}

#[omnifs_sdk::object(kind = "github.review_comment", key = crate::item::ReviewCommentKey)]
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct ReviewComment {
    pub(crate) id: u64,
    pub(crate) user: Option<User>,
    pub(crate) body: String,
    pub(crate) path: String,
    pub(crate) diff_hunk: String,
}

impl ReviewComment {
    pub(crate) fn body_md(&self, _key: &crate::item::ReviewCommentKey) -> Result<FileProjection> {
        Ok(FileProjection::body(self.body.clone())
            .content_type(ContentType::Markdown)
            .build())
    }

    pub(crate) fn author(&self, _key: &crate::item::ReviewCommentKey) -> Result<FileProjection> {
        let login = self.user.as_ref().map_or("", |u| u.login.as_str());
        Ok(FileProjection::text(login.to_owned(), TextFormat::Raw).build())
    }

    pub(crate) fn path(&self, _key: &crate::item::ReviewCommentKey) -> Result<FileProjection> {
        Ok(FileProjection::text(self.path.clone(), TextFormat::Raw).build())
    }
}

impl Representable<Markdown> for ReviewComment {
    fn represent(&self) -> Vec<u8> {
        let user = self.user.as_ref().map_or("", |u| u.login.as_str());
        let Self {
            path,
            body,
            diff_hunk,
            ..
        } = self;
        format!("{user} on `{path}`:\n\n{body}\n\n```diff\n{diff_hunk}\n```\n").into_bytes()
    }
}

#[omnifs_sdk::object(kind = "github.check", key = crate::item::CheckRunKey)]
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct CheckRun {
    pub(crate) id: u64,
    pub(crate) name: String,
    pub(crate) status: String,
    pub(crate) conclusion: Option<String>,
    pub(crate) html_url: Option<String>,
    pub(crate) output: CheckOutput,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct CheckOutput {
    pub(crate) title: Option<String>,
    pub(crate) summary: Option<String>,
    pub(crate) text: Option<String>,
}

impl CheckRun {
    pub(crate) fn name(&self, _key: &crate::item::CheckRunKey) -> Result<FileProjection> {
        Ok(FileProjection::text(self.name.clone(), TextFormat::Raw).build())
    }

    pub(crate) fn status(&self, _key: &crate::item::CheckRunKey) -> Result<FileProjection> {
        Ok(FileProjection::text(self.status.clone(), TextFormat::Raw).build())
    }

    pub(crate) fn conclusion(&self, _key: &crate::item::CheckRunKey) -> Result<FileProjection> {
        Ok(
            FileProjection::text(self.conclusion.clone().unwrap_or_default(), TextFormat::Raw)
                .build(),
        )
    }

    pub(crate) fn summary_md(&self, _key: &crate::item::CheckRunKey) -> Result<FileProjection> {
        Ok(FileProjection::body(self.summary_markdown())
            .content_type(ContentType::Markdown)
            .build())
    }

    fn summary_markdown(&self) -> String {
        let output = &self.output;
        let mut summary = String::new();
        if let Some(title) = &output.title {
            summary.push_str("## ");
            summary.push_str(title);
            summary.push_str("\n\n");
        }
        if let Some(body) = &output.summary {
            summary.push_str(body);
            summary.push('\n');
        }
        if let Some(text) = &output.text {
            if !summary.ends_with("\n\n") {
                summary.push('\n');
            }
            summary.push_str(text);
            summary.push('\n');
        }
        summary
    }
}

impl Representable<Markdown> for CheckRun {
    fn represent(&self) -> Vec<u8> {
        let conclusion = self.conclusion.as_deref().unwrap_or("");
        let url = self.html_url.as_deref().unwrap_or("");
        format!(
            "# {}\n\n- Status: {}\n- Conclusion: {conclusion}\n- URL: {url}\n\n{}",
            self.name,
            self.status,
            self.summary_markdown()
        )
        .into_bytes()
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
        let name = self.name.as_deref().unwrap_or("");
        let bio = self.bio.as_deref().unwrap_or("");
        format!("# {login}\n\n- Type: {kind}\n- Name: {name}\n\n{bio}\n").into_bytes()
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
    pub(crate) user: Option<User>,
    pub(crate) body: Option<String>,
}

impl Comment {
    /// The login of the author, empty for a comment without one.
    pub(crate) fn login(&self) -> &str {
        self.user.as_ref().map_or("", |user| user.login.as_str())
    }

    /// The body, empty for a comment without one.
    pub(crate) fn body_text(&self) -> &str {
        self.body.as_deref().unwrap_or("")
    }
}

impl Representable<Markdown> for Comment {
    fn represent(&self) -> Vec<u8> {
        format!("{}:\n{}\n", self.login(), self.body_text()).into_bytes()
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

#[omnifs_sdk::object(kind = "github.run", key = crate::item::RunKey)]
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct WorkflowRun {
    pub(crate) id: u64,
    pub(crate) status: String,
    pub(crate) conclusion: Option<String>,
}

impl WorkflowRun {
    pub(crate) fn status(&self, _key: &crate::item::RunKey) -> Result<FileProjection> {
        Ok(FileProjection::text(self.status.clone(), TextFormat::Raw).build())
    }

    pub(crate) fn conclusion(&self, _key: &crate::item::RunKey) -> Result<FileProjection> {
        let c = self.conclusion.clone().unwrap_or_default();
        Ok(FileProjection::text(c, TextFormat::Raw).build())
    }
}
