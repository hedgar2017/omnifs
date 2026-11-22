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
//! each other operation from the projection; a lookup of that anchor past
//! that deadline changes the attributes of its directory, so that a
//! filesystem lists it again; the kept parts of a later answer continue as
//! for each feed.
//!
//! The host calls the feed `method` once per part of an answer with a
//! [`FeedCx`]: the anchor path and the key of the object, the token the feed
//! returned last, the declared starting path an operation without its fact
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
use std::time::SystemTime;

use crate::browse::{Effects, ListingWrite};
use crate::cx::Cx;
use crate::error::{ProviderError, Result};
use crate::identity::IdentityCaptures;
use crate::object::{Key, Object, ObjectKind};
use omnifs_core::path::Path;
use omnifs_wit::provider::types as wit_types;

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
    /// the inner state an outer feed gave it; `None` before its first part.
    pub fn token(&self) -> Option<&str> {
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

    /// The listing of the mount-absolute `path` in the host projection, as a
    /// count of its entries or their names, with its completeness; `None`
    /// for a path the projection has no listing of. The host answers only
    /// for a path in the subtree of the feed's object.
    pub fn listing(
        &self,
        path: impl AsRef<str>,
        form: ListingForm,
    ) -> Result<Option<ListingFacts>> {
        let facts =
            crate::omnifs::provider::projection::listing(self.cx.id(), path.as_ref(), form.into())?;
        Ok(facts.map(ListingFacts::from))
    }

    /// The bytes of the fed file at the mount-absolute `path` in the host
    /// projection, without the kept parts of an answer; `None` for a path
    /// without one. The host answers only for a path in the subtree of the
    /// feed's object.
    pub fn read(&self, path: impl AsRef<str>) -> Result<Option<Vec<u8>>> {
        Ok(crate::omnifs::provider::projection::file(
            self.cx.id(),
            path.as_ref(),
        )?)
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
    /// size bounds the file, and its bytes cross no boundary of the provider.
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

/// What [`FeedCx::listing`] returns of a listing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListingForm {
    Count,
    Names,
}

impl From<ListingForm> for wit_types::ListingForm {
    fn from(form: ListingForm) -> Self {
        match form {
            ListingForm::Count => Self::Count,
            ListingForm::Names => Self::Names,
        }
    }
}

/// A listing of the host projection with its completeness.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListingFacts {
    pub complete: bool,
    pub entries: ListingEntries,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListingEntries {
    Count(u64),
    Names(Vec<String>),
}

impl From<wit_types::ListingFacts> for ListingFacts {
    fn from(facts: wit_types::ListingFacts) -> Self {
        Self {
            complete: facts.complete,
            entries: match facts.entries {
                wit_types::ListingEntries::Count(count) => ListingEntries::Count(count),
                wit_types::ListingEntries::Names(names) => ListingEntries::Names(names),
            },
        }
    }
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

    /// One part of the answer, with the token after it. The answer ends with
    /// it unless [`FeedPart::more`] says more parts follow.
    pub fn part(token: impl Into<String>) -> FeedPart {
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
    pub fn restart(token: impl Into<String>, error: ProviderError) -> Self {
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
    /// an item of a page of rows: the host keeps it as that feed's state.
    #[must_use]
    pub fn inner<O: Object>(mut self, key: &O::Key, token: impl Into<String>) -> Self {
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
