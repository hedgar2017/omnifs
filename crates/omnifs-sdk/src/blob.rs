//! Typed async blob callout builders.
//!
//! Blobs keep large bodies host-side by design.
//! `cx.http().get(url).into_blob().send()` (or the typed
//! [`crate::endpoint::RequestBuilder::into_blob`]) lands the response body in
//! the host's blob cache and returns a [`BlobRef`] carrying metadata only.
//! The host derives the request identity and body generation from validated
//! request facts, so providers never name cache entries.
//!
//! A stored blob is consumed by:
//! - [`crate::projection::FileProjection::blob`]: serve the bytes verbatim
//!   from a file route; the host reads them without guest involvement.

use crate::error::{ProviderError, Result};
use omnifs_wit::provider::types::{BlobFetchRequest, BlobFetched, Callout, CalloutResult, Header};

/// Runtime-local handle for a blob stored in the host cache. Valid only for
/// the current provider instance; do not persist it or derive paths from it.
/// The host-owned cache reference, not this id, selects a rehydrated blob.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlobId(pub(crate) u64);

impl BlobId {
    /// Return the raw WIT `blob-id` value.
    pub fn raw(self) -> u64 {
        self.0
    }
}

/// Metadata returned by a blob fetch.
///
/// The response body stays in the host cache. Use [`Self::id`] when passing
/// the blob to [`crate::projection::FileProjection::blob`].
/// The fetch resolves for any upstream status; check
/// [`Self::error_for_status`] before treating the blob as valid content.
#[derive(Clone, Debug)]
pub struct BlobRef {
    /// Runtime-local id of the cached blob.
    pub id: BlobId,
    /// Cached blob size in bytes.
    pub size: u64,
    /// Response `Content-Type`, when present.
    pub content_type: Option<String>,
    /// Response `ETag`, when present.
    pub etag: Option<String>,
    /// HTTP status returned by the upstream fetch.
    pub status: u16,
    /// Response headers preserved for provider inspection.
    pub response_headers: Vec<(String, String)>,
}

impl BlobRef {
    /// The blob of a successful status, and the error of any other: the
    /// body of an informational, redirect, or failing answer is no content.
    /// Mirrors the inline-HTTP `error_for_status` so blob fetches participate
    /// in the same error flow.
    pub fn error_for_status(self) -> Result<Self> {
        if (200..300).contains(&self.status) {
            Ok(self)
        } else {
            Err(ProviderError::from_http_status(self.status))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BlobId, BlobRef};
    use crate::error::ProviderErrorKind;

    #[test]
    fn a_blob_of_a_successful_status_alone_is_content() {
        let blob = |status| BlobRef {
            id: BlobId(1),
            size: 0,
            content_type: None,
            etag: None,
            status,
            response_headers: Vec::new(),
        };
        for status in [200, 206, 299] {
            assert!(blob(status).error_for_status().is_ok(), "{status}");
        }
        for status in [100, 199, 300, 302, 304, 399, 404, 500] {
            assert!(blob(status).error_for_status().is_err(), "{status}");
        }
        assert_eq!(
            blob(404).error_for_status().unwrap_err().kind(),
            ProviderErrorKind::NotFound
        );
    }
}

impl From<BlobFetched> for BlobRef {
    fn from(fetched: BlobFetched) -> Self {
        Self {
            id: BlobId(fetched.blob),
            size: fetched.size,
            content_type: fetched.content_type,
            etag: fetched.etag,
            status: fetched.status,
            response_headers: fetched
                .response_headers
                .into_iter()
                .map(|h| (h.name, h.value))
                .collect(),
        }
    }
}

pub(crate) fn blob_fetch_callout(
    method: String,
    url: String,
    headers: Vec<Header>,
    body: Option<Vec<u8>>,
    md5: Option<String>,
    version: Option<String>,
) -> Callout {
    Callout::FetchBlob(BlobFetchRequest {
        method,
        url,
        headers,
        body,
        md5,
        version,
    })
}

pub(crate) fn extract_blob(result: CalloutResult) -> Result<BlobRef> {
    crate::http::expect_callout(
        "fetch-blob",
        |r| match r {
            CalloutResult::BlobFetched(f) => Some(Ok(BlobRef::from(f))),
            _ => None,
        },
        result,
    )
}
