//! Provider-facing `fetch-blob` executor.
//!
//! Provider HTTP fetches whose payload should never cross the WIT
//! boundary are streamed into the mount-owned blob cache.
//! Other host-side machinery consumes the file in place, so the bytes never
//! round-trip back through the provider.

use crate::cache::MountResources;
use crate::cache::body::{BodyStore, BodyStoreError, BodyWriter};
use crate::cache::identity::BlobRequestId;
use crate::cache::mount::{BlobMetadata, BlobRecord};
use crate::callouts::{callout_internal, callout_network, callout_too_large, record_outcome};
use crate::http::{HttpStack, decode_response_headers};
use crate::log_redaction::{LogUrl, WitHeaders};
use futures::StreamExt;
use md5::{Digest as _, Md5};
use omnifs_wit::host::types as wit_types;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

/// The longest wait for the status of the answer of a blob fetch, and for
/// the next bytes of its body: no time bounds the whole of a body, which no
/// size bounds beside the limit of its mount.
const BLOB_IDLE_TIMEOUT: Duration = Duration::from_mins(2);

const DEFAULT_MAX_FETCH_BLOB_BYTES: u64 = 1024 * 1024 * 1024;
/// Host-side size limits for blob fetches.
///
/// Fetch limits bound how much data the host stores on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlobLimits {
    /// Maximum response-body bytes accepted by `fetch-blob`.
    pub max_fetch_blob_bytes: u64,
}

impl Default for BlobLimits {
    fn default() -> Self {
        Self {
            max_fetch_blob_bytes: DEFAULT_MAX_FETCH_BLOB_BYTES,
        }
    }
}

impl BlobLimits {
    pub fn from_max_fetch_bytes(max_fetch_blob_bytes: Option<u64>) -> Self {
        let defaults = Self::default();
        Self {
            max_fetch_blob_bytes: max_fetch_blob_bytes.unwrap_or(defaults.max_fetch_blob_bytes),
        }
    }
}

/// Errors raised while fetching or storing host-resident blobs.
#[derive(Debug, thiserror::Error)]
enum BlobError {
    #[error("{operation} exceeds host blob cap ({actual} > {max} bytes)")]
    TooLarge {
        operation: &'static str,
        max: u64,
        actual: u64,
    },
    #[error("network: {0}")]
    Network(String),
    /// The upstream answered with other bytes than the provider asked for,
    /// as of a file that changed after the provider read its MD5.
    #[error("the upstream answered bytes of the MD5 {actual}, not {expected}")]
    Content { expected: String, actual: String },
    #[error("I/O error")]
    Io(#[source] std::io::Error),
    #[error("{0}")]
    Internal(String),
}

impl From<std::io::Error> for BlobError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl BlobError {
    /// Add operation context to an I/O error without relabeling typed failures.
    fn with_io_context(self, context: &'static str) -> Self {
        match self {
            Self::Io(error) => Self::Internal(format!("{context}: {error}")),
            other => other,
        }
    }
}

impl From<BodyStoreError> for BlobError {
    fn from(error: BodyStoreError) -> Self {
        Self::Internal(format!("body store publication failed: {error}"))
    }
}

impl From<BlobError> for wit_types::CalloutResult {
    fn from(error: BlobError) -> Self {
        match error {
            BlobError::TooLarge { .. } => callout_too_large(error.to_string()),
            BlobError::Network(_) | BlobError::Content { .. } => callout_network(error.to_string()),
            BlobError::Io(_) => callout_internal(error.to_string()),
            BlobError::Internal(msg) => callout_internal(msg),
        }
    }
}

/// Executes provider blob callouts against a host-owned disk cache.
#[derive(Clone)]
pub struct BlobExecutor {
    http: Arc<HttpStack>,
    resources: Arc<MountResources>,
    limits: BlobLimits,
}

impl BlobExecutor {
    /// Construct an executor with explicit host blob limits.
    pub fn new(http: Arc<HttpStack>, resources: Arc<MountResources>, limits: BlobLimits) -> Self {
        Self {
            http,
            resources,
            limits,
        }
    }

