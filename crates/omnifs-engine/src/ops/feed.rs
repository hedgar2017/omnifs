//! Feed operations: one answer of the feed of an object, published part by
//! part, or, after the end of the first answer of the feed, kept part by
//! part until its last part publishes them all, and the projection reads a
//! feed call makes.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::EngineError;
use crate::Runtime;
use crate::cache::{FeedState, KeptWrite, MountResources, ProjectionTransition};
use crate::clock;
use crate::effect_apply::EffectApplier;
use crate::inspector;
use crate::object_id::ObjectId;
use crate::runtime::Result;
use omnifs_api::events::InspectorOutcome;
use omnifs_core::path::Path;
use omnifs_wit::host::types as wit_types;
use tracing::Instrument;

use super::namespace::SharedError;

/// The object of a feed: its id, which keys its state in the projection, and
/// the path of its anchor, where the provider runs its feed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FeedObject {
    pub(crate) id: ObjectId,
    pub(crate) path: Path,
}

/// A feed that answers for the path of an operation; `starts` marks an
/// operation on one of its starting paths, `own` an own feed, which the
/// operation asks for the facts of its own starting paths after the
/// answering feed of an outer object, and `full_listings` a feed of full
/// listings, which an operation asks after the end of its first answer only
/// for a listing of its anchor and for a lookup of an entry of that listing,
/// on a starting path, without its positive lookup.
#[derive(Debug, Clone)]
pub struct FeedClaim {
    pub(crate) object: FeedObject,
    pub(crate) starts: bool,
    pub(crate) own: bool,
    pub(crate) full_listings: bool,
}

impl FeedClaim {
    pub(crate) fn from_wit(claim: &wit_types::FeedClaim) -> anyhow::Result<Self> {
        Ok(Self {
            object: FeedObject {
                id: ObjectId::from_wit(&claim.object.id),
                path: Path::parse(&claim.object.path).map_err(anyhow::Error::from)?,
            },
            starts: claim.starts,
            own: claim.own,
            full_listings: claim.full_listings,
        })
    }
}

/// The operation on a starting path of a feed that starts an answer, by its
/// path under the anchor of the claim: one without its fact in the
/// projection, or one of an answer of the answering claim past its deadline.
#[derive(Debug, Clone)]
pub(crate) enum FeedStart {
    Lacks(Path),
    Due(Path),
}

impl FeedStart {
    fn to_wit(&self) -> wit_types::FeedStart {
        match self {
            Self::Lacks(path) => wit_types::FeedStart::Lacks(path.as_str().to_string()),
            Self::Due(path) => wit_types::FeedStart::Due(path.as_str().to_string()),
        }
    }
}

/// One call of a feed: the token it continues from, the version of the
/// state of its feed, in a `later` answer, after the end of the first answer
/// of the feed, with `kept` parts, and the count of the publications of the
/// mount when it began.
struct FeedCall {
    token: Option<String>,
    from: Option<u64>,
    later: bool,
    kept: bool,
    began: u64,
}

/// Where the answer of a call of a feed began: the version of the state of
/// its feed, and the path of the fact that its operation lacked.
#[derive(Debug, Clone)]
pub(crate) struct FeedOrigin {
    pub(crate) from: Option<u64>,
    pub(crate) lacked: Option<Path>,
}

/// The effects of one call of an answer, with the counts of the publications
/// of the mount when the call began and when it ended.
pub(crate) struct CallFacts<'a> {
    pub(crate) effects: &'a wit_types::Effects,
    pub(crate) began: u64,
    pub(crate) ended: u64,
}

/// What one call of a feed did: the facts of the feed stand, a part,
/// published or kept, and whether more parts follow it, or nothing, for a
/// call whose feed another answer moved the state of.
enum FeedStep {
    Unchanged,
    Part { more: bool },
    Moved,
}

/// A part of a later answer of a feed, which the projection keeps until the
/// last part of the answer: the part and the effects of its call, with the
/// counts of the publications of the mount when the call began and when it
/// ended. In the projection a kept part is without its token, which the
/// projection has of the last kept part alone, and each blob of its effects
/// is the index of its body in `bodies`: a blob handle is of the process
/// that fetched it.
#[derive(serde::Serialize, serde::Deserialize)]
struct KeptPart {
    part: wit_types::FeedPart,
    effects: wit_types::Effects,
    bodies: Vec<([u8; 32], u64)>,
    began: u64,
    ended: u64,
}

