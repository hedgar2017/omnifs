//! The feeds of the users, the channels, the messages, the replies, and the
//! files of the workspace.
//!
//! The feeds of the users and of the channels are feeds of full listings of
//! `users.list` and of `users.conversations`: each call takes a page and
//! writes its objects, with their IDs in the token until the last page, which
//! writes the listing complete and removes each other entry of it.
//!
//! The feed of the history of a channel answers from `conversations.history`
//! alone, which owns the files of each message: each answer sweeps the
//! pages of the history, the newest first, once in a sync and at the first
//! read of the channel. Each page writes each of its messages as it came, and
//! the last page writes `messages/` complete, empty for a channel without a
//! message, without each message of the projection that the pages lack. A
//! message without replies has `replies/` complete and empty; the `replies/`
//! of a message whose `reply_count` or `latest_reply` are not those of the
//! replies that the projection has complete leave the projection, for their
//! next read to take them once more. The first sweep gives the feed of each
//! message that it writes its state. A channel that the feed of the channels
//! writes and that the projection did not list before, as after its
//! removal, begins its history again with the token of a channel before its
//! first sweep, without the kept pages of an answer from another token.
//!
//! The own feed of a message takes a message that the projection lacks by
//! its `ts` before the end of the first sweep of its channel, and the replies of a `replies/` that the
//! projection has not complete from the pages of `conversations.replies`,
//! each page at its answer, without the message itself, whose files the
//! feed of the history owns. The own feed of a file of a message or a reply
//! takes its content from the URL that the feed that took the file gives
//! it, once for each `url_private_download` and `edit_timestamp` of the file.

use std::collections::BTreeSet;

