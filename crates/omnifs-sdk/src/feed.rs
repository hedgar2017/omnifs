//! Object feeds: an object whose upstream reports its changes keeps the facts
//! of its fed faces in the host projection through one feed.
//!
//! An object declares its feed with `o.feed(method).starts([...])` and its
//! fed faces with `.fed()` (a file face: its lookup and bytes; a dir face: its
//! listing; `o.fed()`: the lookup of the anchor). An operation on a fed path
//! answers `feed` with the claims of the feeds that answer for it, the outer
//! object first, and never runs a request; the host then asks the answering
//! feed for an answer when its facts lack the path, for a lookup only before
//! the end of the first answer of the feed, or are past their deadline, and
//! serves the path from its projection, as not found for a path that it
//! lacks. A lookup of a path that the projection lacks, under a fed listing
//! below the anchor of the answering feed that is not complete, takes the
//! answer of that listing first.
//!
//! An object without faces of its own may anchor at the template of a fed
//! dir face of another object, as the object of the listing of that dir:
//! the dir and each fed face below it have the claims of its feed and of the
//! feeds of the objects below it alone, without the feeds of the objects
//! outer to it.
//!
//! `o.feed(method).own().starts([...])` makes the feed answer the facts of
//! its own starting paths for an upstream without an inventory of them: a
//! lookup below its anchor without its positive lookup starts it as a read or
//! a listing does, after the end of its first answer too, and once the
//! answering feed of an outer object needs no answer, or after its answer
//! that the operation ran, the operation asks the innermost own feed inner to
//! it that starts the operation without its fact, the lookup of its anchor
//! only when the feed has no state or is past its deadline outside the
//! rate-limit window of the mount.
//!
//! `o.feed(method).full_listings().starts([...])` makes a feed of full
//! listings: after the end of its first answer, the host asks it past its
//! deadline only for a listing of its anchor and for a lookup of an entry of
//! that listing, on a starting path, without its positive lookup, and serves
//! each other operation from the projection; the kept parts of a later
//! answer continue as for each feed.
//!
//! The host calls the feed `method` once per part of an answer with a
//! [`FeedCx`]: the anchor path and the key of the object, the token the feed
//! returned last, as the type the feed gave it ([`FeedCx::token`]), the declared starting path an operation without its fact
//! started the answer from ([`FeedCx::start`]), or of the operation of an
//! answer past the deadline of the feed ([`FeedCx::due`]), the listings and
//! the fed files of the projection, and the facts the part writes. The method
//! answers [`Feed::unchanged`], a [`FeedPart`] with the token after it,
//! whether more parts follow, the tokens of the inner feeds it brought, and
//! the time after which its facts count as lacking for the start of an
//! answer while the projection serves them, or [`Feed::restart`], which drops
//! the kept parts of the answer, takes its token as the token of the feed,
//! and gives its error to each operation that waits for the answer.

use std::cell::RefCell;
use std::ops::Deref;
use std::rc::Rc;
use std::str::FromStr;
use std::time::{Duration, SystemTime};

use crate::browse::{Effects, ListingWrite};
use crate::cx::Cx;
use crate::error::{ProviderError, Result};
use crate::identity::IdentityCaptures;
use crate::object::{Key, Object, ObjectKind};
use omnifs_core::path::Path;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// The string the host keeps for `token`: the one encoding of a token, which
/// [`FeedCx::token`] reads. A token that does not encode is the `internal`
/// error of the call.
fn encode(token: &impl Serialize) -> Result<String> {
    serde_json::to_string(token)
        .map_err(|error| ProviderError::internal(format!("a feed token does not encode: {error}")))
}