impl KeptPart {
    /// The part for the projection.
    fn encode(mut self, resources: &MountResources) -> Result<Vec<u8>> {
        self.part.token = String::new();
        for write in &mut self.effects.fs {
            if let wit_types::FsKind::File(file) = &mut write.kind
                && let wit_types::ByteSource::Blob(handle) = &mut file.bytes
            {
                let (body, length) = resources
                    .body_for_handle(*handle)
                    .map_err(|error| EngineError::ProviderProtocol(error.to_string()))?;
                *handle = self.bodies.len() as u64;
                self.bodies.push((*body.as_bytes(), length));
            }
        }
        postcard::to_allocvec(&self)
            .map_err(|error| EngineError::ProviderProtocol(error.to_string()))
    }

    /// The part of the projection, each blob of its effects a handle of the
    /// operation `operation`.
    fn decode(bytes: &[u8], resources: &MountResources, operation: u64) -> Result<Self> {
        let mut kept: Self = postcard::from_bytes(bytes)
            .map_err(|error| EngineError::ProviderProtocol(error.to_string()))?;
        for write in &mut kept.effects.fs {
            if let wit_types::FsKind::File(file) = &mut write.kind
                && let wit_types::ByteSource::Blob(handle) = &mut file.bytes
            {
                let (body, length) = usize::try_from(*handle)
                    .ok()
                    .and_then(|index| kept.bodies.get(index))
                    .ok_or_else(|| {
                        EngineError::ProviderProtocol("a kept part lacks the body of a blob".into())
                    })?;
                *handle = resources
                    .body_handle(
                        operation,
                        crate::view::BodyId::from_digest_bytes(*body),
                        *length,
                    )
                    .map_err(|error| EngineError::ProviderProtocol(error.to_string()))?;
            }
        }
        Ok(kept)
    }
}

/// The bodies of the blobs of the kept part `bytes` of the projection, which
/// the part refers to in the body store.
pub(crate) fn kept_bodies(
    bytes: &[u8],
) -> std::result::Result<Vec<crate::view::BodyId>, postcard::Error> {
    let kept: KeptPart = postcard::from_bytes(bytes)?;
    Ok(kept
        .bodies
        .into_iter()
        .map(|(body, _)| crate::view::BodyId::from_digest_bytes(body))
        .collect())
}

/// What one answer of a feed changes: the listings it writes into and the
/// files it writes, which keep their identity in filesystems, and the paths
/// it removes, whose answers filesystems drop.
#[derive(Debug, Default)]
pub(crate) struct FeedChanges {
    pub(crate) listings: Vec<Path>,
    pub(crate) files: Vec<Path>,
    pub(crate) removals: Vec<Path>,
}

/// The feed operations in flight, by correlation id, with the anchor path of
/// their object: the reads of the `projection` import answer only for them
/// and only under that path.
#[derive(Default)]
pub(crate) struct FeedOperations {
    paths: dashmap::DashMap<u64, Path>,
}

/// Removes a feed operation from [`FeedOperations`] when its call returns.
struct FeedOperation<'a> {
    operations: &'a FeedOperations,
    id: u64,
}

impl Drop for FeedOperation<'_> {
    fn drop(&mut self) {
        self.operations.paths.remove(&self.id);
    }
}

impl FeedOperations {
    fn begin(&self, id: u64, path: Path) -> FeedOperation<'_> {
        self.paths.insert(id, path);
        FeedOperation {
            operations: self,
            id,
        }
    }
}

/// The reads of the projection a provider makes through the `projection`
/// import.
#[derive(Clone)]
pub(crate) struct ProjectionReads {
    resources: Arc<MountResources>,
    operations: Arc<FeedOperations>,
}

impl ProjectionReads {
    pub(crate) fn new(resources: Arc<MountResources>, operations: Arc<FeedOperations>) -> Self {
        Self {
            resources,
            operations,
        }
    }

    /// `path`, for the feed operation `id` and a path in the subtree of its
    /// object.
    fn feed_path(
        &self,
        id: u64,
        path: &str,
    ) -> std::result::Result<Path, wit_types::ProviderError> {
        let Some(object) = self.operations.paths.get(&id).map(|entry| entry.clone()) else {
            return Err(refusal(
                wit_types::ErrorKind::Denied,
                "the projection answers only within a feed operation",
            ));
        };
        let path = Path::parse(path)
            .map_err(|error| refusal(wit_types::ErrorKind::InvalidInput, error.to_string()))?;
        if !path.has_prefix(&object) {
            return Err(refusal(
                wit_types::ErrorKind::Denied,
                format!("{path} is outside the feed of {object}"),
            ));
        }
        Ok(path)
    }

