//! Internal whole-file read result types and provider-read execution.
//!
//! This is the renderer-neutral read policy shared by FUSE and NFS: the cache
//! cascade, the per-mount `op_gen` write fence, the canonical-not-copied hybrid,
//! and learned-size promotion. It is fully async (no `block_on`): the renderer
//! drives the internal read path from its own executor and turns the neutral
//! `ReadResult` into kernel/protocol identity + reply encoding.

use crate::cache::{FactPayload, MountResources, ProjectionTransition, RecordKind, RecordWrite};
use crate::ops::namespace::{ReadBytes, ReadOutcome, ReadReply};
use crate::pagination::NextPageOutcome;
use crate::view as view_types;
use crate::view::{AttrPayload, EntryMeta, FileAttrsCache, LookupPayload};
use crate::{EngineError, Runtime};
use tracing::warn;

use super::MATERIALIZE_MAX_BYTES;
use super::error::{Result, TreeError};
use super::node::{Node, PaginationControl, SyntheticContent};
use crate::view::BodyId;
use crate::{EngineNamespace, RequestCtx};
use omnifs_api::events::CacheKind;

/// Result of the internal read path. A host-tree node is read via
/// renderer std::fs passthrough over a real dir) can never be confused with
/// resolved provider bytes. `Bytes.attrs` is the POST-read learned attrs (exact
/// size promoted from the bytes) the renderer applies to st_size / the NFSv4
/// change attribute. On a cache hit it is the node's projected attrs (the size
/// was already learned when the entry was first materialized).
#[derive(Debug, Clone)]
pub(crate) struct ReadResult {
    pub(crate) data: Vec<u8>,
    pub(crate) attrs: Option<FileAttrsCache>,
}

/// The bytes of a read of a range of a file, whether they reach its end, and
/// the attrs it learned.
pub(crate) struct ReadRange {
    pub(crate) bytes: Vec<u8>,
    pub(crate) eof: bool,
    pub(crate) attrs: Option<FileAttrsCache>,
}

impl ReadRange {
    /// At most `len` bytes from `offset` of the whole file of `result`.
    fn of(result: ReadResult, offset: u64, len: u32) -> Self {
        let ReadResult { data, attrs } = result;
        let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(data.len());
        let end = start.saturating_add(len as usize).min(data.len());
        Self {
            bytes: data[start..end].to_vec(),
            eof: end >= data.len(),
            attrs,
        }
    }
}

/// A read of a file: the whole file, for bytes that the host has in memory
/// as they came, or the range of the read from a body of the body store,
/// without the rest of the body in memory.
enum FileRead {
    Whole(ReadResult),
    Range(ReadRange),
}

/// One ranged chunk from a `RangedHandle`. `learned_attrs` is `Some` on an
/// EOF-short read when an exact size was learned, so the renderer promotes
/// st_size.
#[derive(Debug, Clone)]
pub struct Chunk {
    pub bytes: Vec<u8>,
    pub eof: bool,
    pub learned_attrs: Option<FileAttrsCache>,
}

pub(crate) struct FileAttrStore<'a> {
    resources: &'a MountResources,
    path: &'a omnifs_core::path::Path,
}

impl<'a> FileAttrStore<'a> {
    pub(crate) fn new(resources: &'a MountResources, path: &'a omnifs_core::path::Path) -> Self {
        Self { resources, path }
    }

    pub(crate) fn cached(&self, respect_expiry: bool) -> Result<Option<FileAttrsCache>> {
        let lookup = if respect_expiry {
            self.resources.view_get(self.path, RecordKind::Lookup, None)
        } else {
            self.resources
                .cache_get(self.path, RecordKind::Lookup, None)
        }
        .map_err(|error| TreeError::internal(error.to_string()))?;
        if let Some(record) = lookup {
            let payload: LookupPayload = postcard::from_bytes(&record.payload)
                .map_err(|error| TreeError::internal(error.to_string()))?;
            if let LookupPayload::Positive(meta) = payload
                && let Some(attrs) = meta.into_attrs()
            {
                return Ok(Some(attrs));
            }
        }

        let attrs = if respect_expiry {
            self.resources.view_get(self.path, RecordKind::Attr, None)
        } else {
            self.resources.cache_get(self.path, RecordKind::Attr, None)
        }
        .map_err(|error| TreeError::internal(error.to_string()))?
        .map(|record| {
            postcard::from_bytes::<AttrPayload>(&record.payload)
                .map_err(|error| TreeError::internal(error.to_string()))
                .map(|payload| payload.meta.into_attrs())
        })
        .transpose()?
        .flatten();
        Ok(attrs)
    }

