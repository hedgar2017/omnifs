#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

//! slack-provider: the users, the channels, and the messages of a workspace
//! of Slack for omnifs, as its bot sees them.
//!
//! The mount has `users/{id}/` for each user with `name` and `user.json`,
//! and `channels/{id}/` for each public and private channel that the bot is
//! in, archived too, with `name`, `channel.json`, and `messages/{ts}/` for
//! each message of its history with `author`, `text`, `message.json`,
//! `replies/{ts}/` with `author`, `text`, and `message.json` of each reply of
//! its thread, and `files/{id}/` with `name` and `content` of each file of a
//! message or a reply, by the ID of Slack of a user, a channel, or a file,
//! and the `ts` of a message or a reply. Their facts come from feeds alone:
//! the feeds of full listings of the users and of the channels, the feed of
//! the history of each channel, which sweeps its pages, and the own feeds of
//! a message, which takes it by its `ts` and its replies, and of a file,
//! which takes its content.

use std::future::Future;

use omnifs_sdk::prelude::*;
use serde::{Deserialize, Serialize};

pub(crate) use omnifs_sdk::prelude::Result;

mod api;
mod feed;

/// The ID of a user, a channel, or a file of Slack: ASCII uppercase letters
/// and digits.
#[omnifs_sdk::path_segment(validate = is_id)]
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Id(String);

impl From<Id> for String {
    fn from(id: Id) -> Self {
        id.0
    }
}

impl TryFrom<String> for Id {
    type Error = String;

    fn try_from(id: String) -> std::result::Result<Self, Self::Error> {
        id.parse()
            .map_err(|()| format!("{id} is not an ID of Slack"))
    }
}

fn is_id(segment: &str) -> bool {
    !segment.is_empty()
        && segment
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
}

/// The `ts` of a message or a reply of Slack: digits, `.`, and digits. Slack
/// gives each with ten digits before its `.` and six after it, so that the
/// order of their text is their order in time.
#[omnifs_sdk::path_segment(validate = is_ts)]
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Ts(String);

impl TryFrom<String> for Ts {
    type Error = String;

    fn try_from(ts: String) -> std::result::Result<Self, Self::Error> {
        ts.parse()
            .map_err(|()| format!("{ts} is not a ts of Slack"))
    }
}

impl From<Ts> for String {
    fn from(ts: Ts) -> Self {
        ts.0
    }
}

