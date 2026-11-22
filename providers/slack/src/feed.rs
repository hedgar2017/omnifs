//! The feeds of the users, the channels, the messages, the replies, and the
//! files of the workspace.
//!
//! The feeds of the users and of the channels are feeds of full listings of
//! `users.list` and of `users.conversations`: each call takes a page and
//! writes its objects, with their IDs in the token until the last page, which
//! writes the listing complete and removes each other entry of it.
//!
//! The feed of the history of a channel takes a part for each page of
//! `conversations.history`, the newest messages first, in a sweep. The first
//! sweep and the sweep of a read of `messages/` past the deadline of the feed
//! take each page; the sweep of each other operation in the channel past that
//! deadline takes the pages down to the latest message of the sweep before
//! it, one page unless the channel has more new messages than a page. A sweep
//! writes each message that is not the `message.json` of the projection, and
//! removes each message of the projection that the pages lack between the
//! oldest and the newest message of the pages, and older ones too after the
//! last page of the history. The first sweep gives the feed of each message
//! its state, so that the feed of a message takes it by its `ts` past its
//! deadline before the end of that sweep.
//!
//! Slack tells an edit of a reply in `conversations.replies` alone, so the
//! `replies/` of a message leave the projection, for their next read to take
//! them once more, for another `latest_reply` or `reply_count` of the message
//! in each sweep, and for each message with replies in a sweep of each page
//! after the first one. The own feed of a message takes it by its `ts`, and,
//! for a `replies/` that the projection has not complete, its replies with
//! the message itself from the pages of `conversations.replies`, kept in its
//! token until the last page, and the own feed of a file of a message or a
//! reply takes its content from the URL that the feed that took the file
//! gives it.
//!
//! The take of a message or a reply writes its files against its
//! `message.json` in the projection, with the removal of the `content` of a
//! file with another `edit_timestamp`, which its next read takes once more.

use std::collections::BTreeSet;
use std::str::FromStr;

use omnifs_sdk::prelude::*;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::api::{
    ConversationObject, MessageObject, Refusal, Refused, SlackApi, UserObject, decode, page,
    rate_limited,
};
use crate::{
    ChannelKey, Channels, File, FileKey, History, Id, Message, MessageKey, ReplyFile, ReplyFileKey,
    Ts, Users, WorkspaceKey,
};

/// The objects of a page of `users.list`, `users.conversations`, and
/// `conversations.history`: the most below 1000 that Slack gives.
const PAGE: usize = 999;

/// The messages of a page of `conversations.replies`: the most that Slack
/// gives.
const REPLIES: usize = 1000;

// ===========================================================================
// Tokens
// ===========================================================================

/// The token of the feed of the users or of the channels: the next page of
/// an unfinished full listing, none for the first page of the next one.
type ListingToken = Option<ListingPage>;

/// The next page of an unfinished full listing: the `next_cursor` of the page
/// before it, and the IDs of the objects of the pages so far.
#[derive(Serialize, Deserialize)]
struct ListingPage {
    cursor: String,
    ids: BTreeSet<Id>,
}

/// The token of the feed of the history of a channel: the `ts` of the latest
/// message of its last sweep, none for a channel without one, and the stage
/// of an unfinished sweep.
#[derive(Serialize, Deserialize)]
struct HistoryToken {
    latest: Option<Ts>,
    stage: Option<Stage>,
}

/// The stage of an unfinished sweep of the history of a channel: the sweep,
/// the cursor of its next page, none for its first one, and the `ts` of the
/// messages of its pages so far.
#[derive(Serialize, Deserialize)]
struct Stage {
    sweep: Sweep,
    cursor: Option<String>,
    messages: BTreeSet<Ts>,
}

impl Stage {
    /// The stage of the first page of `sweep`.
    fn first(sweep: Sweep) -> Self {
        Self {
            sweep,
            cursor: None,
            messages: BTreeSet::new(),
        }
    }
}

/// A sweep of the history of a channel.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Sweep {
    /// The first one, of each page, whose parts the projection has as they
    /// come.
    First,
    /// A later one of each page.
    Full,
    /// One of the pages down to the latest message of the sweep before it.
    Recent,
}

/// The token of the feed of a message: the replies of an unfinished answer.
type MessageToken = Option<Replies>;