    /// At most `len` bytes from `offset` of the body of the fed file at
    /// `path` in the projection, with the length of the body and the time of
    /// its fed mark, for the feed operation `id` and a path in the subtree
    /// of its object; none for a path without a fed file. The body stays in
    /// its store but for the range.
    pub(crate) fn file(
        &self,
        id: u64,
        path: &str,
        offset: u64,
        len: u32,
    ) -> std::result::Result<Option<wit_types::FileRange>, wit_types::ProviderError> {
        let path = self.feed_path(id, path)?;
        if !self.resources.is_fed(&path).map_err(internal)? {
            return Ok(None);
        }
        let Some((body, length)) = self.resources.file_body(&path, None).map_err(internal)? else {
            return Ok(None);
        };
        // The file whose body left since the read of its fact is the file of
        // another answer: none, as for a path without one.
        let Some(bytes) = self
            .resources
            .held_body_range(body, length, offset, len)
            .map_err(internal)?
        else {
            return Ok(None);
        };
        Ok(Some(wit_types::FileRange {
            bytes,
            length,
            until: self.resources.fed_until(&path).map_err(internal)?,
        }))
    }

    /// The names of the listing of `path` in the projection, for the feed
    /// operation `id` and a path in the subtree of its object; none for a
    /// path without a listing.
    pub(crate) fn names(
        &self,
        id: u64,
        path: &str,
    ) -> std::result::Result<Option<wit_types::ListingNames>, wit_types::ProviderError> {
        Ok(self
            .listing(id, path)?
            .map(|listing| wit_types::ListingNames {
                names: listing.names,
                complete: listing.complete,
                until: listing.until,
            }))
    }

    /// The count of the names of the listing of `path` in the projection,
    /// for the paths of [`Self::names`].
    pub(crate) fn count(
        &self,
        id: u64,
        path: &str,
    ) -> std::result::Result<Option<wit_types::ListingCount>, wit_types::ProviderError> {
        self.listing(id, path)?
            .map(|listing| {
                Ok(wit_types::ListingCount {
                    count: u64::try_from(listing.names.len()).map_err(internal)?,
                    complete: listing.complete,
                    until: listing.until,
                })
            })
            .transpose()
    }

    /// The listing of `path` in the projection, for the feed operation `id`
    /// and a path in the subtree of its object: its names, whether it is
    /// complete, and the time of its fed mark, after which it counts as
    /// lacking; none for a path without a listing.
    fn listing(
        &self,
        id: u64,
        path: &str,
    ) -> std::result::Result<Option<Listing>, wit_types::ProviderError> {
        let path = self.feed_path(id, path)?;
        let Some(listing) = self.resources.dirents_payload(&path).map_err(internal)? else {
            return Ok(None);
        };
        Ok(Some(Listing {
            names: listing
                .entries()
                .filter(|entry| !crate::tree::synthetic::is_reserved_provider_leaf(&entry.name))
                .map(|entry| entry.name.clone())
                .collect(),
            complete: listing.is_complete_offline(),
            until: self.resources.fed_until(&path).map_err(internal)?,
        }))
    }
}

/// A listing of the projection as [`ProjectionReads`] gives it.
struct Listing {
    names: Vec<String>,
    complete: bool,
    until: Option<u64>,
}

/// The refusal of a read of the projection that failed in the host.
fn internal(error: impl std::fmt::Display) -> wit_types::ProviderError {
    refusal(wit_types::ErrorKind::Internal, error.to_string())
}

fn refusal(kind: wit_types::ErrorKind, message: impl Into<String>) -> wit_types::ProviderError {
    wit_types::ProviderError {
        kind,
        message: message.into(),
        retryable: false,
        retry_after: None,
    }
}

