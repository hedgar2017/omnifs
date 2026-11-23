#![cfg(target_arch = "wasm32")]

//! notion-provider: the pages, data sources, and comments of a Notion
//! workspace for omnifs.
//!
//! The mount has `pages/{id}/` for each page, a row of a data source too,
//! with `title`, `page.json`, `body.md`, and `comments/{id}/` with `author`,
//! `body.md`, and `comment.json` of each comment, and `databases/{id}/` for
//! each data source with `title` and `schema.json`, by the 32 hexadecimal
//! digits of the ID of Notion, as at the end of a link of Notion. Their facts
//! come from feeds alone: the feed of the workspace lists the pages and the
//! data sources and takes their changes from the search of Notion, and the
//! own feeds of a page and of a data source take what the search does not
//! bring, the content and the comments of a page among them.

use omnifs_sdk::prelude::*;
use serde::{Deserialize, Serialize};

pub(crate) use omnifs_sdk::prelude::Result;

mod api;
mod feed;

/// The ID of a page, a data source, or a comment: the 32 lowercase
/// hexadecimal digits of the ID of an answer of Notion without its hyphens.
#[omnifs_sdk::path_segment(validate = is_id)]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Id(String);

impl TryFrom<String> for Id {
    type Error = String;

    fn try_from(id: String) -> std::result::Result<Self, Self::Error> {
        id.parse()
            .map_err(|()| format!("{id} is not an ID of Notion"))
    }
}

impl From<Id> for String {
    fn from(id: Id) -> Self {
        id.0
    }
}

fn is_id(segment: &str) -> bool {
    segment.len() == 32
        && segment
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

impl Id {
    /// The ID of an object of an answer of Notion, without its hyphens.
    pub(crate) fn of(id: &str) -> Result<Self> {
        id.replace('-', "")
            .parse()
            .map_err(|()| ProviderError::invalid_input(format!("the ID {id} of Notion")))
    }
}

#[omnifs_sdk::path_captures]
pub(crate) struct WorkspaceKey {}

#[omnifs_sdk::path_captures]
pub(crate) struct PageKey {
    pub(crate) id: Id,
}

#[omnifs_sdk::path_captures]
pub(crate) struct CommentKey {
    pub(crate) id: Id,
    pub(crate) comment: Id,
}

#[omnifs_sdk::path_captures]
pub(crate) struct DataSourceKey {
    pub(crate) id: Id,
}

/// The workspace of the connection, with the pages and the data sources
/// that it can read.
#[omnifs_sdk::object(kind = "notion.workspace", key = WorkspaceKey, no_load)]
#[derive(Deserialize)]
pub(crate) struct Workspace {}

/// A page of the workspace, a row of a data source too.
#[omnifs_sdk::object(kind = "notion.page", key = PageKey, no_load)]
#[derive(Deserialize)]
pub(crate) struct Page {}

/// A comment of a page.
#[omnifs_sdk::object(kind = "notion.comment", key = CommentKey, no_load)]
#[derive(Deserialize)]
pub(crate) struct Comment {}

/// A data source of a database of the workspace.
#[omnifs_sdk::object(kind = "notion.data_source", key = DataSourceKey, no_load)]
#[derive(Deserialize)]
pub(crate) struct DataSource {}

#[omnifs_sdk::provider(
    id = "notion",
    display_name = "Notion",
    description = "pages, data sources, and comments from your workspace",
    mount = "notion",
    projection = 4,
    capabilities(domain(
        "api.notion.com",
        "Fetch the pages, data sources, page content, and comments of a Notion workspace."
    ),),
    limits(memory_mb(
        256,
        "Leave room for the markdown of large pages and the comments of a page."
    ),),
    auth = r#"{"default":"token","schemes":[{"staticToken":{"key":"token","valuePrefix":"Bearer ","description":"Notion internal connection token","injectDomains":["api.notion.com"],"validation":{"method":"GET","url":"https://api.notion.com/v1/users/me","headers":{"Notion-Version":"2026-03-11"},"expectStatus":200}}}]}"#
)]
impl NotionProvider {
    fn start(r: &mut Router) -> Result<()> {
        r.object::<Workspace>("/", |o| {
            o.dynamic();
            o.dir("pages").fed()?;
            o.dir("databases").fed()?;
            o.feed(Workspace::feed).starts(["pages", "databases"])?;
            Ok(())
        })?;

        r.object::<Page>("/pages/{id}", |o| {
            o.dynamic();
            o.fed();
            o.file("title").fed()?;
            o.file("page.json").fed()?;
            o.file("body.md").fed()?;
            o.dir("comments").fed()?;
            o.feed(Page::feed).own().starts([
                "",
                "title",
                "page.json",
                "body.md",
                "comments",
                "comments/{comment}",
                "comments/{comment}/author",
                "comments/{comment}/body.md",
                "comments/{comment}/comment.json",
            ])?;
            Ok(())
        })?;

        r.object::<Comment>("/pages/{id}/comments/{comment}", |o| {
            o.dynamic();
            o.fed();
            o.file("author").fed()?;
            o.file("body.md").fed()?;
            o.file("comment.json").fed()?;
            Ok(())
        })?;

        r.object::<DataSource>("/databases/{id}", |o| {
            o.dynamic();
            o.fed();
            o.file("title").fed()?;
            o.file("schema.json").fed()?;
            o.feed(DataSource::feed)
                .own()
                .starts(["", "title", "schema.json"])?;
            Ok(())
        })?;

        Ok(())
    }
}