/// The declared starting path of the operation that started an answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FeedStart {
    /// An operation without its fact in the projection.
    Lacks(&'static str),
    /// An operation of an answer past the deadline of the feed.
    Due(&'static str),
}

/// The context of one feed call. Derefs to [`Cx`] for callouts and state.
pub struct FeedCx<K, S = ()> {
    cx: Cx<S>,
    path: Path,
    key: K,
    token: Option<String>,
    start: Option<FeedStart>,
    facts: Rc<RefCell<Effects>>,
}

impl<K, S> FeedCx<K, S> {
    pub(crate) fn new(
        cx: Cx<S>,
        path: Path,
        key: K,
        token: Option<String>,
        start: Option<FeedStart>,
        facts: Rc<RefCell<Effects>>,
    ) -> Self {
        Self {
            cx,
            path,
            key,
            token,
            start,
            facts,
        }
    }

    /// The anchor path of the feed's object as the host asked for it, which
    /// the paths of its facts extend: a key whose captures normalize names
    /// no path the host serves.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The key of the feed's object.
    pub fn key(&self) -> &K {
        &self.key
    }

    /// The token of the last part the host published for this feed, or of
    /// the inner state an outer feed gave it, as the `T` that
    /// [`Feed::part`], [`FeedPart::inner`], or [`Feed::restart`] took;
    /// `None` before its first part. A token that is no `T` is the
    /// `invalid-input` error of the call, which no retry answers: the host
    /// keeps the state of the feed, and each operation that asks the feed
    /// takes the error until the provider moves the number of its
    /// projection, which a change of the meaning of its tokens asks for.
    pub fn token<T: DeserializeOwned>(&self) -> Result<Option<T>> {
        self.token
            .as_deref()
            .map(|token| {
                serde_json::from_str(token).map_err(|error| {
                    ProviderError::invalid_input(format!(
                        "the token of the feed at {} is not of its type: {error}",
                        self.path
                    ))
                })
            })
            .transpose()
    }

    /// The token as the string the host keeps.
    pub fn raw_token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    /// The declared starting path, relative to the object, of the operation
    /// without its fact in the projection that started this answer (`""` for
    /// the anchor itself), or `None` for an answer the host asks for once the
    /// feed is past its deadline.
    pub fn start(&self) -> Option<&'static str> {
        match self.start {
            Some(FeedStart::Lacks(start)) => Some(start),
            Some(FeedStart::Due(_)) | None => None,
        }
    }

    /// The declared starting path, relative to the object, of the operation
    /// of an answer the host asks for once the feed is past its deadline,
    /// `None` for another answer or an operation on no starting path.
    pub fn due(&self) -> Option<&'static str> {
        match self.start {
            Some(FeedStart::Due(due)) => Some(due),
            Some(FeedStart::Lacks(_)) | None => None,
        }
    }

    /// The names of the listing of the mount-absolute `path` in the host
    /// projection, each parsed as a `T`, as a `String` or as the key of an
    /// entry; `None` for a path the projection has no listing of. A name
    /// that is no `T` is the `internal` error of the call: the feed wrote
    /// it. The host answers only for a path in the subtree of the feed's
    /// object.
    pub fn names<T: FromStr>(&self, path: impl AsRef<str>) -> Result<Option<ListedNames<T>>> {
        let path = path.as_ref();
        crate::omnifs::provider::projection::names(self.cx.id(), path)?
            .map(|listed| {
                Ok(ListedNames {
                    names: listed
                        .names
                        .iter()
                        .map(|name| {
                            name.parse().map_err(|_| {
                                ProviderError::internal(format!("{path} lists {name}"))
                            })
                        })
                        .collect::<Result<_>>()?,
                    complete: listed.complete,
                    until: listed.until.map(time),
                })
            })
            .transpose()
    }

    /// The count of the names of the listing of the mount-absolute `path`
    /// in the host projection; `None` for a path the projection has no
    /// listing of. The host answers only for a path in the subtree of the
    /// feed's object.
    pub fn count(&self, path: impl AsRef<str>) -> Result<Option<ListedCount>> {
        Ok(
            crate::omnifs::provider::projection::count(self.cx.id(), path.as_ref())?.map(
                |listed| ListedCount {
                    count: listed.count,
                    complete: listed.complete,
                    until: listed.until.map(time),
                },
            ),
        )
    }

    /// At most `len` bytes from `offset` of the fed file at the
    /// mount-absolute `path` in the host projection, without the kept parts
    /// of an answer, with the length of the whole file; `None` for a path
    /// without one. No more of the file than the range reaches the memory
    /// of the host or of the provider: a file of any size is read by its
    /// ranges. The host answers only for a path in the subtree of the
    /// feed's object.
    pub fn read_range(
        &self,
        path: impl AsRef<str>,
        offset: u64,
        len: u32,
    ) -> Result<Option<FedRange>> {
        Ok(
            crate::omnifs::provider::projection::file(self.cx.id(), path.as_ref(), offset, len)?
                .map(|range| FedRange {
                    bytes: range.bytes,
                    length: range.length,
                    until: range.until.map(time),
                }),
        )
    }

    /// The whole of the fed file at the mount-absolute `path` in the host
    /// projection, for a file that is one record of at most `max` bytes,
    /// such as the JSON of an object; `None` for a path without one. A
    /// longer file is the `too-large` error of the call, with no more than
    /// `max` bytes of it read: [`Self::read_range`] reads a file of any
    /// size.
    pub fn read_record(&self, path: impl AsRef<str>, max: u32) -> Result<Option<FedRecord>> {
        let path = path.as_ref();
        self.read_range(path, 0, max)?
            .map(|range| {
                if range.length > u64::from(max) {
                    return Err(ProviderError::too_large(format!(
                        "{path} has {} bytes, more than the {max} of a record",
                        range.length
                    )));
                }
                Ok(FedRecord {
                    bytes: range.bytes,
                    until: range.until,
                })
            })
            .transpose()
    }

    /// Write a stable inline file at the mount-absolute `path` as a fact of
    /// this part.
    pub fn file(&self, path: impl AsRef<str>, bytes: impl Into<Vec<u8>>) -> Result<()> {
        self.facts
            .borrow_mut()
            .fed_file(path.as_ref(), bytes.into())?;
        Ok(())
    }

    /// Write a stable file at the mount-absolute `path` as a fact of this
    /// part, with the bytes of `blob`, which the host fetched and holds: no
    /// size bounds the file beside the limit that its mount declares for a
    /// blob fetch, and its bytes cross no boundary of the provider. The host
    /// refuses the blob of a status other than 2xx.
    pub fn blob(&self, path: impl AsRef<str>, blob: &crate::endpoint::BlobHandle) -> Result<()> {
        self.facts
            .borrow_mut()
            .fed_blob(path.as_ref(), blob.id, blob.size)?;
        Ok(())
    }

    /// Write the directory at the mount-absolute `path`; the files and
    /// directories this part writes directly under it join its listing as
    /// `write` says.
    pub fn dir(&self, path: impl AsRef<str>, write: ListingWrite) -> Result<()> {
        self.facts.borrow_mut().directory(path.as_ref(), write)?;
        Ok(())
    }

    /// Retract the facts of the mount-absolute `path` and of everything
    /// beneath it, leaving its parent listing as complete as it was.
    pub fn remove(&self, path: impl AsRef<str>) -> Result<()> {
        self.facts.borrow_mut().remove(path)?;
        Ok(())
    }
}