    pub(crate) fn publish(&self, attrs: FileAttrsCache, captured_epoch: u64) -> Result<()> {
        let meta = EntryMeta::file(attrs);
        self.resources
            .publish(
                ProjectionTransition {
                    records: vec![
                        RecordWrite {
                            path: self.path.clone(),
                            aux: None,
                            fact: FactPayload::Lookup(LookupPayload::Positive(meta.clone())),
                        },
                        RecordWrite {
                            path: self.path.clone(),
                            aux: None,
                            fact: FactPayload::Attr(AttrPayload { meta }),
                        },
                    ],
                    ..ProjectionTransition::default()
                },
                captured_epoch,
            )
            .map_err(|error| TreeError::internal(error.to_string()))?;
        Ok(())
    }
}

impl EngineNamespace {
    /// At most `len` bytes of a file from `offset`: from its body in the
    /// body store, which no size bounds, for each file whose bytes the
    /// store has.
    pub(crate) async fn read(
        &self,
        node: &Node,
        offset: u64,
        len: u32,
        ctx: &RequestCtx,
    ) -> Result<ReadRange> {
        Ok(match self.read_file(node, offset, len, ctx).await? {
            FileRead::Whole(result) => ReadRange::of(result, offset, len),
            FileRead::Range(range) => range,
        })
    }

    /// Whole-file read. Owns the shared read cache cascade (exact-0
    /// short-circuit, mem hit, durable view hit, backing-fs read), then on a
    /// view miss the cold provider render
    /// fenced by the per-mount `op_gen`, with the canonical-not-copied hybrid
    /// and learned-size promotion.
    ///
    /// The renderer still owns its kernel-side handle caches (the per-`fh` whole
    /// buffer FUSE keeps, the inode size promotion) and kernel offset/size
    /// slicing; the internal read path returns the whole rendered file. A node
    /// under an anchor of another facet reads the path of its facts. A fed
    /// file gives the range of the read alone.
    async fn read_file(
        &self,
        node: &Node,
        offset: u64,
        len: u32,
        _ctx: &RequestCtx,
    ) -> Result<FileRead> {
        let node = &node.at_fact();
        // A host-synthesized node (a mount-root ignore file or a `@next`/`@all`
        // pagination control) is served by `Tree`, never the provider. The
        // renderer materializes the result into a per-handle buffer so a partial
        // or repeated read never re-runs the (mutating) control action.
        if let Some(synthetic) = node.synthetic_kind() {
            return self
                .read_synthetic(node, &synthetic.content)
                .await
                .map(FileRead::Whole);
        }

        // A host-tree node is served by the engine from the retained capability
        // dir; `Tree` hands the path back without a provider round trip.
        if node.host().is_some() {
            return Err(TreeError::is_directory(node.path().as_str()));
        }

        let entry = self.entry_for(node.mount())?;
        let resources = entry.resources()?;
        let offline = entry.runtime().is_none();
        let path = node.path();
        let captured_epoch = resources.current_epoch();
        let attr_store = FileAttrStore::new(resources, path);
        // A fed file is served once its feeds have answered, so its read asks
        // the provider for their claims; without the provider, the
        // projection serves the range of the body that it has.
        if resources
            .is_fed(path)
            .map_err(|error| TreeError::internal(error.to_string()))?
        {
            let Some(runtime) = entry.runtime() else {
                return fed_range(resources, node.fact(), offset, len, false)?.ok_or_else(|| {
                    TreeError::offline_miss(format!("offline read has no body for {path}"))
                });
            };
            let content_type = path.content_type_mime(None).to_string();
            return match runtime
                .read_file(path, content_type, captured_epoch)
                .await?
            {
                ReadReply::File(result) => finish_read(&runtime, path, result, offset, len),
                ReadReply::Feed(claims) => {
                    serve_fed_read(&runtime, path, &claims, offset, len).await
                },
            };
        }
        if let Some(read) = stored_read(
            &attr_store,
            resources,
            node,
            offline,
            captured_epoch,
            offset,
            len,
        )? {
            return Ok(read);
        }

        if offline {
            return Err(TreeError::offline_miss(format!(
                "offline read has no complete body for {path}"
            )));
        }

        let runtime = entry.runtime().expect("online entry has a runtime");

        // Cold miss. Derive the content type the host echoes into `read-file`:
        // the path's representation suffix wins, else octet-stream (the
        // SDK-supplied content type is unavailable on a cold read).
        let content_type = node.path().content_type_mime(None).to_string();

        // Capture the invalidation epoch BEFORE awaiting the render so the result can be
        // fenced against an invalidation that lands mid-read.
        crate::inspector::cache_event(CacheKind::FileMiss);
        let result = match runtime.read_file(path, content_type, captured_epoch).await {
            Ok(ReadReply::File(result)) => result,
            Ok(ReadReply::Feed(claims)) => {
                return serve_fed_read(&runtime, path, &claims, offset, len).await;
            },
            Err(EngineError::ProviderError(error)) => {
                warn!(
                    path = %path,
                    kind = ?error.kind,
                    retryable = error.retryable,
                    message = error.message,
                    "provider returned typed error for read_file"
                );
                return Err(EngineError::ProviderError(error).into());
            },
            Err(error) => {
                warn!(path = %path, error = %error, "read_file runtime error");
                return Err(error.into());
            },
        };

        finish_read(&runtime, path, result, offset, len)
    }