/// The replies of an unfinished answer of a message: the cursor of their
/// next request, none for their first one, and the messages of their pages
/// so far.
#[derive(Default, Serialize, Deserialize)]
struct Replies {
    cursor: Option<String>,
    messages: Vec<Box<RawValue>>,
}

/// The token of the feed of a file of a message or a reply: the
/// `url_private_download` and the `edit_timestamp` of the file, from the
/// feed that took it.
#[derive(Serialize, Deserialize)]
struct FileToken {
    url: String,
    edit_timestamp: Option<u64>,
}

fn encode<T: Serialize>(token: &T) -> Result<String> {
    serde_json::to_string(token)
        .map_err(|error| ProviderError::internal(format!("feed token: {error}")))
}

fn decode_token<T: DeserializeOwned>(token: &str) -> Result<T> {
    serde_json::from_str(token)
        .map_err(|error| ProviderError::invalid_input(format!("feed token: {error}")))
}

// ===========================================================================
// Facts
// ===========================================================================

/// The folder of `path`.
fn folder(path: &Path) -> Result<Path> {
    path.parent()
        .ok_or_else(|| ProviderError::internal(format!("{path} has no folder")))
}

/// The entries of the listing of `path` in the projection, none without a
/// listing.
fn listed<K, T: FromStr<Err = ()> + Ord>(cx: &FeedCx<K>, path: &str) -> Result<BTreeSet<T>> {
    Ok(cx
        .names(path)?
        .map(|listed| listed.names.into_iter().collect())
        .unwrap_or_default())
}

/// The token of the feed of an object that a feed took, as an inner state
/// of that feed.
enum Inner {
    Message(MessageKey, String),
    File(FileKey, String),
    ReplyFile(ReplyFileKey, String),
}

/// A part with `token`, followed by more parts for `more`, with the inner
/// states `inner`.
fn feed_part(token: String, more: bool, inner: impl IntoIterator<Item = Inner>) -> Feed {
    let mut part = Feed::raw_part(token).more(more);
    for inner in inner {
        part = match inner {
            Inner::Message(key, token) => part.raw_inner::<Message>(&key, token),
            Inner::File(key, token) => part.raw_inner::<File>(&key, token),
            Inner::ReplyFile(key, token) => part.raw_inner::<ReplyFile>(&key, token),
        };
    }
    part.into()
}

// ===========================================================================
// The take of a message or a reply
// ===========================================================================

/// What a take takes: a message of a channel, or a reply of the message
/// `ts` of a channel.
#[derive(Clone, Copy)]
enum Of<'a> {
    Channel(&'a Id),
    Thread(&'a Id, &'a Ts),
}

impl Of<'_> {
    /// The inner state `token` of the feed of the file `file` of the message
    /// or the reply `ts`.
    fn file(self, ts: &Ts, file: &Id, token: String) -> Inner {
        match self {
            Self::Channel(channel) => Inner::File(
                FileKey {
                    id: channel.clone(),
                    ts: ts.clone(),
                    file: file.clone(),
                },
                token,
            ),
            Self::Thread(channel, message) => Inner::ReplyFile(
                ReplyFileKey {
                    id: channel.clone(),
                    ts: message.clone(),
                    reply: ts.clone(),
                    file: file.clone(),
                },
                token,
            ),
        }
    }
}

/// The `message.json` of a message or a reply in the projection before its
/// take.
enum Projected {
    /// None: the take wrote a new one.
    Absent,
    /// The one that the take took, which wrote nothing.
    Same,
    /// Another one, which the take wrote over.
    Other(MessageObject),
}

/// A message or a reply that a take took.
struct Taken {
    message: MessageObject,
    projected: Projected,
}

impl Taken {
    /// Whether the take wrote the message.
    fn written(&self) -> bool {
        !matches!(self.projected, Projected::Same)
    }

    /// Whether the message has another `latest_reply` or `reply_count` than
    /// in the projection.
    fn replies_changed(&self) -> bool {
        match &self.projected {
            Projected::Other(projected) => !projected.same_replies(&self.message),
            Projected::Absent | Projected::Same => false,
        }
    }
}