fn is_ts(segment: &str) -> bool {
    segment.split_once('.').is_some_and(|(seconds, fraction)| {
        [seconds, fraction]
            .iter()
            .all(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
    })
}

#[omnifs_sdk::path_captures]
pub(crate) struct WorkspaceKey {}

#[omnifs_sdk::path_captures]
pub(crate) struct UserKey {
    pub(crate) id: Id,
}

#[omnifs_sdk::path_captures]
pub(crate) struct ChannelKey {
    pub(crate) id: Id,
}

#[omnifs_sdk::path_captures]
pub(crate) struct MessageKey {
    pub(crate) id: Id,
    pub(crate) ts: Ts,
}

#[omnifs_sdk::path_captures]
pub(crate) struct ReplyKey {
    pub(crate) id: Id,
    pub(crate) ts: Ts,
    pub(crate) reply: Ts,
}

#[omnifs_sdk::path_captures]
pub(crate) struct FileKey {
    pub(crate) id: Id,
    pub(crate) ts: Ts,
    pub(crate) file: Id,
}

#[omnifs_sdk::path_captures]
pub(crate) struct ReplyFileKey {
    pub(crate) id: Id,
    pub(crate) ts: Ts,
    pub(crate) reply: Ts,
    pub(crate) file: Id,
}

/// The workspace of the bot, with its users and the channels that it is in.
#[omnifs_sdk::object(kind = "slack.workspace", key = WorkspaceKey, load = fed_only)]
#[derive(Deserialize)]
pub(crate) struct Workspace {}

/// The users of the workspace, the object of the listing of `users/`.
#[omnifs_sdk::object(kind = "slack.users", key = WorkspaceKey, load = fed_only)]
#[derive(Deserialize)]
pub(crate) struct Users {}

/// A user of the workspace.
#[omnifs_sdk::object(kind = "slack.user", key = UserKey, load = fed_only)]
#[derive(Deserialize)]
pub(crate) struct User {}

/// The channels that the bot is in, the object of the listing of
/// `channels/`.
#[omnifs_sdk::object(kind = "slack.channels", key = WorkspaceKey, load = fed_only)]
#[derive(Deserialize)]
pub(crate) struct Channels {}

/// A public or private channel that the bot is in.
#[omnifs_sdk::object(kind = "slack.channel", key = ChannelKey, load = fed_only)]
#[derive(Deserialize)]
pub(crate) struct Channel {}

/// The history of a channel, the object of the listing of its `messages/`.
#[omnifs_sdk::object(kind = "slack.history", key = ChannelKey, load = fed_only)]
#[derive(Deserialize)]
pub(crate) struct History {}

/// A message of the history of a channel.
#[omnifs_sdk::object(kind = "slack.message", key = MessageKey, load = fed_only)]
#[derive(Deserialize)]
pub(crate) struct Message {}

/// A reply of the thread of a message.
#[omnifs_sdk::object(kind = "slack.reply", key = ReplyKey, load = fed_only)]
#[derive(Deserialize)]
pub(crate) struct Reply {}

/// A file of a message.
#[omnifs_sdk::object(kind = "slack.file", key = FileKey, load = fed_only)]
#[derive(Deserialize)]
pub(crate) struct File {}

/// A file of a reply.
#[omnifs_sdk::object(kind = "slack.reply_file", key = ReplyFileKey, load = fed_only)]
#[derive(Deserialize)]
pub(crate) struct ReplyFile {}

/// The load of an object of the mount, which no face asks for: the objects
/// have no canonical bytes, and the facts of their faces come from feeds
/// alone.
fn fed_only<O: Object>(
    _cx: &Cx<O::State>,
    _key: &O::Key,
    _since: Option<Validator>,
) -> impl Future<Output = Result<Load<O>>> {
    std::future::ready(Err(ProviderError::internal(format!(
        "the object {} has no canonical bytes",
        O::kind().as_str()
    ))))
}

#[omnifs_sdk::provider(
    id = "slack",
    display_name = "Slack",
    description = "users, channels, messages, and files from your workspace",
    mount = "slack",
    projection = 2,
    capabilities(
        domain(
            "slack.com",
            "Fetch the users, the channels, the messages, and the replies of a workspace of Slack."
        ),
        domain(
            "files.slack.com",
            "Fetch the content of the files of the messages and the replies of a workspace of Slack."
        ),
    ),
    limits(memory_mb(
        256,
        "Leave room for a page of 999 objects of Slack and the replies of a message that a feed token keeps."
    ),),
    auth = r#"{"default":"token","schemes":[{"staticToken":{"key":"token","valuePrefix":"Bearer ","description":"Slack bot token","injectDomains":["slack.com","files.slack.com"],"validation":{"method":"POST","url":"https://slack.com/api/auth.test","expectStatus":200,"jsonPointer":"/bot_id"}}}]}"#
)]
impl SlackProvider {
    fn start(r: &mut Router) -> Result<()> {
        r.object::<Workspace>("/", |o| {
            o.dynamic();
            o.dir("users").fed()?;
            o.dir("channels").fed()?;
            Ok(())
        })?;

        r.object::<Users>("/users", |o| {
            o.dynamic();
            o.feed(Users::feed).full_listings().starts([
                "",
                "{id}",
                "{id}/name",
                "{id}/user.json",
            ])?;
            Ok(())
        })?;

        r.object::<User>("/users/{id}", |o| {
            o.dynamic();
            o.fed();
            o.file("name").fed()?;
            o.file("user.json").fed()?;
            Ok(())
        })?;

        r.object::<Channels>("/channels", |o| {
            o.dynamic();
            o.feed(Channels::feed).full_listings().starts([
                "",
                "{id}",
                "{id}/name",
                "{id}/channel.json",
            ])?;
            Ok(())
        })?;

        r.object::<Channel>("/channels/{id}", |o| {
            o.dynamic();
            o.fed();
            o.file("name").fed()?;
            o.file("channel.json").fed()?;
            o.dir("messages").fed()?;
            Ok(())
        })?;

        r.object::<History>("/channels/{id}/messages", |o| {
            o.dynamic();
            o.feed(History::feed).starts([""])?;
            Ok(())
        })?;

        // No start of the anchor or of an own face: no lookup of a message
        // asks its feed after the end of the first sweep of its channel.
        r.object::<Message>("/channels/{id}/messages/{ts}", |o| {
            o.dynamic();
            o.fed();
            o.file("author").fed()?;
            o.file("text").fed()?;
            o.file("message.json").fed()?;
            o.dir("replies").fed()?;
            o.dir("files").fed()?;
            o.feed(Message::feed).own().starts([
                "replies",
                "replies/{reply}",
                "replies/{reply}/author",
                "replies/{reply}/text",
                "replies/{reply}/message.json",
                "replies/{reply}/files",
                "replies/{reply}/files/{file}",
                "replies/{reply}/files/{file}/name",
            ])?;
            Ok(())
        })?;

        r.object::<Reply>("/channels/{id}/messages/{ts}/replies/{reply}", |o| {
            o.dynamic();
            o.fed();
            o.file("author").fed()?;
            o.file("text").fed()?;
            o.file("message.json").fed()?;
            o.dir("files").fed()?;
            Ok(())
        })?;

        r.object::<File>("/channels/{id}/messages/{ts}/files/{file}", |o| {
            o.dynamic();
            o.fed();
            o.file("name").fed()?;
            o.file("content").fed()?;
            o.feed(File::feed).own().starts(["content"])?;
            Ok(())
        })?;

        r.object::<ReplyFile>(
            "/channels/{id}/messages/{ts}/replies/{reply}/files/{file}",
            |o| {
                o.dynamic();
                o.fed();
                o.file("name").fed()?;
                o.file("content").fed()?;
                o.feed(ReplyFile::feed).own().starts(["content"])?;
                Ok(())
            },
        )?;

        Ok(())
    }
}