    /// Fetch an HTTP response body into the blob cache.
    ///
    /// The body is streamed to a temporary file and published by the cache
    /// only after the response has stayed within the configured fetch cap.
    #[tracing::instrument(target = "omnifs_callout", skip_all, fields(
        method = req.method.as_str(),
        url = %LogUrl(&req.url),
        request_headers = %WitHeaders(&req.headers),
        request_body_bytes = req.body.as_ref().map_or(0, Vec::len),
        blob = tracing::field::Empty,
        status = tracing::field::Empty,
        response_headers = tracing::field::Empty,
        response_body_bytes = tracing::field::Empty,
        error.kind = tracing::field::Empty,
        error.message = tracing::field::Empty,
        error.retryable = tracing::field::Empty,
    ))]
    pub async fn fetch(
        &self,
        req: &wit_types::BlobFetchRequest,
        operation_id: u64,
    ) -> wit_types::CalloutResult {
        let result =
            match self
                .http
                .validate(&req.method, &req.url, &req.headers, req.body.as_deref())
            {
                Err(early) => early,
                Ok(request) => {
                    let request_id = request.blob_request_id(
                        self.http.auth_binding_for_url(request.original_url()),
                        req.md5.as_deref(),
                        req.version.as_deref(),
                    );
                    let lock = self.resources.blob_request_lock(request_id);
                    let _guard = lock.lock().await;
                    match self.resources.blob_for_request(request_id) {
                        Ok(Some(record)) => {
                            let record = self.resources.stage_blob_write(
                                operation_id,
                                request_id,
                                record.body,
                                record.metadata(),
                            );
                            wit_types::CalloutResult::BlobFetched(record.as_ref().into())
                        },
                        Ok(None) => {
                            let sent = tokio::time::timeout(
                                BLOB_IDLE_TIMEOUT,
                                self.http.send_validated(&request, None),
                            )
                            .await
                            .unwrap_or_else(|_| Err(idle().into()));
                            match sent {
                                Ok(response) => {
                                    match self
                                        .materialize(
                                            operation_id,
                                            request_id,
                                            response,
                                            req.md5.as_deref(),
                                        )
                                        .await
                                    {
                                        Ok(record) => {
                                            wit_types::CalloutResult::BlobFetched((&record).into())
                                        },
                                        Err(error) => error.into(),
                                    }
                                },
                                Err(early) => early,
                            }
                        },
                        Err(error) => BlobError::Internal(error.to_string()).into(),
                    }
                },
            };
        record_outcome(&result);
        result
    }

    /// Stream the response body to disk and persist the cache record.
    /// Internal helper that keeps `BlobError` typed up to the public
    /// `fetch` boundary so the `?` operator and `From` impls stay in
    /// play here.
    async fn materialize(
        &self,
        operation_id: u64,
        request_id: BlobRequestId,
        response: reqwest::Response,
        md5: Option<&str>,
    ) -> Result<BlobRecord, BlobError> {
        let status = response.status().as_u16();
        let response_headers = decode_response_headers(response.headers());
        let etag = lookup_header(&response_headers, "etag");
        let content_type = lookup_header(&response_headers, "content-type");

        // The body of a failing status is of no content.
        let staged = stream_response_body(
            response,
            &self.resources.body,
            self.limits.max_fetch_blob_bytes,
            md5.filter(|_| (200..300).contains(&status)),
        )
        .await
        .map_err(|error| error.with_io_context("fetch blob"))?;
        let size = staged.size;
        let body = self
            .resources
            .body
            .publish_staged(staged.writer)
            .map_err(|error| BlobError::from(error).with_io_context("publish body"))?;

        let metadata = BlobMetadata {
            status,
            content_type,
            etag,
            response_headers,
            size,
        };
        Ok((*self
            .resources
            .stage_blob_write(operation_id, request_id, body, metadata))
        .clone())
    }
}

impl From<&BlobRecord> for wit_types::BlobFetched {
    fn from(record: &BlobRecord) -> Self {
        Self {
            blob: record.id,
            size: record.size,
            content_type: record.content_type.clone(),
            etag: record.etag.clone(),
            status: record.status,
            response_headers: record
                .response_headers
                .iter()
                .map(|(name, value)| wit_types::Header {
                    name: name.clone(),
                    value: value.clone(),
                })
                .collect(),
        }
    }
}

struct StagedBlob {
    writer: BodyWriter,
    size: u64,
}