impl<K, S> Deref for FeedCx<K, S> {
    type Target = Cx<S>;

    fn deref(&self) -> &Cx<S> {
        &self.cx
    }
}

/// The names of a listing of the host projection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListedNames<T> {
    pub names: Vec<T>,
    /// Whether the listing is complete.
    pub complete: bool,
    /// The time after which the listing counts as lacking for the start of
    /// an answer, which the part that wrote it gave with
    /// [`FeedPart::until`]; `None` for a listing without one.
    pub until: Option<SystemTime>,
}

/// The count of the names of a listing of the host projection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ListedCount {
    pub count: u64,
    /// Whether the listing is complete.
    pub complete: bool,
    /// The time after which the listing counts as lacking, as of
    /// [`ListedNames::until`].
    pub until: Option<SystemTime>,
}

/// A range of a fed file of the host projection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FedRange {
    /// The bytes of the range: fewer than asked for at the end of the file.
    pub bytes: Vec<u8>,
    /// The length of the whole file.
    pub length: u64,
    /// The time after which the file counts as lacking, as of
    /// [`ListedNames::until`].
    pub until: Option<SystemTime>,
}

/// The whole of a fed file of the host projection that is one record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FedRecord {
    pub bytes: Vec<u8>,
    /// The time after which the file counts as lacking, as of
    /// [`ListedNames::until`].
    pub until: Option<SystemTime>,
}

/// The time of `millis` milliseconds of the Unix epoch.
fn time(millis: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_millis(millis)
}

/// The answer of one feed call.
pub enum Feed {
    /// The facts of the feed stand.
    Unchanged,
    /// One part of the answer; its facts are the writes of the [`FeedCx`].
    Part(FeedPart),
    /// A restart of the answer, with the token the feed takes and the error
    /// of the operations that wait for the answer.
    Restart { token: String, error: ProviderError },
}

impl Feed {
    /// The facts of the feed stand; valid for a call with a token.
    pub fn unchanged() -> Self {
        Self::Unchanged
    }

    /// One part of the answer, with the token after it, which the next call
    /// reads with [`FeedCx::token`]. The answer ends with it unless
    /// [`FeedPart::more`] says more parts follow.
    pub fn part(token: &impl Serialize) -> Result<FeedPart> {
        encode(token).map(Self::raw_part)
    }

