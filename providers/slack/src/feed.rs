//! The feeds of the users, the channels, the messages, the replies, and the
//! files of the workspace.
//!
//! The feeds of the users and of the channels are feeds of full listings of
//! `users.list` and of `users.conversations`: each call takes a page, whose
//! objects the token keeps until the last page, which writes the listing
//! complete and removes each other entry of it. The feed of the history of a
//! channel takes a part for each page of `conversations.history`: a full
//! listing at the first read of `messages/`, at each read of it past the
//! deadline of the feed, and for a channel without a message, after whose
//! last page each message of the projection that it lacks leaves, and else
//! the new messages after the `ts` of the latest message of the channel. Its
//! first full listing gives the feed of each message its state, so that the
//! feed of a message takes it by its `ts` past its deadline before the end
//! of that listing. The own feed of a message takes it by its `ts`, and, for
//! a `replies/` that the projection has not complete, its replies with the
//! message itself from the pages of `conversations.replies`, kept in its
//! token until the last page, and the own feed of a file of a message or a
//! reply takes its content from the URL that the feed that took the file
//! gives it.
//!
//! The take of a message or a reply writes its files against its
//! `message.json` in the projection, with the removal of the `content` of a
//! file with another `edit_timestamp`, which its next read takes once more,
//! and for a message the removal of its replies for another `latest_reply`
//! or `reply_count`, which their next read takes once more.

use std::collections::BTreeSet;
use std::str::FromStr;

use omnifs_sdk::prelude::*;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::api::{
    Answer, ConversationObject, Conversations, Members, MessageObject, Messages, SlackApi,
    SlackFiles, UserObject, answer, decode, failure,
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

/// The token of the feed of the users or of the channels: the page of an
/// unfinished full listing, none for the first page of the next one.
#[derive(Serialize, Deserialize)]
struct ListingToken {
    page: Option<Page>,
}

/// The page of an unfinished full listing: the `next_cursor` of the page
/// before it, and the objects of the pages so far.
#[derive(Serialize, Deserialize)]
struct Page {
    cursor: String,
    objects: Vec<Box<RawValue>>,
}

/// The token of the feed of the history of a channel: the `ts` of its latest
/// message, none for a channel without one, and the stage of an unfinished
/// answer.
#[derive(Serialize, Deserialize)]
struct HistoryToken {
    latest: Option<Ts>,
    stage: Option<HistoryStage>,
}

/// The stage of an unfinished answer of the history of a channel.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum HistoryStage {
    /// A full listing at the cursor of its next page, none for its first
    /// one, with the `ts` of the messages of its pages so far; `first` for
    /// the first full listing.
    Listing {
        first: bool,
        cursor: Option<String>,
        messages: BTreeSet<Ts>,
    },
    /// The new messages after the latest message of the token at the cursor
    /// of their next page, none for their first one, with the latest `ts` of
    /// their pages so far.
    New {
        cursor: Option<String>,
        latest: Option<Ts>,
    },
}

/// The token of the feed of a message: the replies of an unfinished answer.
#[derive(Serialize, Deserialize)]
struct MessageToken {
    replies: Option<Replies>,
}

/// The replies of an unfinished answer of a message: the cursor of their
/// next request, none for their first one, and the messages of their pages
/// so far.
#[derive(Serialize, Deserialize)]
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

const USERS: &str = "/users";
const CHANNELS: &str = "/channels";

fn channel_path(id: &Id) -> String {
    format!("{CHANNELS}/{id}")
}

fn messages_path(channel: &Id) -> String {
    format!("{}/messages", channel_path(channel))
}

fn message_path(channel: &Id, ts: &Ts) -> String {
    format!("{}/{ts}", messages_path(channel))
}

/// The entries of the listing of `path` in the projection, none without a
/// listing.
fn listed<K, T: FromStr<Err = ()> + Ord>(cx: &FeedCx<K>, path: &str) -> Result<BTreeSet<T>> {
    let Some(facts) = cx.listing(path, ListingForm::Names)? else {
        return Ok(BTreeSet::new());
    };
    let ListingEntries::Names(names) = facts.entries else {
        return Err(ProviderError::internal(format!(
            "the listing of {path} has no names"
        )));
    };
    names
        .iter()
        .map(|name| {
            name.parse()
                .map_err(|()| ProviderError::internal(format!("{path} lists {name}")))
        })
        .collect()
}