/// Take the message or the reply `object` of `of` into the projection at its
/// `ts` in `listing`, against its `message.json` there, and write nothing
/// for the same one: else its folder, `author` with its `user`, empty without
/// one, `text`, `message.json`, and `files/` complete with the folder and
/// `name`, empty for `null`, of each file with a `url_private_download`,
/// without each other file of the projection, with the removal of the
/// `content` of a file with another `edit_timestamp` and the inner state of
/// the feed of such a file and of a new one. The folder joins `listing` as
/// the write of `listing` of the part says.
fn take<K>(
    cx: &FeedCx<K>,
    of: Of<'_>,
    listing: &str,
    object: &RawValue,
    inner: &mut Vec<Inner>,
) -> Result<Taken> {
    let bytes = object.get().as_bytes();
    let message: MessageObject = decode(bytes)?;
    let at = format!("{listing}/{}", message.ts);
    let projected = match cx.read(format!("{at}/message.json"))? {
        None => Projected::Absent,
        Some(projected) if projected == bytes => {
            return Ok(Taken {
                message,
                projected: Projected::Same,
            });
        },
        Some(projected) => Projected::Other(decode(&projected)?),
    };
    cx.dir(&at, ListingWrite::Partial)?;
    cx.file(
        format!("{at}/author"),
        message.user.as_deref().unwrap_or_default(),
    )?;
    cx.file(format!("{at}/text"), message.text.as_bytes())?;
    cx.file(format!("{at}/message.json"), bytes)?;

    let folder = format!("{at}/files");
    cx.dir(&folder, ListingWrite::Complete)?;
    let mut files = BTreeSet::new();
    for (file, url) in message.downloads() {
        let file_at = format!("{folder}/{}", file.id);
        cx.dir(&file_at, ListingWrite::Partial)?;
        cx.file(
            format!("{file_at}/name"),
            file.name.as_deref().unwrap_or_default(),
        )?;
        let before = match &projected {
            Projected::Other(projected) => projected
                .downloads()
                .find(|(before, _)| before.id == file.id),
            Projected::Absent | Projected::Same => None,
        };
        if before.is_none_or(|(before, _)| before.edit_timestamp != file.edit_timestamp) {
            if before.is_some() {
                cx.remove(format!("{file_at}/content"))?;
            }
            let token = FileToken {
                url: url.to_owned(),
                edit_timestamp: file.edit_timestamp,
            };
            inner.push(of.file(&message.ts, &file.id, encode(&token)?));
        }
        files.insert(file.id.clone());
    }
    for id in listed::<_, Id>(cx, &folder)?.difference(&files) {
        cx.remove(format!("{folder}/{id}"))?;
    }
    Ok(Taken { message, projected })
}

/// The `replies/` of the message at `at` that a take took: complete without
/// a reply for a message without replies that the take wrote, and its removal
/// for another `latest_reply` or `reply_count`, and for `each` message with
/// replies.
fn take_replies<K>(cx: &FeedCx<K>, at: &str, taken: &Taken, each: bool) -> Result<()> {
    let replies = format!("{at}/replies");
    if !taken.message.has_replies() {
        if taken.written() {
            complete_replies(cx, &replies, &BTreeSet::new())?;
        }
    } else if each || taken.replies_changed() {
        cx.remove(replies)?;
    }
    Ok(())
}

/// Write `at`, the `replies/` of a message, complete with `replies`, which
/// the part wrote, without each other reply of the projection.
fn complete_replies<K>(cx: &FeedCx<K>, at: &str, replies: &BTreeSet<Ts>) -> Result<()> {
    cx.dir(at, ListingWrite::Complete)?;
    for reply in listed::<_, Ts>(cx, at)?.difference(replies) {
        cx.remove(format!("{at}/{reply}"))?;
    }
    Ok(())
}

// ===========================================================================
// The feeds of the users and of the channels
// ===========================================================================

/// What a full listing lists: the users or the channels.
#[derive(Clone, Copy)]
enum Listing {
    Users,
    Channels,
}

impl Listing {
    /// The request of the first page of the listing.
    fn request<K>(self, cx: &FeedCx<K>) -> RequestBuilder<'_, SlackApi> {
        let api = cx.endpoint(SlackApi);
        match self {
            Self::Users => api.get("/api/users.list").query("limit", PAGE),
            Self::Channels => api
                .get("/api/users.conversations")
                .query("types", "public_channel,private_channel")
                .query("exclude_archived", "false")
                .query("limit", PAGE),
        }
    }

    /// Write the folder of an object of the listing in `folder` with `name`
    /// and the object, and give its ID.
    fn write<K>(self, cx: &FeedCx<K>, folder: &Path, object: &RawValue) -> Result<Id> {
        let bytes = object.get().as_bytes();
        let (id, name, json) = match self {
            Self::Users => {
                let user: UserObject = decode(bytes)?;
                let name = user.name().to_owned();
                (user.id, name, "user.json")
            },
            Self::Channels => {
                let channel: ConversationObject = decode(bytes)?;
                (channel.id, channel.name, "channel.json")
            },
        };
        let at = format!("{folder}/{id}");
        cx.dir(&at, ListingWrite::Partial)?;
        cx.file(format!("{at}/name"), name)?;
        cx.file(format!("{at}/{json}"), bytes)?;
        Ok(id)
    }
}