    /// Serve a host-synthesized node. A `Fixed` synthetic (a mount-root ignore
    /// file) returns its static bytes with the node's projected attrs; a
    /// `PaginationControl` runs the accumulating pagination action over the
    /// parent directory, invalidates the parent's cached dirents so a later
    /// listing reflects the grown feed, and returns a one-line status with a
    /// learned exact size so `cat` reads the whole message.
    async fn read_synthetic(&self, node: &Node, content: &SyntheticContent) -> Result<ReadResult> {
        match content {
            SyntheticContent::Fixed(bytes) => Ok(ReadResult {
                data: materialized_bytes(node.path(), bytes.clone())?,
                attrs: node.attrs().cloned(),
            }),
            SyntheticContent::PaginationControl(action) => {
                let runtime = self.runtime_for(node.mount())?;
                let Some((parent, _)) = node.path().parent_and_name() else {
                    return Err(TreeError::invalid_input(format!(
                        "pagination control has no parent: {}",
                        node.path().as_str()
                    )));
                };
                let status = match action {
                    PaginationControl::All => runtime.paginate_all(&parent).await,
                    PaginationControl::Next => match runtime.paginate_next(&parent).await {
                        NextPageOutcome::Loaded { added, more } => format!(
                            "loaded +{added} entries; {}\n",
                            if more { "more available" } else { "complete" }
                        ),
                        NextPageOutcome::NoMore => "no more pages\n".to_string(),
                        NextPageOutcome::Error(message) => message,
                    },
                };
                // The action grew (or exhausted) the parent's accumulated
                // dirents; drop the parent's mem listing so a later browse
                // re-reads the stored record. The kernel re-list notify stays
                // renderer-side (driven from the InvalidationReport).
                runtime
                    .resources
                    .memory_invalidate(&parent, RecordKind::Dirents, None);
                let bytes = status.into_bytes();
                enforce_observed_materialize_cap(node.path(), bytes.len())?;
                let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                Ok(ReadResult {
                    data: bytes,
                    attrs: Some(super::synthetic::control_read_attrs(len)),
                })
            },
        }
    }
}

/// Serve at most `len` bytes from `offset` of a fed file from the body of its
/// file record in the projection once the answering feed among `claims` has
/// answered, or as not found.
async fn serve_fed_read(
    runtime: &Runtime,
    path: &omnifs_core::path::Path,
    claims: &[crate::ops::feed::FeedClaim],
    offset: u64,
    len: u32,
) -> Result<FileRead> {
    let (fact, _) =
        super::feed::answer_claims(runtime, path, super::feed::FedFact::File, claims).await?;
    fed_range(&runtime.resources, &fact, offset, len, true)?
        .ok_or_else(|| TreeError::not_found(fact.as_str()))
}