/// Whether Slack answered that the bot sees the channel of a request no
/// more.
fn channel_gone(error: &str) -> bool {
    matches!(error, "channel_not_found" | "not_in_channel")
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
    let mut part = Feed::part(token).more(more);
    for inner in inner {
        part = match inner {
            Inner::Message(key, token) => part.inner::<Message>(&key, token),
            Inner::File(key, token) => part.inner::<File>(&key, token),
            Inner::ReplyFile(key, token) => part.inner::<ReplyFile>(&key, token),
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
    /// The listing of the messages or of the replies.
    fn listing(self) -> String {
        match self {
            Self::Channel(channel) => messages_path(channel),
            Self::Thread(channel, ts) => format!("{}/replies", message_path(channel, ts)),
        }
    }

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

/// A message or a reply that a take took, with its `message.json` in the
/// projection before the take, none for a new one.
struct Taken {
    message: MessageObject,
    projected: Option<MessageObject>,
}

impl Taken {
    /// Whether the message has another `latest_reply` or `reply_count` than
    /// in the projection.
    fn replies_changed(&self) -> bool {
        self.projected.as_ref().is_some_and(|projected| {
            projected.latest_reply != self.message.latest_reply
                || projected.reply_count != self.message.reply_count
        })
    }
}

/// Take a message or a reply of `of` into the projection, against its
/// `message.json` there: its folder joining its listing as `write` says,
/// `author` with its `user`, empty without one, `text`, `message.json`, and
/// `files/` complete with the folder and `name`, empty for `null`, of each
/// file with a `url_private_download`, without each other file of the
/// projection, with the removal of the `content` of a file with another
/// `edit_timestamp` and the inner state of the feed of such a file and of a
/// new one.
fn take<K>(
    cx: &FeedCx<K>,
    of: Of<'_>,
    object: &RawValue,
    write: ListingWrite,
    inner: &mut Vec<Inner>,
) -> Result<Taken> {
    let message: MessageObject = decode(object.get().as_bytes())?;
    let listing = of.listing();
    let at = format!("{listing}/{}", message.ts);
    let projected = cx
        .read(format!("{at}/message.json"))?
        .map(|bytes| decode::<MessageObject>(&bytes))
        .transpose()?;
    cx.dir(listing, write)?;
    cx.dir(&at, ListingWrite::Partial)?;
    cx.file(
        format!("{at}/author"),
        message.user.as_deref().unwrap_or_default(),
    )?;
    cx.file(format!("{at}/text"), message.text.as_bytes())?;
    cx.file(format!("{at}/message.json"), object.get().as_bytes())?;

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
        let before = projected.as_ref().and_then(|projected| {
            projected
                .downloads()
                .find(|(before, _)| before.id == file.id)
        });
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

/// The `replies/` of a message at `at` that a take took: its removal for
/// another `latest_reply` or `reply_count`, and `replies/` complete for none.
fn take_replies<K>(cx: &FeedCx<K>, at: &str, taken: &Taken) -> Result<()> {
    let replies = format!("{at}/replies");
    if taken.replies_changed() {
        cx.remove(replies)
    } else if taken.message.reply_count.is_none() {
        cx.dir(replies, ListingWrite::Complete)
    } else {
        Ok(())
    }
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
    /// The folder of the listing.
    fn folder(self) -> &'static str {
        match self {
            Self::Users => USERS,
            Self::Channels => CHANNELS,
        }
    }

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

    /// The objects of a page of the listing.
    fn objects(self, page: &[u8]) -> Result<Vec<Box<RawValue>>> {
        Ok(match self {
            Self::Users => decode::<Members>(page)?.members,
            Self::Channels => decode::<Conversations>(page)?.channels,
        })
    }

    /// Write the folder of an object of the listing with `name` and the
    /// object, and give its ID.
    fn write<K>(self, cx: &FeedCx<K>, object: &RawValue) -> Result<Id> {
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
        let at = format!("{}/{id}", self.folder());
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
/// first one, with the `next_cursor` of the page before it as `cursor`, kept
/// in the token until the last page, whose part writes the listing complete
/// with the folder of each object of the pages and removes each other entry
/// of the listing; `invalid_cursor` restarts the feed with the token of the
/// first page.
async fn full_listing(cx: &FeedCx<WorkspaceKey>, listing: Listing) -> Result<Feed> {
    let token = cx.token().map(decode_token::<ListingToken>).transpose()?;
    let (cursor, mut objects) = match token.and_then(|token| token.page) {
        Some(Page { cursor, objects }) => (Some(cursor), objects),
        None => (None, Vec::new()),
    };
    let mut request = listing.request(cx);
    if let Some(cursor) = &cursor {
        request = request.query("cursor", cursor);
    }
    let response = request.send().await?;
    let next = match answer(&response)? {
        Answer::Page { next } => next,
        Answer::Error(error) if error == "invalid_cursor" && cursor.is_some() => {
            let token = ListingToken { page: None };
            return Ok(Feed::restart(encode(&token)?, failure(&error)));
        },
        Answer::Error(error) => return Err(failure(&error)),
    };
    objects.extend(listing.objects(response.body())?);
    if let Some(cursor) = next {
        let token = ListingToken {
            page: Some(Page { cursor, objects }),
        };
        return Ok(Feed::part(encode(&token)?).more(true).into());
    }
    let folder = listing.folder();
    cx.dir(folder, ListingWrite::Complete)?;
    let mut ids = BTreeSet::new();
    for object in &objects {
        ids.insert(listing.write(cx, object)?);
    }
    for id in listed::<_, Id>(cx, folder)?.difference(&ids) {
        cx.remove(format!("{folder}/{id}"))?;
    }
    Ok(Feed::part(encode(&ListingToken { page: None })?).into())
}

// ===========================================================================
// The feed of the history of a channel
// ===========================================================================

/// One call of the feed of the history of a channel: its context, its
/// channel, the `ts` of the latest message of its token, and the inner
/// states of its part.
struct HistoryFeed<'a> {
    cx: &'a FeedCx<ChannelKey>,
    channel: &'a Id,
    latest: Option<Ts>,
    inner: Vec<Inner>,
}

impl History {
    /// The feed of the history of a channel: the next request of its token;
    /// else a full listing for a call without a token, for the start or the
    /// due start `""`, and for a channel without a message in its token or
    /// in the projection, as after a removal of the channel by another feed;
    /// else the new messages after the latest message of its token.
    pub(crate) async fn feed(cx: FeedCx<ChannelKey>) -> Result<Feed> {
        let token = cx.token().map(decode_token::<HistoryToken>).transpose()?;
        let first = token.is_none();
        let (latest, stage) = match token {
            Some(HistoryToken { latest, stage }) => (latest, stage),
            None => (None, None),
        };
        let listing = [cx.start(), cx.due()].contains(&Some(""));
        let stage = match stage {
            Some(stage) => stage,
            None if first || listing || latest.is_none() || !has_messages(&cx)? => {
                HistoryStage::Listing {
                    first,
                    cursor: None,
                    messages: BTreeSet::new(),
                }
            },
            None => HistoryStage::New {
                cursor: None,
                latest: None,
            },
        };
        let mut feed = HistoryFeed {
            cx: &cx,
            channel: &cx.key().id,
            latest,
            inner: Vec::new(),
        };
        feed.page(stage).await
    }
}

/// Whether the projection has a message of the channel of `cx`.
fn has_messages(cx: &FeedCx<ChannelKey>) -> Result<bool> {
    let facts = cx.listing(messages_path(&cx.key().id), ListingForm::Count)?;
    Ok(facts.is_some_and(|facts| !matches!(facts.entries, ListingEntries::Count(0))))
}

impl HistoryFeed<'_> {
    /// A request of a page of the history, with `oldest` of the latest
    /// message of the token for the new messages, and its messages: those
    /// of the first full listing with `messages/` partial and the inner
    /// state of the feed of each message, so that it asks for the message
    /// again past its deadline alone, and those of a later answer with
    /// `messages/` complete. The last page of a full listing writes
    /// `messages/` complete and removes each other message of the
    /// projection. `channel_not_found` and `not_in_channel` remove the
    /// channel, and `invalid_cursor` restarts with the token of the first
    /// request of the answer.
    async fn page(&mut self, stage: HistoryStage) -> Result<Feed> {
        let (cursor, oldest) = match &stage {
            HistoryStage::Listing { cursor, .. } => (cursor.clone(), None),
            HistoryStage::New { cursor, .. } => (cursor.clone(), self.latest.clone()),
        };
        let mut request = self
            .cx
            .endpoint(SlackApi)
            .get("/api/conversations.history")
            .query("channel", self.channel);
        if let Some(oldest) = &oldest {
            request = request.query("oldest", oldest);
        }
        request = request.query("limit", PAGE);
        if let Some(cursor) = &cursor {
            request = request.query("cursor", cursor);
        }
        let response = request.send().await?;
        let next = match answer(&response)? {
            Answer::Page { next } => next,
            Answer::Error(error) if channel_gone(&error) => {
                self.cx.remove(channel_path(self.channel))?;
                let token = HistoryToken {
                    latest: None,
                    stage: None,
                };
                return Ok(Feed::part(encode(&token)?).into());
            },
            Answer::Error(error) if error == "invalid_cursor" && cursor.is_some() => {
                return self.restart(stage, &error);
            },
            Answer::Error(error) => return Err(failure(&error)),
        };
        let page: Messages = decode(response.body())?;
        match stage {
            HistoryStage::Listing {
                first,
                mut messages,
                ..
            } => {
                let write = if first {
                    ListingWrite::Partial
                } else {
                    ListingWrite::Complete
                };
                for object in &page.messages {
                    let ts = self.message(object, write)?;
                    if first {
                        let key = MessageKey {
                            id: self.channel.clone(),
                            ts: ts.clone(),
                        };
                        let token = encode(&MessageToken { replies: None })?;
                        self.inner.push(Inner::Message(key, token));
                    }
                    messages.insert(ts);
                }
                if let Some(cursor) = next {
                    return self.part(Some(HistoryStage::Listing {
                        first,
                        cursor: Some(cursor),
                        messages,
                    }));
                }
                let at = messages_path(self.channel);
                self.cx.dir(&at, ListingWrite::Complete)?;
                for ts in listed::<_, Ts>(self.cx, &at)?.difference(&messages) {
                    self.cx.remove(format!("{at}/{ts}"))?;
                }
                self.latest = messages.last().cloned();
                self.part(None)
            },
            HistoryStage::New { mut latest, .. } => {
                for object in &page.messages {
                    let ts = self.message(object, ListingWrite::Complete)?;
                    latest = latest.max(Some(ts));
                }
                if let Some(cursor) = next {
                    return self.part(Some(HistoryStage::New {
                        cursor: Some(cursor),
                        latest,
                    }));
                }
                self.latest = self.latest.take().max(latest);
                self.part(None)
            },
        }
    }

    /// Take a message of the history, its folder joining `messages/` as
    /// `write` says, with its `replies/`, and give its `ts`.
    fn message(&mut self, object: &RawValue, write: ListingWrite) -> Result<Ts> {
        let taken = take(
            self.cx,
            Of::Channel(self.channel),
            object,
            write,
            &mut self.inner,
        )?;
        take_replies(
            self.cx,
            &message_path(self.channel, &taken.message.ts),
            &taken,
        )?;
        Ok(taken.message.ts)
    }

    /// The restart of an answer whose cursor Slack refused, with the token
    /// of the first request of the answer.
    fn restart(&self, stage: HistoryStage, error: &str) -> Result<Feed> {
        let stage = match stage {
            HistoryStage::Listing { first, .. } => HistoryStage::Listing {
                first,
                cursor: None,
                messages: BTreeSet::new(),
            },
            HistoryStage::New { .. } => HistoryStage::New {
                cursor: None,
                latest: None,
            },
        };
        let token = HistoryToken {
            latest: self.latest.clone(),
            stage: Some(stage),
        };
        Ok(Feed::restart(encode(&token)?, failure(error)))
    }

    /// A part with the latest `ts` of the token and `stage`, followed by
    /// more parts while a stage remains, with the inner states of its
    /// messages.
    fn part(&mut self, stage: Option<HistoryStage>) -> Result<Feed> {
        let more = stage.is_some();
        let token = HistoryToken {
            latest: self.latest.clone(),
            stage,
        };
        Ok(feed_part(encode(&token)?, more, self.inner.drain(..)))
    }
}

// ===========================================================================
// The feed of a message
// ===========================================================================

/// One call of the feed of a message: its context, its channel and `ts`,
/// and the inner states of its part.
struct MessageFeed<'a> {
    cx: &'a FeedCx<MessageKey>,
    channel: &'a Id,
    ts: &'a Ts,
    inner: Vec<Inner>,
}

impl Message {
    /// The feed of a message: the next request of the replies of its token;
    /// else its replies for a start in `replies` of a message without
    /// `replies/` complete in the projection; else the message by its `ts`,
    /// and after it, in the same answer, its replies for a due start in
    /// `replies` of a message with another `latest_reply` or `reply_count`.
    pub(crate) async fn feed(cx: FeedCx<MessageKey>) -> Result<Feed> {
        let token = cx.token().map(decode_token::<MessageToken>).transpose()?;
        let key = cx.key();
        let mut feed = MessageFeed {
            cx: &cx,
            channel: &key.id,
            ts: &key.ts,
            inner: Vec::new(),
        };
        if let Some(MessageToken {
            replies: Some(replies),
        }) = token
        {
            return feed.replies(replies).await;
        }
        if cx.start().is_some_and(in_replies) {
            return feed.thread().await;
        }
        feed.message(cx.due().is_some_and(in_replies)).await
    }
}

/// Whether a starting path is in `replies`.
fn in_replies(start: &str) -> bool {
    start == "replies" || start.starts_with("replies/")
}

impl MessageFeed<'_> {
    fn path(&self) -> String {
        message_path(self.channel, self.ts)
    }

    /// The replies of the message for a start in `replies`: nothing for
    /// `replies/` complete in the projection, which has each reply, as for a
    /// lookup of a reply that it lacks; `replies/` complete without a
    /// request for a `reply_count` of `0` or none in its `message.json` in
    /// the projection; else their first request.
    async fn thread(&mut self) -> Result<Feed> {
        let replies = self
            .cx
            .listing(format!("{}/replies", self.path()), ListingForm::Count)?;
        if replies.is_some_and(|facts| facts.complete) {
            return self.part(None);
        }
        let projected = self
            .cx
            .read(format!("{}/message.json", self.path()))?
            .map(|bytes| decode::<MessageObject>(&bytes))
            .transpose()?;
        if projected.is_some_and(|message| matches!(message.reply_count, None | Some(0))) {
            return self.replies_end(&[]);
        }
        self.replies(Replies {
            cursor: None,
            messages: Vec::new(),
        })
        .await
    }

    /// The message by its `ts`: its take, its folder joining `messages/`, or
    /// its removal for an answer without it; then, for `due` in `replies`, a
    /// message with another `latest_reply` or `reply_count` takes in the
    /// same answer its replies, which replace those of the projection.
    async fn message(&mut self, due: bool) -> Result<Feed> {
        let response = self
            .cx
            .endpoint(SlackApi)
            .get("/api/conversations.history")
            .query("channel", self.channel)
            .query("latest", self.ts)
            .query("oldest", self.ts)
            .query("inclusive", "true")
            .query("limit", 1)
            .send()
            .await?;
        match answer(&response)? {
            Answer::Page { .. } => {},
            Answer::Error(error) if channel_gone(&error) => return self.channel_removal(),
            Answer::Error(error) => return Err(failure(&error)),
        }
        let page: Messages = decode(response.body())?;
        let mut found = None;
        for object in &page.messages {
            if decode::<MessageObject>(object.get().as_bytes())?.ts == *self.ts {
                found = Some(object);
            }
        }
        let Some(object) = found else {
            return self.removal();
        };
        let taken = take(
            self.cx,
            Of::Channel(self.channel),
            object,
            ListingWrite::Join,
            &mut self.inner,
        )?;
        if due && taken.replies_changed() {
            if matches!(taken.message.reply_count, None | Some(0)) {
                return self.replies_end(&[]);
            }
            return self.part(Some(Replies {
                cursor: None,
                messages: Vec::new(),
            }));
        }
        take_replies(self.cx, &self.path(), &taken)?;
        self.part(None)
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
        let mut request = self
            .cx
            .endpoint(SlackApi)
            .get("/api/conversations.replies")
            .query("channel", self.channel)
            .query("ts", self.ts)
            .query("limit", REPLIES);
        if let Some(cursor) = &cursor {
            request = request.query("cursor", cursor);
        }
        let response = request.send().await?;
        let next = match answer(&response)? {
            Answer::Page { next } => next,
            Answer::Error(error) if error == "thread_not_found" => return self.removal(),
            Answer::Error(error) if channel_gone(&error) => return self.channel_removal(),
            Answer::Error(error) if error == "invalid_cursor" && cursor.is_some() => {
                let token = MessageToken {
                    replies: Some(Replies {
                        cursor: None,
                        messages: Vec::new(),
                    }),
                };
                return Ok(Feed::restart(encode(&token)?, failure(&error)));
            },
            Answer::Error(error) => return Err(failure(&error)),
        };
        messages.extend(decode::<Messages>(response.body())?.messages);
        if let Some(cursor) = next {
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
        let thread = Of::Thread(self.channel, self.ts);
        let mut replies = BTreeSet::new();
        for object in messages {
            let message: MessageObject = decode(object.get().as_bytes())?;
            if message.ts == *self.ts {
                let channel = Of::Channel(self.channel);
                take(
                    self.cx,
                    channel,
                    object,
                    ListingWrite::Join,
                    &mut self.inner,
                )?;
            } else if message.thread_ts.as_ref() == Some(self.ts) {
                let reply = take(
                    self.cx,
                    thread,
                    object,
                    ListingWrite::Complete,
                    &mut self.inner,
                )?;
                replies.insert(reply.message.ts);
            }
        }
        let at = thread.listing();
        self.cx.dir(&at, ListingWrite::Complete)?;
        for reply in listed::<_, Ts>(self.cx, &at)?.difference(&replies) {
            self.cx.remove(format!("{at}/{reply}"))?;
        }
        self.part(None)
    }

    /// The removal of the message, with its replies.
    fn removal(&mut self) -> Result<Feed> {
        self.cx.remove(self.path())?;
        self.part(None)
    }

    /// The removal of the channel of the message.
    fn channel_removal(&mut self) -> Result<Feed> {
        self.cx.remove(channel_path(self.channel))?;
        self.part(None)
    }

    /// A part with the replies of an unfinished answer, followed by more
    /// parts while there are, with the inner states of its messages and
    /// replies.
    fn part(&mut self, replies: Option<Replies>) -> Result<Feed> {
        let more = replies.is_some();
        let token = encode(&MessageToken { replies })?;
        Ok(feed_part(token, more, self.inner.drain(..)))
    }
}

// ===========================================================================
// The feeds of the files
// ===========================================================================

impl File {
    /// The feed of a file of a message: its content.
    pub(crate) async fn feed(cx: FeedCx<FileKey>) -> Result<Feed> {
        let FileKey { id, ts, file } = cx.key();
        let at = format!("{}/files/{file}/content", message_path(id, ts));
        content(&cx, at).await
    }
}

impl ReplyFile {
    /// The feed of a file of a reply: its content.
    pub(crate) async fn feed(cx: FeedCx<ReplyFileKey>) -> Result<Feed> {
        let ReplyFileKey {
            id,
            ts,
            reply,
            file,
        } = cx.key();
        let at = format!(
            "{}/replies/{reply}/files/{file}/content",
            message_path(id, ts)
        );
        content(&cx, at).await
    }
}

/// The content of a file as its file `at`: the path of the URL of its token
/// on `SlackFiles` as a blob fetch with the version of its `edit_timestamp`,
/// and without one for a file without it.
async fn content<K>(cx: &FeedCx<K>, at: String) -> Result<Feed> {
    let token = cx.token().ok_or_else(|| {
        ProviderError::internal("the feed of a file has no token of the feed that took the file")
    })?;
    let file: FileToken = decode_token(token)?;
    let path = file
        .url
        .strip_prefix(SlackFiles.base())
        .filter(|path| path.starts_with('/'))
        .ok_or_else(|| {
            ProviderError::invalid_input(format!(
                "the url_private_download {} of Slack is not of {}",
                file.url,
                SlackFiles.base()
            ))
        })?;
    let mut request = cx.endpoint(SlackFiles).get(path).into_blob();
    if let Some(edited) = file.edit_timestamp {
        request = request.version(edited.to_string());
    }
    let blob = request.fetch().await?;
    cx.blob(at, &blob)?;
    Ok(Feed::part(token).into())
}