impl Users {
    /// The feed of the users of the workspace.
    pub(crate) async fn feed(cx: FeedCx<WorkspaceKey>) -> Result<Feed> {
        full_listing(&cx, Listing::Users).await
    }
}

impl Channels {
    /// The feed of the channels that the bot is in.
    pub(crate) async fn feed(cx: FeedCx<WorkspaceKey>) -> Result<Feed> {
        full_listing(&cx, Listing::Channels).await
    }
}

/// A call of the feed of a full listing: the page of its token, else the
/// first one, with the `next_cursor` of the page before it as `cursor`, whose
/// part writes the folder of each object of the page; the part of the last
/// page writes the listing complete and removes each entry of it that no
/// page had. `invalid_cursor` restarts the feed with the token of the first
/// page.
async fn full_listing(cx: &FeedCx<WorkspaceKey>, listing: Listing) -> Result<Feed> {
    let token = cx
        .raw_token()
        .map(decode_token::<ListingToken>)
        .transpose()?;
    let (cursor, mut ids) = match token.flatten() {
        Some(ListingPage { cursor, ids }) => (Some(cursor), ids),
        None => (None, BTreeSet::new()),
    };
    let mut request = listing.request(cx);
    if let Some(cursor) = &cursor {
        request = request.query("cursor", cursor);
    }
    let page = match page(&request.send().await?) {
        Ok(page) => page,
        Err(Refusal {
            of: Refused::Cursor,
            error,
        }) if cursor.is_some() => {
            return Ok(Feed::raw_restart(encode(&ListingToken::None)?, error));
        },
        Err(refusal) => return Err(refusal.error),
    };
    let folder = cx.path();
    let write = if page.next.is_some() {
        ListingWrite::Partial
    } else {
        ListingWrite::Complete
    };
    cx.dir(folder, write)?;
    for object in &page.objects {
        ids.insert(listing.write(cx, folder, object)?);
    }
    if let Some(cursor) = page.next {
        let token = Some(ListingPage { cursor, ids });
        return Ok(Feed::raw_part(encode(&token)?).more(true).into());
    }
    for id in listed::<_, Id>(cx, folder.as_str())?.difference(&ids) {
        cx.remove(format!("{folder}/{id}"))?;
    }
    Ok(Feed::raw_part(encode(&ListingToken::None)?).into())
}

// ===========================================================================
// The feed of the history of a channel
// ===========================================================================

impl History {
    /// The feed of the history of a channel: the next page of the sweep of
    /// its token; else a sweep of each page for a call without a token, for
    /// the start or the due start `""`, and for a channel whose messages the
    /// projection lost, as after a removal of the channel by another feed;
    /// else a sweep down to the latest message of its token.
    pub(crate) async fn feed(cx: FeedCx<ChannelKey>) -> Result<Feed> {
        let token = cx
            .raw_token()
            .map(decode_token::<HistoryToken>)
            .transpose()?;
        let (latest, stage) = match token {
            None => (None, Stage::first(Sweep::First)),
            Some(token) if lost(&cx, &token)? => {
                let first = matches!(&token.stage, Some(stage) if stage.sweep == Sweep::First);
                let sweep = if first { Sweep::First } else { Sweep::Full };
                (None, Stage::first(sweep))
            },
            Some(HistoryToken {
                latest,
                stage: Some(stage),
            }) => (latest, stage),
            Some(HistoryToken {
                latest,
                stage: None,
            }) => {
                let sweep = if [cx.start(), cx.due()].contains(&Some("")) {
                    Sweep::Full
                } else {
                    Sweep::Recent
                };
                (latest, Stage::first(sweep))
            },
        };
        sweep(&cx, latest, stage).await
    }
}