/// At most `len` bytes from `offset` of the body of the file record of the
/// fed file at `path` in the projection, in a mount that is `online`, with
/// its provider; none for a path without one.
fn fed_range(
    resources: &MountResources,
    path: &omnifs_core::path::Path,
    offset: u64,
    len: u32,
    online: bool,
) -> Result<Option<FileRead>> {
    for _ in 0..2 {
        let Some((body, length)) = resources
            .file_body(path, None)
            .map_err(|error| TreeError::internal(error.to_string()))?
        else {
            return Ok(None);
        };
        let attrs = FileAttrStore::new(resources, path).cached(online)?;
        if let Some(range) = body_range(resources, body, length, offset, len, attrs)? {
            return Ok(Some(range));
        }
    }
    Err(lost_body(path))
}

/// The error of a fact of `path` whose body the store lacks at two reads of
/// the fact in a row.
fn lost_body(path: &omnifs_core::path::Path) -> TreeError {
    TreeError::internal(format!(
        "the projection refers to a body of {path} that the body store lacks"
    ))
}

/// The range of a read from `body` of `length` bytes, with `attrs` as the
/// attrs of its file; none for a body that the store has no more.
fn body_range(
    resources: &MountResources,
    body: BodyId,
    length: u64,
    offset: u64,
    len: u32,
    attrs: Option<FileAttrsCache>,
) -> Result<Option<FileRead>> {
    let Some(bytes) = resources
        .held_body_range(body, length, offset, len)
        .map_err(|error| TreeError::internal(error.to_string()))?
    else {
        return Ok(None);
    };
    let eof = offset.saturating_add(bytes.len() as u64) >= length;
    Ok(Some(FileRead::Range(ReadRange { bytes, eof, attrs })))
}

/// The attrs of a file with `attrs` whose whole content has `length` bytes.
fn learned(
    path: &omnifs_core::path::Path,
    attrs: &FileAttrsCache,
    length: u64,
) -> Result<FileAttrsCache> {
    usize::try_from(length)
        .map_err(|error| error.to_string())
        .and_then(|length| attrs.learned_complete_content_attrs(length))
        .map_err(|error| {
            TreeError::internal(format!(
                "the body of {path} contradicts its file attrs: {error}"
            ))
        })
}

/// The read of a file from what the projection has of it: the bytes of its
/// attrs, or a range of its body in the body store, as the body of its
/// attrs, the canonical of its object, or its file record. None for a file
/// that the projection has no bytes of.
fn stored_read(
    attr_store: &FileAttrStore<'_>,
    resources: &MountResources,
    node: &Node,
    offline: bool,
    captured_epoch: u64,
    offset: u64,
    len: u32,
) -> Result<Option<FileRead>> {
    let path = node.path();
    let internal = |error: crate::cache::ProjectionError| TreeError::internal(error.to_string());
    // A fact that a publication replaced between its read and the read of
    // its body names a body that the store has no more: the next read of
    // the facts has the body that replaced it.
    for _ in 0..2 {
        let expired = !offline && resources.view_expired(path).map_err(internal)?;
        let projected = attr_store
            .cached(!offline)?
            .or_else(|| (!expired).then(|| node.attrs().cloned()).flatten());
        let Some(attrs) = projected.as_ref() else {
            return Ok(None);
        };
        if matches!(attrs.size(), view_types::FileSize::Exact(0))
            && (!offline || !matches!(attrs.byte_source(), view_types::ByteSource::Deferred(_)))
        {
            return Ok(Some(FileRead::Whole(ReadResult {
                data: Vec::new(),
                attrs: Some(attrs.clone()),
            })));
        }
        if let Some(bytes) = attrs.inline_bytes() {
            let learned = learned(path, attrs, bytes.len() as u64)?;
            if !offline {
                attr_store.publish(learned.clone(), captured_epoch)?;
            }
            return Ok(Some(FileRead::Whole(ReadResult {
                data: bytes.to_vec(),
                attrs: Some(learned),
            })));
        }
        let stored = match attrs.byte_source() {
            view_types::ByteSource::Body(body) => {
                let view_types::FileSize::Exact(length) = attrs.size() else {
                    return Err(TreeError::internal(
                        "validated durable body metadata lost its exact length",
                    ));
                };
                Some((body, length))
            },
            view_types::ByteSource::Canonical => {
                resources.canonical_body(path).map_err(internal)?
            },
            view_types::ByteSource::Inline(_) | view_types::ByteSource::Deferred(_) => None,
        };
        let stored = match stored {
            Some(stored) => Some(stored),
            None => file_record(resources, path, attrs, offline, expired)?,
        };
        let Some((body, length)) = stored else {
            return Ok(None);
        };
        let learned = learned(path, attrs, length)?;
        if let Some(range) = body_range(resources, body, length, offset, len, Some(learned))? {
            crate::inspector::cache_event(CacheKind::FileHit);
            return Ok(Some(range));
        }
    }
    // The projection has no bytes of a file whose body the store lacks:
    // its provider answers the read.
    Ok(None)
}