use omnifs_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::api::{
    ConversationObject, ConversationsMessages, FileObject, MessageObject, Refusal, Refused,
    SlackApi, SlackFiles, UserObject, UsersConversations, UsersList, decode, page,
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
/// before it, the IDs of the objects of the pages so far, and the IDs of the
/// listing in the projection before the listing, none for the first one.
#[derive(Serialize, Deserialize)]
struct ListingPage {
    cursor: String,
    ids: BTreeSet<Id>,
    before: Option<BTreeSet<Id>>,
}

/// The token of the feed of the history of a channel: whether a sweep of
/// the history ended, and the next page of an unfinished sweep.
#[derive(Serialize, Deserialize)]
struct HistoryToken {
    swept: bool,
    sweep: Option<Sweep>,
}

impl HistoryToken {
    /// The token of a channel before its first sweep.
    fn unswept() -> Self {
        Self {
            swept: false,
            sweep: None,
        }
    }
}

/// The next page of an unfinished sweep: the cursor of that page, and the
/// `ts` of the messages of the pages so far.
#[derive(Serialize, Deserialize)]
struct Sweep {
    cursor: String,
    messages: BTreeSet<Ts>,
}

/// The token of the feed of a message: the replies of an unfinished answer.
type MessageToken = Option<Replies>;

/// The replies of an unfinished answer of a message: the cursor of their
/// next page, none for their first one, and the `ts` of the replies of their
/// pages so far.
#[derive(Serialize, Deserialize)]
struct Replies {
    cursor: Option<String>,
    replies: BTreeSet<Ts>,
}

impl Replies {
    fn first() -> Self {
        Self {
            cursor: None,
            replies: BTreeSet::new(),
        }
    }
}

/// The token of the feed of a file of a message or a reply: the
/// `url_private_download` and the `edit_timestamp` of the file, from the
/// feed that took it.
#[derive(PartialEq, Eq, Serialize, Deserialize)]
struct FileToken {
    url: String,
    edit_timestamp: Option<u64>,
}

impl FileToken {
    fn of(file: &FileObject, url: &str) -> Self {
        Self {
            url: url.to_owned(),
            edit_timestamp: file.edit_timestamp,
        }
    }
}

// ===========================================================================
// Facts
// ===========================================================================

/// The folder of `path`.
fn folder(path: &Path) -> Result<Path> {
    path.parent()
        .ok_or_else(|| ProviderError::internal(format!("{path} has no folder")))
}

/// The names of the listing of `path` in the projection, none without a
/// listing.
fn listed<K>(cx: &FeedCx<K>, path: &str) -> Result<BTreeSet<Ts>> {
    Ok(cx
        .names::<Ts>(path)?
        .map(|listed| listed.names.into_iter().collect())
        .unwrap_or_default())
}

/// The whole of the fed file at `path` in the projection, by a range of its
/// length, none without one.
fn read_whole<K>(cx: &FeedCx<K>, path: &str) -> Result<Option<Vec<u8>>> {
    let Some(file) = cx.read_range(path, 0, 0)? else {
        return Ok(None);
    };
    let length = u32::try_from(file.length).map_err(|_| {
        ProviderError::too_large(format!("{path} has more bytes than a read gives"))
    })?;
    Ok(cx.read_range(path, 0, length)?.map(|file| file.bytes))
}

/// The `message.json` of the message or the reply at `at` in the
/// projection, none without one.
fn projected<K>(cx: &FeedCx<K>, at: &str) -> Result<Option<MessageObject>> {
    read_whole(cx, &format!("{at}/message.json"))?
        .map(|bytes| decode(&bytes))
        .transpose()
}

/// The token of the feed of an object that a feed took, as an inner state
/// of that feed.
enum Inner {
    Message(MessageKey),
    File(FileKey, FileToken),
    ReplyFile(ReplyFileKey, FileToken),
}

/// A part with `token`, followed by more parts for `more`, with the inner
/// states `inner`.
fn feed_part(
    token: &impl Serialize,
    more: bool,
    inner: impl IntoIterator<Item = Inner>,
) -> Result<FeedPart> {
    let mut part = Feed::part(token)?.more(more);
    for inner in inner {
        part = match inner {
            Inner::Message(key) => part.inner::<Message>(&key, &MessageToken::None)?,
            Inner::File(key, token) => part.inner::<File>(&key, &token)?,
            Inner::ReplyFile(key, token) => part.inner::<ReplyFile>(&key, &token)?,
        };
    }
    Ok(part)
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
    fn file(self, ts: &Ts, file: &Id, token: FileToken) -> Inner {
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

/// A message or a reply that a take took, and whether the projection had
/// none at its `ts` before.
struct Taken {
    message: MessageObject,
    /// The message as the projection had it before, none for a new one.
    before: Option<MessageObject>,
}

/// Take the message or the reply `object` of `of` into the projection at its
/// `ts` in `listing`: its folder, `author` with its `user`, empty without
/// one, `text`, `message.json` as it came, and `files/` complete with the
/// folder and `name`, empty for `null`, of each file with a
/// `url_private_download`, without each other file of the projection. A file
/// whose `url_private_download` or `edit_timestamp` is not that of the
/// `message.json` of the projection loses its `content`, and it and each new
/// file give their feed their token. The folder joins `listing` as the write
/// of `listing` of the part says.
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
    let before = projected(cx, &at)?;
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
        let token = FileToken::of(file, url);
        let was = before.as_ref().and_then(|before| {
            before
                .downloads()
                .find(|(was, _)| was.id == file.id)
                .map(|(was, url)| FileToken::of(was, url))
        });
        if was.as_ref() != Some(&token) {
            if was.is_some() {
                cx.remove(format!("{file_at}/content"))?;
            }
            inner.push(of.file(&message.ts, &file.id, token));
        }
        files.insert(file.id.clone());
    }
    let listed = cx
        .names::<Id>(&folder)?
        .map(|listed| listed.names)
        .unwrap_or_default();
    for id in listed.iter().filter(|id| !files.contains(id)) {
        cx.remove(format!("{folder}/{id}"))?;
    }
    Ok(Taken { message, before })
}

/// The `replies/` of the message `message` at `at`: complete and empty for a
/// message without replies, and their removal for replies complete in the
/// projection when the `reply_count` or the `latest_reply` of the message are
/// not those of `before`, the message as the projection had it, so that a
/// thread is asked for again only when Slack tells of another reply.
fn take_replies<K>(
    cx: &FeedCx<K>,
    at: &str,
    message: &MessageObject,
    before: Option<&MessageObject>,
) -> Result<()> {
    let replies = format!("{at}/replies");
    let Some(thread) = message.thread() else {
        return complete_replies(cx, &replies, &BTreeSet::new());
    };
    if !cx
        .names::<Ts>(&replies)?
        .is_some_and(|listed| listed.complete)
    {
        return Ok(());
    }
    if before.and_then(MessageObject::thread) != Some(thread) {
        cx.remove(replies)?;
    }
    Ok(())
}

/// Write `at`, the `replies/` of a message, complete with `replies`, which
/// the answer wrote, without each other reply of the projection.
fn complete_replies<K>(cx: &FeedCx<K>, at: &str, replies: &BTreeSet<Ts>) -> Result<()> {
    cx.dir(at, ListingWrite::Complete)?;
    for reply in listed(cx, at)?.difference(replies) {
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
    /// The page of the listing from `cursor`, none for the first one.
    async fn page<K>(
        self,
        cx: &FeedCx<K>,
        cursor: Option<&String>,
    ) -> Result<std::result::Result<crate::api::Page, Refusal>> {
        let api = cx.endpoint(SlackApi);
        let mut request = match self {
            Self::Users => api.get("/api/users.list").query("limit", PAGE),
            Self::Channels => api
                .get("/api/users.conversations")
                .query("types", "public_channel,private_channel")
                .query("exclude_archived", "false")
                .query("limit", PAGE),
        };
        if let Some(cursor) = cursor {
            request = request.query("cursor", cursor);
        }
        let response = request.send().await?;
        Ok(match self {
            Self::Users => page::<UsersList>(&response),
            Self::Channels => page::<UsersConversations>(&response),
        })
    }

    /// Write the folder of an object of the listing in `folder` with its
    /// `name`, empty for a user without a `real_name`, and the object, and
    /// give its ID.
    fn write<K>(self, cx: &FeedCx<K>, folder: &Path, object: &RawValue) -> Result<Id> {
        let bytes = object.get().as_bytes();
        let (id, name, json) = match self {
            Self::Users => {
                let user: UserObject = decode(bytes)?;
                (user.id, user.profile.real_name, "user.json")
            },
            Self::Channels => {
                let channel: ConversationObject = decode(bytes)?;
                (channel.id, Some(channel.name), "channel.json")
            },
        };
        let at = format!("{folder}/{id}");
        cx.dir(&at, ListingWrite::Partial)?;
        cx.file(format!("{at}/name"), name.unwrap_or_default())?;
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
/// first one, whose part writes the folder of each object of the page; the
/// part of the last page writes the listing complete and removes each entry
/// of it that no page had. A channel that a later listing writes and that
/// the projection did not list before it, as after its removal, gives the
/// feed of its history the token of a channel before its first sweep.
/// `invalid_cursor` restarts the feed with the token of the first page.
async fn full_listing(cx: &FeedCx<WorkspaceKey>, listing: Listing) -> Result<Feed> {
    let folder = cx.path();
    let (cursor, mut ids, before) = match cx.token::<ListingToken>()?.flatten() {
        Some(ListingPage {
            cursor,
            ids,
            before,
        }) => (Some(cursor), ids, before),
        None => (
            None,
            BTreeSet::new(),
            cx.names::<Id>(folder.as_str())?
                .map(|listed| listed.names.into_iter().collect()),
        ),
    };
    let page = match listing.page(cx, cursor.as_ref()).await? {
        Ok(page) => page,
        Err(Refusal {
            of: Refused::Cursor,
            error,
        }) if cursor.is_some() => return Feed::restart(&ListingToken::None, error),
        Err(refusal) => return Err(refusal.error),
    };
    let write = if page.next.is_some() {
        ListingWrite::Partial
    } else {
        ListingWrite::Complete
    };
    cx.dir(folder, write)?;
    let mut back = Vec::new();
    for object in &page.objects {
        let id = listing.write(cx, folder, object)?;
        if matches!(listing, Listing::Channels)
            && before.as_ref().is_some_and(|before| !before.contains(&id))
        {
            back.push(ChannelKey { id: id.clone() });
        }
        ids.insert(id);
    }
    let mut part = if let Some(cursor) = page.next {
        Feed::part(&Some(ListingPage {
            cursor,
            ids,
            before,
        }))?
        .more(true)
    } else {
        let listed = cx
            .names::<Id>(folder.as_str())?
            .map(|listed| listed.names)
            .unwrap_or_default();
        for id in listed.iter().filter(|id| !ids.contains(id)) {
            cx.remove(format!("{folder}/{id}"))?;
        }
        Feed::part(&ListingToken::None)?
    };
    for key in back {
        part = part.inner::<History>(&key, &HistoryToken::unswept())?;
    }
    Ok(part.into())
}

// ===========================================================================
// The feed of the history of a channel
// ===========================================================================

impl History {
    /// The feed of the history of a channel: the next page of the sweep of
    /// its token, else the first one.
    pub(crate) async fn feed(cx: FeedCx<ChannelKey>) -> Result<Feed> {
        let token = cx
            .token::<HistoryToken>()?
            .unwrap_or_else(HistoryToken::unswept);
        sweep(&cx, token).await
    }
}

/// Take the message `object` of the history of the channel of `cx`, with
/// its `replies/`, the inner state of the feed of a new message of the first
/// sweep, and give its `ts`.
fn take_message(
    cx: &FeedCx<ChannelKey>,
    token: &HistoryToken,
    object: &RawValue,
    inner: &mut Vec<Inner>,
) -> Result<Ts> {
    let channel = &cx.key().id;
    let at = cx.path().as_str();
    let Taken { message, before } = take(cx, Of::Channel(channel), at, object, inner)?;
    take_replies(
        cx,
        &format!("{at}/{}", message.ts),
        &message,
        before.as_ref(),
    )?;
    let new = before.is_none();
    let ts = message.ts;
    if new && !token.swept {
        inner.push(Inner::Message(MessageKey {
            id: channel.clone(),
            ts: ts.clone(),
        }));
    }
    Ok(ts)
}

/// A page of `conversations.history` of the sweep of `token`. The last page
/// writes `messages/` complete and removes each message of the projection
/// that the pages lack, and, in the first sweep, older than the newest
/// message of the pages alone: the feed of a message took a newer one by its
/// `ts` meanwhile. `channel_not_found` and `not_in_channel` remove the
/// channel with the token of a channel before its first sweep, and
/// `invalid_cursor` restarts the sweep.
async fn sweep(cx: &FeedCx<ChannelKey>, token: HistoryToken) -> Result<Feed> {
    let (cursor, mut messages) = match token.sweep {
        Some(Sweep { cursor, messages }) => (Some(cursor), messages),
        None => (None, BTreeSet::new()),
    };
    let token = HistoryToken {
        sweep: None,
        ..token
    };
    let mut request = cx
        .endpoint(SlackApi)
        .get("/api/conversations.history")
        .query("channel", &cx.key().id)
        .query("limit", PAGE);
    if let Some(cursor) = &cursor {
        request = request.query("cursor", cursor);
    }
    let page = match page::<ConversationsMessages>(&request.send().await?) {
        Ok(page) => page,
        Err(Refusal {
            of: Refused::Channel,
            ..
        }) => {
            cx.remove(folder(cx.path())?)?;
            return Ok(Feed::part(&HistoryToken::unswept())?.anew().into());
        },
        Err(Refusal {
            of: Refused::Cursor,
            error,
        }) if cursor.is_some() => return Feed::restart(&token, error),
        Err(refusal) => return Err(refusal.error),
    };
    let at = cx.path().as_str();
    let mut inner = Vec::new();
    for object in &page.objects {
        messages.insert(take_message(cx, &token, object, &mut inner)?);
    }
    if let Some(cursor) = page.next {
        cx.dir(at, ListingWrite::Partial)?;
        let token = HistoryToken {
            sweep: Some(Sweep { cursor, messages }),
            ..token
        };
        return Ok(feed_part(&token, true, inner)?.into());
    }
    cx.dir(at, ListingWrite::Complete)?;
    let newest = messages.last();
    for ts in listed(cx, at)?.difference(&messages) {
        if token.swept || newest.is_some_and(|newest| ts < newest) {
            cx.remove(format!("{at}/{ts}"))?;
        }
    }
    let token = HistoryToken {
        swept: true,
        sweep: None,
    };
    Ok(feed_part(&token, false, inner)?.into())
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
    /// The feed of a message: the next page of the replies of its token;
    /// else, for a start, each in `replies`, its replies; else the message
    /// by its `ts` for a message that the projection lacks.
    pub(crate) async fn feed(cx: FeedCx<MessageKey>) -> Result<Feed> {
        let token = cx.token::<MessageToken>()?.flatten();
        let mut feed = MessageFeed {
            cx: &cx,
            messages: folder(cx.path())?,
            inner: Vec::new(),
        };
        if let Some(replies) = token {
            return feed.replies(replies).await;
        }
        if cx.start().is_some() {
            return feed.thread().await;
        }
        feed.message().await
    }
}

impl MessageFeed<'_> {
    /// The path of the `replies/` of the message.
    fn replies_path(&self) -> String {
        format!("{}/replies", self.cx.path())
    }

    /// The replies of the message for a start in `replies`: nothing for
    /// `replies/` complete in the projection, which has each reply, as for a
    /// lookup of a reply that it lacks, and for a message that the
    /// projection does not have; `replies/` complete and empty without a
    /// request for a message without replies; else their first page.
    async fn thread(&mut self) -> Result<Feed> {
        let at = self.replies_path();
        if self.cx.count(&at)?.is_some_and(|listed| listed.complete) {
            return self.part(None);
        }
        match projected(self.cx, self.cx.path().as_str())? {
            None => self.part(None),
            Some(message) if message.thread().is_none() => {
                complete_replies(self.cx, &at, &BTreeSet::new())?;
                self.part(None)
            },
            Some(_) => self.replies(Replies::first()).await,
        }
    }

    /// The message by its `ts` for a message that the projection lacks: its
    /// take, its folder joining `messages/` as complete as it was, or its
    /// removal for an answer without it; nothing for a message that the
    /// projection has, which the sweep of its channel takes.
    async fn message(&mut self) -> Result<Feed> {
        if projected(self.cx, self.cx.path().as_str())?.is_some() {
            return self.part(None);
        }
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
        let page = match page::<ConversationsMessages>(&response) {
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
        self.cx.dir(&self.messages, ListingWrite::Join)?;
        let taken = take(
            self.cx,
            Of::Channel(&key.id),
            self.messages.as_str(),
            object,
            &mut self.inner,
        )?;
        take_replies(
            self.cx,
            self.cx.path().as_str(),
            &taken.message,
            taken.before.as_ref(),
        )?;
        self.part(None)
    }

    /// A page of the replies, written at its answer, by the cursor of the
    /// page before it, with the `ts` of the replies of the pages before it in
    /// the token: each message whose `thread_ts` is the `ts` of the message,
    /// without the message itself; the last page writes `replies/` complete
    /// without each other reply. `thread_not_found` removes the message, `channel_not_found` and
    /// `not_in_channel` the channel, and `invalid_cursor` restarts the
    /// replies.
    async fn replies(&mut self, replies: Replies) -> Result<Feed> {
        let key = self.cx.key();
        let at = self.replies_path();
        let Replies {
            cursor,
            replies: mut taken,
        } = replies;
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
        let page = match page::<ConversationsMessages>(&request.send().await?) {
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
                return Feed::restart(&Some(Replies::first()), error);
            },
            Err(refusal) => return Err(refusal.error),
        };
        let thread = Of::Thread(&key.id, &key.ts);
        for object in &page.objects {
            let message: MessageObject = decode(object.get().as_bytes())?;
            if message.ts != key.ts && message.thread_ts.as_ref() == Some(&key.ts) {
                take(self.cx, thread, &at, object, &mut self.inner)?;
                taken.insert(message.ts);
            }
        }
        if let Some(cursor) = page.next {
            self.cx.dir(&at, ListingWrite::Partial)?;
            return self.part(Some(Replies {
                cursor: Some(cursor),
                replies: taken,
            }));
        }
        complete_replies(self.cx, &at, &taken)?;
        self.part(None)
    }

    /// The removal of the message, with its replies, without the kept pages
    /// of the answer.
    fn removal(&mut self) -> Result<Feed> {
        self.cx.remove(self.cx.path())?;
        Ok(self.anew_part()?.into())
    }

    /// The removal of the channel of the message, without the kept pages of
    /// the answer.
    fn channel_removal(&mut self) -> Result<Feed> {
        self.cx.remove(folder(&self.messages)?)?;
        Ok(self.anew_part()?.into())
    }

    fn anew_part(&mut self) -> Result<FeedPart> {
        Ok(feed_part(&MessageToken::None, false, self.inner.drain(..))?.anew())
    }

    /// A part with the replies of an unfinished answer, followed by more
    /// parts while there are, with the inner states of its messages and
    /// replies.
    fn part(&mut self, replies: Option<Replies>) -> Result<Feed> {
        let more = replies.is_some();
        Ok(feed_part(&replies, more, self.inner.drain(..))?.into())
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

/// The `content` of the file of `cx`: the path of the URL of its token on
/// `SlackFiles` as a blob fetch with the version of its `edit_timestamp`,
/// and without one for a file without it, whose redirects the host does not
/// follow: the bytes of a successful answer alone, which no other URL gave.
async fn content<K>(cx: &FeedCx<K>) -> Result<Feed> {
    let file = cx.token::<FileToken>()?.ok_or_else(|| {
        ProviderError::internal("the feed of a file has no token of the feed that took the file")
    })?;
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
    cx.blob(format!("{}/content", cx.path()), &blob)?;
    Ok(Feed::part(&file)?.into())
}