/// Whether the projection lost the messages of the channel of `cx` that
/// `token` has there: those of its last sweep, or of the pages so far of its
/// first one.
fn lost(cx: &FeedCx<ChannelKey>, token: &HistoryToken) -> Result<bool> {
    let has = token.latest.is_some()
        || matches!(
            &token.stage,
            Some(stage) if stage.sweep == Sweep::First && !stage.messages.is_empty()
        );
    if !has {
        return Ok(false);
    }
    Ok(cx.count(cx.path())?.is_none_or(|listed| listed.count == 0))
}

/// A request of the next page of `stage`, a sweep of the history after the
/// sweep whose latest message is `latest`, and the take of its messages,
/// with the removal of the `replies/` of each one with replies for a sweep of
/// each page after the first one, and with the inner state of the feed of
/// each message for the first one. The page that ends the sweep, the last one
/// of the history or, for a sweep down to `latest`, the first one with a
/// message as old, writes `messages/` complete and removes each message of
/// the projection that the pages lack, from the oldest one of the pages, and
/// each older one too after the last page of the history, and for the first
/// sweep up to the newest one of the pages, since the feed of a message took
/// a newer one meanwhile. `channel_not_found` and `not_in_channel` remove the
/// channel, and `invalid_cursor` restarts the sweep.
async fn sweep(cx: &FeedCx<ChannelKey>, latest: Option<Ts>, stage: Stage) -> Result<Feed> {
    let Stage {
        sweep,
        cursor,
        mut messages,
    } = stage;
    let channel = &cx.key().id;
    let mut request = cx
        .endpoint(SlackApi)
        .get("/api/conversations.history")
        .query("channel", channel)
        .query("limit", PAGE);
    if let Some(cursor) = &cursor {
        request = request.query("cursor", cursor);
    }
    let page = match page(&request.send().await?) {
        Ok(page) => page,
        Err(Refusal {
            of: Refused::Channel,
            ..
        }) => {
            cx.remove(folder(cx.path())?)?;
            let token = HistoryToken {
                latest: None,
                stage: None,
            };
            return Ok(Feed::raw_part(encode(&token)?).into());
        },
        Err(Refusal {
            of: Refused::Cursor,
            error,
        }) if cursor.is_some() => {
            let token = HistoryToken {
                latest,
                stage: Some(Stage::first(sweep)),
            };
            return Ok(Feed::raw_restart(encode(&token)?, error));
        },
        Err(refusal) => return Err(refusal.error),
    };

    let at = cx.path().as_str();
    let mut inner = Vec::new();
    let mut written = false;
    for object in &page.objects {
        let taken = take(cx, Of::Channel(channel), at, object, &mut inner)?;
        let ts = taken.message.ts.clone();
        take_replies(cx, &format!("{at}/{ts}"), &taken, sweep == Sweep::Full)?;
        written |= taken.written();
        if sweep == Sweep::First {
            let key = MessageKey {
                id: channel.clone(),
                ts: ts.clone(),
            };
            inner.push(Inner::Message(key, encode(&MessageToken::None)?));
        }
        messages.insert(ts);
    }
    let reached = sweep == Sweep::Recent
        && latest
            .as_ref()
            .is_some_and(|latest| messages.first().is_some_and(|oldest| oldest <= latest));
    let last = page.next.is_none();
    let more = page.next.filter(|_| !reached);
    if sweep == Sweep::First || written {
        let write = if more.is_some() {
            ListingWrite::Partial
        } else {
            ListingWrite::Complete
        };
        cx.dir(at, write)?;
    }
    if let Some(cursor) = more {
        let token = HistoryToken {
            latest,
            stage: Some(Stage {
                sweep,
                cursor: Some(cursor),
                messages,
            }),
        };
        return Ok(feed_part(encode(&token)?, true, inner));
    }
    let oldest = messages.first().filter(|_| !last);
    let newest = messages.last().filter(|_| sweep == Sweep::First);
    for ts in listed::<_, Ts>(cx, at)?.difference(&messages) {
        if oldest.is_none_or(|oldest| oldest < ts)
            && (sweep != Sweep::First || newest.is_some_and(|newest| ts < newest))
        {
            cx.remove(format!("{at}/{ts}"))?;
        }
    }
    let token = HistoryToken {
        latest: messages.last().cloned(),
        stage: None,
    };
    Ok(feed_part(encode(&token)?, false, inner))
}

// ===========================================================================
// The feed of a message
// ===========================================================================