    /// [`Self::part`] with the token as the string the host keeps.
    pub fn raw_part(token: impl Into<String>) -> FeedPart {
        FeedPart {
            token: token.into(),
            more: false,
            inner: Vec::new(),
            until: None,
        }
    }

    /// A restart of the answer; valid for a call with a token. The host
    /// drops the kept parts of the answer, takes `token` as the token of the
    /// feed, with the end of its first answer and its deadline as they were,
    /// and gives `error` to each operation that waits for the answer. The
    /// writes of the call are not facts.
    pub fn restart(token: &impl Serialize, error: ProviderError) -> Result<Self> {
        encode(token).map(|token| Self::raw_restart(token, error))
    }

    /// [`Self::restart`] with the token as the string the host keeps.
    pub fn raw_restart(token: impl Into<String>, error: ProviderError) -> Self {
        Self::Restart {
            token: token.into(),
            error,
        }
    }
}

impl From<FeedPart> for Feed {
    fn from(part: FeedPart) -> Self {
        Self::Part(part)
    }
}

/// One part of a feed answer.
pub struct FeedPart {
    token: String,
    more: bool,
    inner: Vec<InnerState>,
    until: Option<SystemTime>,
}

impl FeedPart {
    /// Whether more parts follow in the same answer.
    #[must_use]
    pub fn more(mut self, more: bool) -> Self {
        self.more = more;
        self
    }

    /// The token of the inner feed of the object of type `O` with `key`, as
    /// an item of a page of rows: the host keeps it as that feed's state,
    /// which the feed of that object reads with [`FeedCx::token`].
    pub fn inner<O: Object>(self, key: &O::Key, token: &impl Serialize) -> Result<Self> {
        encode(token).map(|token| self.raw_inner::<O>(key, token))
    }

    /// [`Self::inner`] with the token as the string the host keeps.
    #[must_use]
    pub fn raw_inner<O: Object>(mut self, key: &O::Key, token: impl Into<String>) -> Self {
        self.inner.push(InnerState {
            kind: O::kind(),
            identity: key.identity_captures(),
            token: token.into(),
        });
        self
    }

    /// The time after which the facts of this part, a file or a complete
    /// listing, count as lacking for the start of an answer, while the
    /// projection serves them. The host refuses it on a part of a later
    /// answer with [`FeedPart::more`], and takes it for the facts of the
    /// last part of a later answer.
    #[must_use]
    pub fn until(mut self, until: SystemTime) -> Self {
        self.until = Some(until);
        self
    }

    pub(crate) fn into_parts(self) -> (String, bool, Vec<InnerState>, Option<SystemTime>) {
        (self.token, self.more, self.inner, self.until)
    }
}

/// An inner feed state, type-erased to the kind and the identity of the
/// logical id of its object.
pub(crate) struct InnerState {
    pub(crate) kind: ObjectKind,
    pub(crate) identity: Vec<(&'static str, String)>,
    pub(crate) token: String,
}

/// The feed method of an object, erased over its future.
pub(crate) type FeedFuture = std::pin::Pin<Box<dyn std::future::Future<Output = Result<Feed>>>>;

/// Run a typed feed method: parse the key and hand it the context.
pub(crate) fn feed_handler<O, Fut>(
    method: fn(FeedCx<O::Key, O::State>) -> Fut,
) -> std::rc::Rc<FeedHandlerFn<O::State>>
where
    O: Object + 'static,
    O::Key: Key + 'static,
    O::State: 'static,
    Fut: std::future::Future<Output = Result<Feed>> + 'static,
{
    std::rc::Rc::new(
        move |cx: Cx<O::State>,
              path: Path,
              caps: &crate::captures::Captures,
              token: Option<String>,
              start: Option<FeedStart>,
              facts: Rc<RefCell<Effects>>| {
            match <O::Key as crate::captures::FromCaptures>::from_captures(caps) {
                Ok(key) => {
                    Box::pin(method(FeedCx::new(cx, path, key, token, start, facts))) as FeedFuture
                },
                Err(error) => Box::pin(async move { Err(error) }),
            }
        },
    )
}

/// The `dyn Fn` an object's feed boxes: the context of the call, the anchor
/// path and its captures, the token, the declared starting path, and the
/// facts it writes.
pub(crate) type FeedHandlerFn<S> = dyn Fn(
    Cx<S>,
    Path,
    &crate::captures::Captures,
    Option<String>,
    Option<FeedStart>,
    Rc<RefCell<Effects>>,
) -> FeedFuture;