impl Runtime {
    /// Run one answer of the feed of `object`, started by the operation at
    /// `start`, which lacks its fact at `lacked`, in one flight per object
    /// id, while the state of the feed is still `seen`, the state the caller
    /// decided on. Returns whether this caller ran the answer; a caller that
    /// waited on another answer of the same feed, or whose state another
    /// answer moved before its flight, gets that answer and decides again.
    pub(crate) async fn feed(
        &self,
        object: &FeedObject,
        start: Option<&FeedStart>,
        seen: Option<&FeedState>,
        lacked: Option<&Path>,
    ) -> Result<bool> {
        let led = AtomicBool::new(false);
        let runtime = self;
        self.namespace_flights
            .feed
            .run(
                object.id.clone(),
                || std::future::ready(()),
                || async {
                    let state = runtime
                        .resources
                        .feed_state(object.id.as_bytes())
                        .map_err(|error| EngineError::ProviderProtocol(error.to_string()))?;
                    if state.as_ref() != seen {
                        return Ok(());
                    }
                    // After an answer whose feed another answer moved, the
                    // operation decides again, as after another answer
                    // before its flight.
                    let answered = runtime
                        .run_feed_answer(object, start, lacked)
                        .await
                        .map_err(SharedError::from)?;
                    led.store(answered, Ordering::Relaxed);
                    Ok(())
                },
            )
            .await
            .map_err(SharedError::into_engine)?;
        Ok(led.load(Ordering::Relaxed))
    }

    /// Call the export until a part without `more`. A part of the first
    /// answer of the feed publishes with its token and inner states as it
    /// comes; a part of a later answer is kept, and the last part publishes
    /// the kept parts with it. The answer continues from the token of the
    /// last kept part, else of the last published part, which a failure
    /// keeps. A restart publishes its token, drops the kept parts, and is
    /// the error of the answer. `false` for an answer that another answer
    /// moved the state of the feed under, which publishes nothing.
    async fn run_feed_answer(
        &self,
        object: &FeedObject,
        start: Option<&FeedStart>,
        lacked: Option<&Path>,
    ) -> Result<bool> {
        let stored =
            |error: crate::cache::ProjectionError| EngineError::ProviderProtocol(error.to_string());
        loop {
            let id = object.id.as_bytes();
            let state = self.resources.feed_state(id).map_err(stored)?;
            let kept = state.as_ref().is_some_and(|state| state.kept > 0);
            let flight = self.resources.begin_feed_call().map_err(stored)?;
            let call = FeedCall {
                token: if kept {
                    self.resources.kept_token(id).map_err(stored)?
                } else {
                    self.resources.feed_token(id).map_err(stored)?
                },
                from: state.as_ref().map(|state| state.version),
                later: state.as_ref().is_some_and(|state| state.ended),
                kept,
                began: flight.began,
            };
            match self.run_feed_call(object, call, start, lacked).await? {
                FeedStep::Unchanged | FeedStep::Part { more: false } => return Ok(true),
                FeedStep::Moved => return Ok(false),
                FeedStep::Part { more: true } => {},
            }
        }
    }