/// One call of the feed of a message: its context, the path of the
/// `messages/` of its channel, and the inner states of its part.
struct MessageFeed<'a> {
    cx: &'a FeedCx<MessageKey>,
    messages: Path,
    inner: Vec<Inner>,
}

impl Message {
    /// The feed of a message: the next request of the replies of its token;
    /// else, for a start, each in `replies`, its replies, and nothing for
    /// `replies/` complete in the projection; else the message by its `ts`,
    /// and after it, in the same answer, its replies for a due start, each in
    /// `replies`, of a message with another `latest_reply` or `reply_count`.
    pub(crate) async fn feed(cx: FeedCx<MessageKey>) -> Result<Feed> {
        let token = cx
            .raw_token()
            .map(decode_token::<MessageToken>)
            .transpose()?;
        let mut feed = MessageFeed {
            cx: &cx,
            messages: folder(cx.path())?,
            inner: Vec::new(),
        };
        if let Some(replies) = token.flatten() {
            return feed.replies(replies).await;
        }
        if cx.start().is_some() {
            return feed.thread().await;
        }
        feed.message(cx.due().is_some()).await
    }
}

impl MessageFeed<'_> {
    /// The path of the `replies/` of the message.
    fn replies_path(&self) -> String {
        format!("{}/replies", self.cx.path())
    }

    /// The replies of the message for a start in `replies`: nothing for
    /// `replies/` complete in the projection, which has each reply, as for a
    /// lookup of a reply that it lacks; `replies/` complete without a
    /// request for a message without replies by its `message.json` in the
    /// projection; else their first request.
    async fn thread(&mut self) -> Result<Feed> {
        let at = self.replies_path();
        let replies = self.cx.count(&at)?;
        if replies.is_some_and(|listed| listed.complete) {
            return self.part(None);
        }
        let projected = self
            .cx
            .read(format!("{}/message.json", self.cx.path()))?
            .map(|bytes| decode::<MessageObject>(&bytes))
            .transpose()?;
        if projected.is_some_and(|message| !message.has_replies()) {
            complete_replies(self.cx, &at, &BTreeSet::new())?;
            return self.part(None);
        }
        self.replies(Replies::default()).await
    }

    /// The message by its `ts`: its take, its folder joining `messages/`, or
    /// its removal for an answer without it; then, for `due`, a message with
    /// replies and another `latest_reply` or `reply_count` takes in the same
    /// answer its replies, which replace those of the projection.
    async fn message(&mut self, due: bool) -> Result<Feed> {
        let key = self.cx.key();
        let response = self
            .cx
            .endpoint(SlackApi)
            .get("/api/conversations.history")
            .query("channel", &key.id)
            .query("latest", &key.ts)
            .query("oldest", &key.ts)
            .query("inclusive", "true")
            .query("limit", 1)
            .send()
            .await?;
        let page = match page(&response) {
            Ok(page) => page,
            Err(Refusal {
                of: Refused::Channel,
                ..
            }) => return self.channel_removal(),
            Err(refusal) => return Err(refusal.error),
        };
        let mut found = None;
        for object in &page.objects {
            if decode::<MessageObject>(object.get().as_bytes())?.ts == key.ts {
                found = Some(object);
            }
        }
        let Some(object) = found else {
            return self.removal();
        };
        let taken = self.take_message(object)?;
        if due && taken.message.has_replies() && taken.replies_changed() {
            return self.part(Some(Replies::default()));
        }
        take_replies(self.cx, self.cx.path().as_str(), &taken, false)?;
        self.part(None)
    }

    /// Take the message itself, its folder joining `messages/` as complete
    /// as it was.
    fn take_message(&mut self, object: &RawValue) -> Result<Taken> {
        let taken = take(
            self.cx,
            Of::Channel(&self.cx.key().id),
            self.messages.as_str(),
            object,
            &mut self.inner,
        )?;
        if taken.written() {
            self.cx.dir(&self.messages, ListingWrite::Join)?;
        }
        Ok(taken)
    }

    /// A request of the replies, by the cursor of the request before it,
    /// whose messages the token keeps until the last page. `thread_not_found`
    /// removes the message, `channel_not_found` and `not_in_channel` the
    /// channel, and `invalid_cursor` restarts the replies.
    async fn replies(&mut self, replies: Replies) -> Result<Feed> {
        let Replies {
            cursor,
            mut messages,
        } = replies;
        let key = self.cx.key();
        let mut request = self
            .cx
            .endpoint(SlackApi)
            .get("/api/conversations.replies")
            .query("channel", &key.id)
            .query("ts", &key.ts)
            .query("limit", REPLIES);
        if let Some(cursor) = &cursor {
            request = request.query("cursor", cursor);
        }
        let page = match page(&request.send().await?) {
            Ok(page) => page,
            Err(Refusal {
                of: Refused::Thread,
                ..
            }) => return self.removal(),
            Err(Refusal {
                of: Refused::Channel,
                ..
            }) => return self.channel_removal(),
            Err(Refusal {
                of: Refused::Cursor,
                error,
            }) if cursor.is_some() => {
                let token = Some(Replies::default());
                return Ok(Feed::raw_restart(encode(&token)?, error));
            },
            Err(refusal) => return Err(refusal.error),
        };
        messages.extend(page.objects);
        if let Some(cursor) = page.next {
            return self.part(Some(Replies {
                cursor: Some(cursor),
                messages,
            }));
        }
        self.replies_end(&messages)
    }

    /// The last part of the replies: the message of `messages` with the `ts`
    /// of the message as the message, its folder joining `messages/`, and
    /// `replies/` complete with each other message whose `thread_ts` is that
    /// `ts`, with the removal of each other reply.
    fn replies_end(&mut self, messages: &[Box<RawValue>]) -> Result<Feed> {
        let key = self.cx.key();
        let at = self.replies_path();
        let mut replies = BTreeSet::new();
        for object in messages {
            let message: MessageObject = decode(object.get().as_bytes())?;
            if message.ts == key.ts {
                self.take_message(object)?;
            } else if message.thread_ts.as_ref() == Some(&key.ts) {
                let thread = Of::Thread(&key.id, &key.ts);
                take(self.cx, thread, &at, object, &mut self.inner)?;
                replies.insert(message.ts);
            }
        }
        complete_replies(self.cx, &at, &replies)?;
        self.part(None)
    }

    /// The removal of the message, with its replies.
    fn removal(&mut self) -> Result<Feed> {
        self.cx.remove(self.cx.path())?;
        self.part(None)
    }

    /// The removal of the channel of the message.
    fn channel_removal(&mut self) -> Result<Feed> {
        self.cx.remove(folder(&self.messages)?)?;
        self.part(None)
    }

    /// A part with the replies of an unfinished answer, followed by more
    /// parts while there are, with the inner states of its messages and
    /// replies.
    fn part(&mut self, replies: Option<Replies>) -> Result<Feed> {
        let more = replies.is_some();
        Ok(feed_part(encode(&replies)?, more, self.inner.drain(..)))
    }
}