/// The body and the length of the file record of `path` that is still a
/// faithful answer for `attrs`: of the version of a dynamic file, and of a
/// size that the attrs allow. None past the deadline of the path of a mount
/// with its provider, which `expired` tells.
fn file_record(
    resources: &MountResources,
    path: &omnifs_core::path::Path,
    attrs: &FileAttrsCache,
    offline: bool,
    expired: bool,
) -> Result<Option<(BodyId, u64)>> {
    let aux = if offline {
        attrs.durable_observation_cache_aux()
    } else {
        attrs.online_cache_aux()
    };
    let Some(aux) = aux.filter(|_| !expired) else {
        return Ok(None);
    };
    let Some((version_token, body, length)) = resources
        .file_fact(path, aux.as_deref())
        .map_err(|error| TreeError::internal(error.to_string()))?
    else {
        return Ok(None);
    };
    if matches!(attrs.stability(), view_types::Stability::Dynamic)
        && version_token.as_deref() != attrs.version_token()
    {
        return Ok(None);
    }
    let faithful =
        usize::try_from(length).is_ok_and(|length| attrs.validate_complete_content(length).is_ok());
    Ok(faithful.then_some((body, length)))
}

/// Resolve a cold `read-file` terminal into the read of its file: the whole
/// of inline bytes, which came through the provider, and a range of the
/// body of a blob or of a canonical, which the body store has.
fn finish_read(
    runtime: &Runtime,
    path: &omnifs_core::path::Path,
    result: ReadOutcome,
    offset: u64,
    len: u32,
) -> Result<FileRead> {
    let attrs = result.attrs;
    let contradicts = |error: String, actual: u64| {
        warn!(
            path = %path,
            expected = ?attrs.size(),
            actual,
            error,
            "provider returned bytes that contradict file attrs"
        );
        TreeError::internal(format!(
            "read for {path} returned bytes that contradict file attrs"
        ))
    };
    let stored = match result.bytes {
        ReadBytes::Inline(data) => {
            enforce_observed_materialize_cap(path, data.len())?;
            let attrs = attrs
                .learned_complete_content_attrs(data.len())
                .map_err(|error| contradicts(error, data.len() as u64))?;
            return Ok(FileRead::Whole(ReadResult {
                data,
                attrs: Some(attrs),
            }));
        },
        ReadBytes::Body(body) => match attrs.size() {
            view_types::FileSize::Exact(length) => Some((body, length)),
            view_types::FileSize::NonZero | view_types::FileSize::Unknown => None,
        },
        ReadBytes::Canonical => runtime
            .resources
            .canonical_body(path)
            .map_err(|error| TreeError::internal(error.to_string()))?,
    };
    let Some((body, length)) = stored else {
        warn!(path = %path, "read answered a byte source that the projection lacks");
        return Err(TreeError::internal(format!(
            "read for {path} could not resolve its byte source"
        )));
    };
    let attrs = usize::try_from(length)
        .map_err(|error| error.to_string())
        .and_then(|length| attrs.learned_complete_content_attrs(length))
        .map_err(|error| contradicts(error, length))?;
    body_range(&runtime.resources, body, length, offset, len, Some(attrs))?
        .ok_or_else(|| lost_body(path))
}

fn enforce_observed_materialize_cap(path: &omnifs_core::path::Path, size: usize) -> Result<()> {
    let size = u64::try_from(size).unwrap_or(u64::MAX);
    if size <= MATERIALIZE_MAX_BYTES {
        return Ok(());
    }
    Err(TreeError::too_large(format!(
        "full read for {path} materialized {size} bytes, above cap {MATERIALIZE_MAX_BYTES}"
    )))
}

fn materialized_bytes(path: &omnifs_core::path::Path, data: Vec<u8>) -> Result<Vec<u8>> {
    enforce_observed_materialize_cap(path, data.len())?;
    Ok(data)
}