    /// One call of the feed of `object`. The `until` of a part is the time
    /// of the facts of the part under the anchor of the object, a part of a
    /// later answer with `more` having none.
    #[allow(clippy::too_many_lines)]
    async fn run_feed_call(
        &self,
        object: &FeedObject,
        call: FeedCall,
        start: Option<&FeedStart>,
        lacked: Option<&Path>,
    ) -> Result<FeedStep> {
        let id = self.next_operation_id();
        let span = inspector::provider_span(
            id,
            &self.mount_name,
            &self.provider_name,
            "feed",
            object.path.as_str(),
        );
        let captured_epoch = self.resources.current_epoch();
        let _blobs = self.resources.operation_blobs(id);
        let tokened = call.token.is_some();
        let FeedCall {
            token,
            from,
            later,
            kept,
            began,
        } = call;
        let origin = FeedOrigin {
            from,
            lacked: lacked.cloned(),
        };
        async {
            let operation = self.feed_operations.begin(id, object.path.clone());
            let (wire_result, effects, until) = self
                .instance
                .feed(
                    id,
                    object.path.as_str().to_string(),
                    token,
                    start.map(FeedStart::to_wit),
                )
                .await?;
            drop(operation);
            let ended = self
                .resources
                .sequence()
                .map_err(|error| EngineError::ProviderProtocol(error.to_string()))?;
            crate::op_validate::validate_feed(&wire_result, &effects, until, tokened, |tree| {
                self.tree_ref(tree).is_some()
            })
            .map_err(EngineError::ProviderProtocol)?;
            super::lifecycle::inspect_result(&span, &wire_result);
            let answer = self.provider_result(wire_result)?;
            // The facts of the part of this call under the anchor of its
            // object, whose time is its `until`: a fact that the part writes
            // elsewhere, as the listing that its object joins, is of another
            // feed.
            let mut deadlined = HashMap::new();
            if let Some(until) = until {
                for write in &effects.fs {
                    let path = Path::parse(&write.path)
                        .map_err(|error| EngineError::ProviderProtocol(error.to_string()))?;
                    if path.has_prefix(&object.path) {
                        deadlined.insert(path, until);
                    }
                }
            }
            let last = KeptPart {
                part: wit_types::FeedPart {
                    token: String::new(),
                    more: false,
                    inner: Vec::new(),
                    anchors: Vec::new(),
                    unlisted: false,
                    anew: false,
                },
                effects,
                bodies: Vec::new(),
                began,
                ended,
            };
            let (answer, parts) = match answer {
                wit_types::FeedAnswer::Part(part) if later && part.more => {
                    if until.is_some() {
                        return Err(EngineError::ProviderProtocol(
                            "a part of a later feed answer with more parts has no until".into(),
                        ));
                    }
                    let kept = self.keep_feed_part(
                        object,
                        KeptPart { part, ..last },
                        &origin,
                        captured_epoch,
                    )?;
                    inspector::record_outcome(&span, InspectorOutcome::Ok);
                    return Ok(if kept {
                        FeedStep::Part { more: true }
                    } else {
                        FeedStep::Moved
                    });
                },
                wit_types::FeedAnswer::Unchanged if kept => {
                    return Err(EngineError::ProviderProtocol(
                        "a feed answered unchanged after the kept parts of its answer".into(),
                    ));
                },
                // A part that declines the kept parts begins the answer:
                // its publication drops them.
                wit_types::FeedAnswer::Part(mut part) if kept && !part.anew => {
                    let mut parts = self
                        .resources
                        .kept_parts(object.id.as_bytes())
                        .map_err(|error| EngineError::ProviderProtocol(error.to_string()))?
                        .iter()
                        .map(|bytes| KeptPart::decode(bytes, &self.resources, id))
                        .collect::<Result<Vec<_>>>()?;
                    // The inner states and the facet anchors of each part in
                    // their order, with the token of the last.
                    let (inner, anchors) = (
                        std::mem::take(&mut part.inner),
                        std::mem::take(&mut part.anchors),
                    );
                    for kept in &mut parts {
                        part.inner.append(&mut kept.part.inner);
                        part.anchors.append(&mut kept.part.anchors);
                    }
                    part.inner.extend(inner);
                    part.anchors.extend(anchors);
                    parts.push(last);
                    (wit_types::FeedAnswer::Part(part), parts)
                },
                answer => (answer, vec![last]),
            };
            let step = match &answer {
                wit_types::FeedAnswer::Part(part) => Ok(FeedStep::Part { more: part.more }),
                wit_types::FeedAnswer::Unchanged => Ok(FeedStep::Unchanged),
                wit_types::FeedAnswer::Restart(restart) => Err(restart.error.clone()),
            };
            let calls = parts
                .iter()
                .map(|part| CallFacts {
                    effects: &part.effects,
                    began: part.began,
                    ended: part.ended,
                })
                .collect::<Vec<_>>();
            let (transition, changes) = EffectApplier::new(&self.resources)
                .lower_feed(
                    object,
                    answer,
                    &calls,
                    deadlined,
                    origin,
                    clock::now_millis(),
                )
                .map_err(|error| EngineError::ProviderProtocol(error.to_string()))?;
            let Some(published) = self.publish_feed_transition(transition, captured_epoch)? else {
                inspector::record_outcome(&span, InspectorOutcome::Ok);
                return Ok(FeedStep::Moved);
            };
            self.record_feed_changes(changes, &published)?;
            match step {
                Ok(step) => {
                    inspector::record_outcome(&span, InspectorOutcome::Ok);
                    Ok(step)
                },
                Err(error) => {
                    inspector::record_outcome(&span, inspector::outcome_for_provider_error(&error));
                    self.provider_result(Err(error))
                },
            }
        }
        .instrument(span.clone())
        .await
    }