// ===========================================================================
// The feeds of the files
// ===========================================================================

impl File {
    /// The feed of a file of a message: its content.
    pub(crate) async fn feed(cx: FeedCx<FileKey>) -> Result<Feed> {
        content(&cx).await
    }
}

impl ReplyFile {
    /// The feed of a file of a reply: its content.
    pub(crate) async fn feed(cx: FeedCx<ReplyFileKey>) -> Result<Feed> {
        content(&cx).await
    }
}

/// The `content` of the file of `cx`: the bytes of the URL of its token as a
/// blob fetch with the version of its `edit_timestamp`, and without one for a
/// file without it. Slack answers the bytes of a file with the status `200`
/// alone: each other answer is the error of its status, the rate-limited
/// error with the seconds of its `Retry-After` for `429`.
async fn content<K>(cx: &FeedCx<K>) -> Result<Feed> {
    let token = cx.raw_token().ok_or_else(|| {
        ProviderError::internal("the feed of a file has no token of the feed that took the file")
    })?;
    let file: FileToken = decode_token(token)?;
    let mut request = cx.http().get(file.url).into_blob();
    if let Some(edited) = file.edit_timestamp {
        request = request.version(edited.to_string());
    }
    let blob = request.send().await?;
    match blob.status {
        200 => {},
        429 => {
            let retry_after = blob
                .response_headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("retry-after"))
                .map(|(_, value)| value.as_str());
            return Err(rate_limited(retry_after));
        },
        status => return Err(ProviderError::from_http_status(status)),
    }
    let handle = BlobHandle {
        id: blob.id,
        size: blob.size,
    };
    cx.blob(format!("{}/content", cx.path()), &handle)?;
    Ok(Feed::raw_part(token).into())
}
