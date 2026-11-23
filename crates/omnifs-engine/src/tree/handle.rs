//! Ranged read handle and the provider open probe.
//!
//! The internal open path probes the provider for a deferred file and returns a
//! runtime-owned `RangedHandle` when the source is ranged (`None` when it is
//! not, so the renderer falls through to a full read). `RangedHandle::read`
//! drives `Namespace::read_chunk` and learns the exact size on an EOF-short
//! read, growing a live file monotonically. Fully async (no `block_on`): the
//! renderer binds the handle to its own kernel handle (FUSE fh / NFS stateid)
//! and drives reads from its own executor.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::view as view_types;
use crate::view::FileAttrsCache;
use crate::{EngineError, Runtime};
use omnifs_core::path::Path;

use super::error::{Result, TreeError};
use super::node::Node;
use super::read::Chunk;
use crate::EngineNamespace;

/// Runtime-owned ranged read handle for `Deferred(Ranged)` files. Holds an
/// `Arc<Runtime>` so it is self-contained across renderer calls. The renderer
/// binds it to its own kernel handle (FUSE fh / NFS stateid) and owns ONLY that
/// mapping + lease; the provider handle (u64) and chunk validation are neutral.
pub struct RangedHandle {
    pub(crate) runtime: Arc<Runtime>,
    pub(crate) path: Path,
    pub(crate) provider_handle: u64,
    pub(crate) attrs: FileAttrsCache,
    /// Monotonic high-water mark of the observed upstream end, shared with the
    /// renderer's live-follow loop. A `Live` read grows it on EOF; the loop
    /// probes from it and advances it. The renderer reports this size through
    /// its own getattr so a polling `tail -f` sees the file grow.
    pub(crate) observed_end: Arc<AtomicU64>,
    pub(crate) open_epoch: u64,
}

impl RangedHandle {
    pub(crate) fn attrs(&self) -> &FileAttrsCache {
        &self.attrs
    }

    /// Drive provider `read_chunk` for one ranged read; learn the exact size on
    /// an EOF-short read. The typed Runtime boundary validates the requested
    /// length before this handle-level EOF and projected-attr bookkeeping.
    pub(crate) async fn read(&self, offset: u64, length: u32) -> Result<Chunk> {
        let chunk = match self
            .runtime
            .read_chunk_with_attrs(
                &self.path,
                &self.attrs,
                self.provider_handle,
                offset,
                length,
                self.open_epoch,
            )
            .await
        {
            Ok(chunk) => chunk,
            Err(EngineError::ProviderError(error)) => {
                return Err(EngineError::ProviderError(error).into());
            },
            Err(error) => return Err(error.into()),
        };

        let learned_attrs = chunk.learned_attrs.clone();
        if let Some(attrs) = &learned_attrs
            && matches!(self.attrs.stability(), view_types::Stability::Live)
        {
            self.observed_end
                .fetch_max(attrs.st_size(), Ordering::Relaxed);
        }

        Ok(Chunk {
            bytes: chunk.content,
            eof: chunk.eof,
            learned_attrs,
        })
    }

    /// Release the provider handle without consuming the handle. Used by the
    /// namespace handle cache, which owns the sole reference to a cached handle
    /// and closes it exactly once at eviction, where a consuming `close` cannot
    /// be called through a shared owner.
    pub(crate) fn release(&self) -> Result<()> {
        self.runtime
            .call_close_file(self.provider_handle)
            .map_err(Into::into)
    }
}

impl EngineNamespace {
    /// Probe `Namespace::open_file` for a deferred file and, when the provider's
    /// source is ranged, return a runtime-owned `RangedHandle` the renderer binds
    /// to its kernel handle. A cheap lookup leaves only a `Deferred(Full)`
    /// placeholder on the node, so the real read mode is discovered here: a
    /// non-ranged source reports `InvalidInput`, and a path with no file route
    /// (an object representation or projected leaf) reports `NotFound`. Either
    /// way this returns `Ok(None)` so the renderer falls through to the full read
    /// path.
    pub(crate) async fn open(&self, node: &Node) -> Result<Option<RangedHandle>> {
        // Without a runtime that serves it, the projection serves the read.
        let Some(runtime) = self.entry_for(node.mount())?.runtime() else {
            return Ok(None);
        };
        let captured_epoch = runtime.resources.current_epoch();
        let projected = node.attrs().ok_or_else(|| {
            TreeError::invalid_input(format!(
                "open requires a deferred file projection: {}",
                node.path().as_str()
            ))
        })?;
        if !projected.is_deferred() {
            return Err(TreeError::invalid_input(format!(
                "open requires byte-source::deferred: {}",
                node.path().as_str()
            )));
        }
        if projected.is_deferred_full() {
            return Ok(None);
        }

        let opened = match runtime.open_file(node.path(), captured_epoch).await {
            Ok(opened) => opened,
            Err(error) if error.is_provider_not_found_or_invalid_input() => return Ok(None),
            Err(error) => return Err(error.into()),
        };

        let attrs = opened.attrs;
        Ok(Some(RangedHandle {
            runtime,
            path: node.path().clone(),
            provider_handle: opened.handle,
            attrs,
            observed_end: Arc::new(AtomicU64::new(0)),
            open_epoch: captured_epoch,
        }))
    }

    /// Probe a deferred ranged file's real attrs by opening it through the
    /// provider, then immediately closing the provider handle. This is for
    /// filesystems such as NFS that must render child attrs during directory
    /// flattening. The probe is intentionally named as provider I/O, and the
    /// learned attrs are published through the shared view cache before returning.
    pub(crate) async fn probe_ranged_attrs(
        &self,
        mount: &str,
        path: &Path,
    ) -> Result<Option<FileAttrsCache>> {
        let Some(runtime) = self.entry_for(mount)?.runtime() else {
            return Ok(None);
        };
        let captured_epoch = runtime.resources.current_epoch();
        let opened = match runtime.open_file(path, captured_epoch).await {
            Ok(opened) => opened,
            Err(error) if error.is_provider_not_found_or_invalid_input() => return Ok(None),
            Err(error) => return Err(error.into()),
        };

        let attrs = opened.attrs;
        let validation = attrs.validate().map_err(|error| {
            TreeError::internal(format!(
                "open-file returned invalid attrs for {path}: {error}"
            ))
        });
        let close = runtime.call_close_file(opened.handle);
        if let Err(error) = close {
            tracing::warn!(path = %path, error = %error, "ranged attr probe close failed");
        }
        validation?;
        Ok(Some(attrs))
    }
}