    /// Record what the answer of a feed changed, of the `changes` that its
    /// parts name: a filesystem drops its answers for the paths whose facts
    /// the publication changed alone, and the facts that an answer writes as
    /// they were stand.
    fn record_feed_changes(
        &self,
        mut changes: FeedChanges,
        published: &crate::cache::Changed,
    ) -> Result<()> {
        for listing in changes
            .listings
            .iter()
            .filter(|listing| published.listings.contains(*listing))
        {
            let fed = self
                .resources
                .is_fed(listing)
                .map_err(|error| EngineError::ProviderProtocol(error.to_string()))?;
            self.record_dir_changed(listing, fed);
        }
        for file in changes
            .files
            .iter()
            .filter(|file| published.records.contains(*file))
        {
            self.record_file_changed(file);
        }
        changes.removals.retain(|removed| {
            published
                .records
                .iter()
                .any(|path| path.has_prefix(removed))
        });
        self.record_view_invalidations(Vec::new(), changes.removals);
        Ok(())
    }

    /// Keep a part of a later answer of the feed of `object`. Its effects lower first, so that a part that cannot
    /// publish fails at its own call. `false` for a part whose feed another
    /// answer moved the state of, which the projection does not keep.
    fn keep_feed_part(
        &self,
        object: &FeedObject,
        kept: KeptPart,
        origin: &FeedOrigin,
        captured_epoch: u64,
    ) -> Result<bool> {
        EffectApplier::new(&self.resources)
            .lower_feed(
                object,
                wit_types::FeedAnswer::Part(kept.part.clone()),
                &[CallFacts {
                    effects: &kept.effects,
                    began: kept.began,
                    ended: kept.ended,
                }],
                HashMap::new(),
                origin.clone(),
                clock::now_millis(),
            )
            .map_err(|error| EngineError::ProviderProtocol(error.to_string()))?;
        let transition = ProjectionTransition {
            kept: Some(KeptWrite {
                object: object.id.as_bytes().to_vec(),
                from: origin.from,
                token: kept.part.token.clone(),
                began: kept.began,
                anew: kept.part.anew,
                part: kept.encode(&self.resources)?,
            }),
            ..ProjectionTransition::default()
        };
        Ok(self
            .publish_feed_transition(transition, captured_epoch)?
            .is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::KeptPart;
    use crate::cache::Caches;
    use crate::cache::identity::ProjectionId;
    use omnifs_core::MountName;
    use omnifs_wit::host::types as wit_types;

    /// A kept part names the body of each blob of its effects, so that the
    /// operation that publishes it, of another process too, has a handle of
    /// that body: the handle of the call that kept the part is gone.
    #[test]
    fn a_kept_part_names_the_body_of_its_blob_for_the_operation_that_publishes_it() {
        let dir = tempfile::tempdir().unwrap();
        let caches = Caches::open(dir.path()).unwrap();
        let store = caches
            .mount(&MountName::new("m").unwrap(), ProjectionId::new(b"m"), b"m")
            .unwrap();
        let body = store.body.publish(b"blob").unwrap();
        let blob = |kept: &KeptPart| match &kept.effects.fs[0].kind {
            wit_types::FsKind::File(wit_types::FileOut {
                bytes: wit_types::ByteSource::Blob(handle),
                ..
            }) => *handle,
            other => panic!("{other:?}"),
        };

        let keeping = store.operation_blobs(1);
        let handle = store.body_handle(1, body, 4).unwrap();
        let part = KeptPart {
            part: wit_types::FeedPart {
                token: "token".into(),
                more: true,
                inner: Vec::new(),
                anchors: Vec::new(),
                unlisted: false,
                anew: false,
            },
            effects: wit_types::Effects {
                canonical: Vec::new(),
                fs: vec![wit_types::FsWrite {
                    id: None,
                    path: "/file".into(),
                    kind: wit_types::FsKind::File(wit_types::FileOut {
                        content_type: None,
                        attrs: wit_types::FileAttrs {
                            size: wit_types::FileSize::Exact(4),
                            stability: wit_types::Stability::Stable,
                            version_token: None,
                        },
                        bytes: wit_types::ByteSource::Blob(handle),
                    }),
                }],
                invalidations: Vec::new(),
                removals: Vec::new(),
            },
            bodies: Vec::new(),
            began: 3,
            ended: 4,
        };
        let bytes = part.encode(&store).unwrap();
        drop(keeping);
        assert!(store.body_for_handle(handle).is_err());

        let _publishing = store.operation_blobs(2);
        let kept = KeptPart::decode(&bytes, &store, 2).unwrap();
        assert_eq!(store.body_for_handle(blob(&kept)).unwrap(), (body, 4));
        assert_eq!((kept.began, kept.ended), (3, 4));
        assert_eq!(kept.part.token, "", "the projection has the token apart");
    }
}