/// Stage the body of `response`, of at most `max_bytes` and, for `md5`, of
/// bytes of that MD5 alone.
async fn stream_response_body(
    response: reqwest::Response,
    body: &BodyStore,
    max_bytes: u64,
    md5: Option<&str>,
) -> Result<StagedBlob, BlobError> {
    if let Some(content_length) = response.content_length()
        && content_length > max_bytes
    {
        return Err(BlobError::TooLarge {
            operation: "fetch blob",
            max: max_bytes,
            actual: content_length,
        });
    }
    let mut writer = body.stage().map_err(BlobError::from)?;
    let mut stream = response.bytes_stream();
    let mut total = 0_u64;
    let mut digest = md5.map(|_| Md5::new());
    while let Some(chunk) = tokio::time::timeout(BLOB_IDLE_TIMEOUT, stream.next())
        .await
        .map_err(|_| idle())?
    {
        let chunk = chunk.map_err(|e| BlobError::Network(e.to_string()))?;
        let chunk_len = u64::try_from(chunk.len()).map_err(|_| BlobError::TooLarge {
            operation: "fetch blob",
            max: max_bytes,
            actual: u64::MAX,
        })?;
        total = total.checked_add(chunk_len).ok_or(BlobError::TooLarge {
            operation: "fetch blob",
            max: max_bytes,
            actual: u64::MAX,
        })?;
        if total > max_bytes {
            return Err(BlobError::TooLarge {
                operation: "fetch blob",
                max: max_bytes,
                actual: total,
            });
        }
        if let Some(digest) = &mut digest {
            digest.update(&chunk);
        }
        writer.write_all(&chunk).map_err(BlobError::Io)?;
    }
    if let (Some(expected), Some(digest)) = (md5, digest) {
        let actual = hex::encode(digest.finalize());
        if actual != expected {
            return Err(BlobError::Content {
                expected: expected.to_owned(),
                actual,
            });
        }
    }
    Ok(StagedBlob {
        writer,
        size: total,
    })
}

/// The error of a blob fetch whose upstream sent nothing for
/// [`BLOB_IDLE_TIMEOUT`].
fn idle() -> BlobError {
    BlobError::Network(format!(
        "the upstream sent nothing for {} seconds",
        BLOB_IDLE_TIMEOUT.as_secs()
    ))
}

fn lookup_header(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::body::BodyStore;
    #[tokio::test]
    async fn stream_response_body_rejects_large_content_length_before_writing() {
        let tmp = tempfile::tempdir().unwrap();
        let body = BodyStore::open(tmp.path()).unwrap();
        let response: reqwest::Response = http::Response::builder()
            .header(http::header::CONTENT_LENGTH, "5")
            .body(reqwest::Body::from("abcde"))
            .unwrap()
            .into();

        let result = stream_response_body(response, &body, 4, None).await;

        assert!(matches!(result, Err(BlobError::TooLarge { actual: 5, .. })));
    }

    #[tokio::test]
    async fn stream_response_body_rejects_body_that_exceeds_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let body = BodyStore::open(tmp.path()).unwrap();
        let chunks = futures::stream::iter([
            Ok::<_, std::io::Error>(b"abc".to_vec()),
            Ok::<_, std::io::Error>(b"de".to_vec()),
        ]);
        let response: reqwest::Response = http::Response::builder()
            .body(reqwest::Body::wrap_stream(chunks))
            .unwrap()
            .into();

        let result = stream_response_body(response, &body, 4, None).await;

        assert!(matches!(result, Err(BlobError::TooLarge { actual: 5, .. })));
    }

    #[tokio::test]
    async fn stream_response_body_stages_until_explicit_publish() {
        let tmp = tempfile::tempdir().unwrap();
        let body = BodyStore::open(tmp.path()).unwrap();
        let response: reqwest::Response = http::Response::builder()
            .body(reqwest::Body::from("abcde"))
            .unwrap()
            .into();

        let staged = stream_response_body(response, &body, 5, None)
            .await
            .expect("stage body");

        assert_eq!(staged.size, 5);
        let id = body.publish_staged(staged.writer).unwrap();
        assert_eq!(body.read(id, None).unwrap(), b"abcde");
    }
}
