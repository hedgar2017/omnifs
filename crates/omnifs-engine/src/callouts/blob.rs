//! Provider-facing `fetch-blob` executor.
//!
//! Provider HTTP fetches whose payload should never cross the WIT
//! boundary are streamed into the body store of the host. The tree reads a
//! file of such a body in place, by ranges, so the bytes never round-trip
//! back through the provider. `docs/contracts/10-system.md` owns the
//! identity of a blob request and the answer that the host keeps for it.

use crate::cache::MountResources;
use crate::cache::body::{BodyStore, BodyStoreError};
use crate::cache::identity::BlobRequestId;
use crate::cache::mount::{BlobBody, BlobResponse};
use crate::callouts::{
    callout_error, callout_internal, callout_invalid, callout_network, callout_too_large,
    record_outcome,
};
use crate::http::{
    Answer, BlobPin, HopHeaders, HttpStack, ValidatedRequest, decode_response_headers, successful,
    wit_headers,
};
use crate::log_redaction::{LogUrl, WitHeaders};
use futures::StreamExt;
use md5::{Digest as _, Md5};
use omnifs_wit::host::types as wit_types;
use std::io::Write;
use std::sync::Arc;

/// The longest version of a blob request, in bytes.
const MAX_VERSION_BYTES: usize = 256;

/// Host-side size limits for blob fetches: the limit that the mount
/// declares bounds how much data the host stores on disk for one fetch, and
/// no size bounds the fetch of a mount without one.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BlobLimits {
    /// Maximum response-body bytes accepted by `fetch-blob`.
    pub max_fetch_blob_bytes: Option<u64>,
}

impl BlobLimits {
    pub fn from_max_fetch_bytes(max_fetch_blob_bytes: Option<u64>) -> Self {
        Self {
            max_fetch_blob_bytes,
        }
    }
}

/// Errors raised while fetching or storing host-resident blobs.
#[derive(Debug, thiserror::Error)]
pub(crate) enum BlobError {
    #[error("fetch blob exceeds the limit of the mount ({actual} > {max} bytes)")]
    TooLarge { max: u64, actual: u64 },
    #[error("network: {0}")]
    Network(String),
    /// The upstream answered with other bytes than the provider asked for,
    /// as of a file that changed after the provider read its MD5.
    #[error("the upstream answered bytes of the MD5 {actual}, not {expected}")]
    Content { expected: String, actual: String },
    #[error("{0}")]
    Internal(String),
}

impl From<std::io::Error> for BlobError {
    fn from(error: std::io::Error) -> Self {
        Self::Internal(format!("fetch blob: {error}"))
    }
}

impl From<BodyStoreError> for BlobError {
    fn from(error: BodyStoreError) -> Self {
        Self::Internal(format!("body store publication failed: {error}"))
    }
}

impl From<crate::cache::ProjectionError> for BlobError {
    fn from(error: crate::cache::ProjectionError) -> Self {
        Self::Internal(error.to_string())
    }
}

impl From<BlobError> for wit_types::CalloutResult {
    fn from(error: BlobError) -> Self {
        match error {
            BlobError::TooLarge { .. } => callout_too_large(error.to_string()),
            BlobError::Network(_) => callout_network(error.to_string()),
            // The upstream has another version than the provider asked for:
            // the same request has the same answer, so it is not retryable.
            BlobError::Content { .. } => callout_error(
                wit_types::ErrorKind::VersionMismatch,
                error.to_string(),
                false,
            ),
            BlobError::Internal(msg) => callout_internal(msg),
        }
    }
}

/// Executes provider blob callouts against the body store of the host.
#[derive(Clone)]
pub struct BlobExecutor {
    http: Arc<HttpStack>,
    resources: Arc<MountResources>,
    limits: BlobLimits,
}

/// The answer of an upstream to a blob request as a fetch takes it: its
/// response, the bytes that it declares, and the stream of its body.
pub(crate) struct Upstream<S> {
    pub response: BlobResponse,
    pub length: Option<u64>,
    pub body: S,
}

/// A blob request after its validation: its HTTP request, the MD5 of the
/// bytes that it asks for, its identity, and whether it names the identity
/// of its bytes.
pub struct BlobRequest {
    http: ValidatedRequest,
    md5: Option<String>,
    version: Option<String>,
    id: BlobRequestId,
    pinned: bool,
}

impl BlobRequest {
    pub(crate) fn id(&self) -> BlobRequestId {
        self.id
    }

    /// Whether the host may ask the upstream whether the answer that it
    /// keeps for this request still stands: for a `GET` whose provider
    /// wrote no condition and no range of its own.
    fn revalidates(&self) -> bool {
        !self.pinned
            && self.http.is_get()
            && ![
                "if-none-match",
                "if-modified-since",
                "if-match",
                "if-unmodified-since",
                "if-range",
                "range",
            ]
            .iter()
            .any(|name| self.http.has_header(name))
    }
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

    /// Fetch the answer of an HTTP request, the body of a successful one
    /// into the body store.
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
        let result = match self.prepare(req) {
            Ok(request) => {
                let lock = self.resources.blob_request_lock(request.id);
                let guard = lock.lock().await;
                let result = self.answer(&request, operation_id).await;
                drop(guard);
                self.resources.release_blob_request_lock(request.id, lock);
                result.unwrap_or_else(|early| early)
            },
            Err(early) => early,
        };
        record_outcome(&result);
        result
    }

    /// Validate the facts of `req`, from which alone the host derives the
    /// identity of the request.
    pub(crate) fn prepare(
        &self,
        req: &wit_types::BlobFetchRequest,
    ) -> Result<BlobRequest, wit_types::CalloutResult> {
        let http = self.http.validate(
            &req.method,
            &req.url,
            &req.headers,
            req.body.as_deref(),
            req.follow_redirects,
        )?;
        let pin = BlobPin {
            md5: req
                .md5
                .as_deref()
                .map(valid_md5)
                .transpose()
                .map_err(callout_invalid)?,
            version: req
                .version
                .as_deref()
                .map(valid_version)
                .transpose()
                .map_err(callout_invalid)?,
        };
        let id = http.blob_request_id(self.http.auth_binding_for_url(http.original_url()), pin);
        Ok(BlobRequest {
            http,
            md5: pin.md5.map(ToOwned::to_owned),
            version: pin.version.map(ToOwned::to_owned),
            id,
            pinned: pin.md5.is_some() || pin.version.is_some(),
        })
    }

    /// Answer `request` from the projection, or ask its upstream.
    async fn answer(
        &self,
        request: &BlobRequest,
        operation_id: u64,
    ) -> Result<wit_types::CalloutResult, wit_types::CalloutResult> {
        let resources = Arc::clone(&self.resources);
        let id = request.id;
        let kept = blocking(move || Ok(resources.kept_blob(id)?)).await?;
        let (kept, validators) = match kept {
            // The request with the identity of its bytes has one answer.
            Some((response, body)) if request.pinned => {
                return Ok(self.fetched(operation_id, response, Some(body), false));
            },
            Some((response, body)) if request.revalidates() => {
                let validators = validators(&response, request.http.original_url());
                (Some((response, body)), validators)
            },
            _ => (None, None),
        };
        let Answer {
            response,
            redirected,
        } = self
            .http
            .send_validated(&request.http, validators.as_ref())
            .await?;
        let upstream = Upstream {
            response: BlobResponse {
                status: response.status().as_u16(),
                headers: decode_response_headers(response.headers()),
                redirected,
            },
            length: response.content_length(),
            body: response
                .bytes_stream()
                .map(|chunk| chunk.map_err(|error| error.to_string())),
        };
        Ok(self.receive(operation_id, request, kept, upstream).await?)
    }

    /// Take the answer of the upstream to `request`: the answer that the
    /// host keeps for a `304` to its validators, no body for another status
    /// than a successful one, and the body of a successful one into the
    /// body store, of the MD5 of the request alone.
    pub(crate) async fn receive<B: AsRef<[u8]> + Send + 'static>(
        &self,
        operation_id: u64,
        request: &BlobRequest,
        kept: Option<(BlobResponse, BlobBody)>,
        upstream: Upstream<impl futures::Stream<Item = Result<B, String>> + Unpin>,
    ) -> Result<wit_types::CalloutResult, BlobError> {
        let Upstream {
            response: answered,
            length,
            body,
        } = upstream;
        if let Some((kept, body)) = kept
            && answered.status == 304
        {
            return Ok(self.fetched(operation_id, kept, Some(body), true));
        }
        if !successful(answered.status) {
            return Ok(self.fetched(operation_id, answered, None, true));
        }
        let keeps = request.pinned
            || (request.revalidates()
                && answered.status == 200
                && validators(&answered, request.http.original_url()).is_some());
        let Streamed { body, md5 } = stream_response_body(
            body,
            length,
            Arc::clone(&self.resources.body),
            self.limits.max_fetch_blob_bytes,
            request.md5.is_some(),
        )
        .await?;
        // Bytes of another MD5 than the request asks for are the answer of
        // the request for that MD5: the host keeps them under it, so that
        // the next fetch of those bytes needs no download, and the request
        // has the error of the mismatch.
        if let (Some(expected), Some(actual)) = (&request.md5, md5)
            && *expected != actual
        {
            let id = request.http.blob_request_id(
                self.http.auth_binding_for_url(request.http.original_url()),
                BlobPin {
                    md5: Some(&actual),
                    version: request.version.as_deref(),
                },
            );
            let resources = Arc::clone(&self.resources);
            let answered = answered.clone();
            blocking(move || Ok(resources.keep_blob(id, answered, &body)?)).await?;
            return Err(BlobError::Content {
                expected: expected.clone(),
                actual,
            });
        }
        let body = if keeps {
            let resources = Arc::clone(&self.resources);
            let (id, answered) = (request.id, answered.clone());
            blocking(move || {
                resources.keep_blob(id, answered, &body)?;
                Ok(body)
            })
            .await?
        } else {
            body
        };
        Ok(self.fetched(operation_id, answered, Some(body), true))
    }

    /// The answer of a blob fetch for the provider, with a blob handle of
    /// the operation for its body.
    fn fetched(
        &self,
        operation_id: u64,
        response: BlobResponse,
        body: Option<BlobBody>,
        upstream: bool,
    ) -> wit_types::CalloutResult {
        wit_types::CalloutResult::BlobFetched(wit_types::BlobFetched {
            status: response.status,
            response_headers: wit_headers(response.headers),
            redirected: response.redirected,
            upstream,
            content: body.map(|body| wit_types::BlobContent {
                size: body.length,
                blob: self.resources.blob_handle(operation_id, body),
            }),
        })
    }
}

/// Run the disk work `work` of a fetch off the thread of the provider
/// instance, which runs every operation of the instance.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, BlobError> + Send + 'static,
) -> Result<T, BlobError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| BlobError::Internal(format!("blob task failed: {error}")))?
}

/// The validators of `response`, the answer that the host keeps of a
/// request to `url`, as the conditions of the next request to the URL that
/// answered it; none for an answer without an `ETag` and a `Last-Modified`.
fn validators(response: &BlobResponse, url: &str) -> Option<HopHeaders> {
    let headers = [
        ("etag", "if-none-match"),
        ("last-modified", "if-modified-since"),
    ]
    .into_iter()
    .filter_map(|(validator, condition)| {
        let (_, value) = response
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(validator))?;
        Some((condition.to_owned(), value.clone()))
    })
    .collect::<Vec<_>>();
    (!headers.is_empty()).then(|| HopHeaders {
        url: response
            .redirected
            .clone()
            .unwrap_or_else(|| url.to_owned()),
        headers,
    })
}

/// A part of the body of an answer for the task that writes it.
enum Part<B> {
    Bytes(B),
    /// The body ended: a channel that closes without it is of a fetch that
    /// stopped, whose bytes are no body.
    End,
}

/// A body that a fetch published, with its MD5 when the fetch asked for it.
struct Streamed {
    body: BlobBody,
    md5: Option<String>,
}

/// Publish the body `stream`, which declares `declared` bytes, in `store`,
/// of at most the `limit` bytes of a mount with one, with its MD5 for
/// `md5`. The writes and the digests run on a blocking task. No timer
/// bounds the wait for the next bytes: the connection of the upstream ends
/// it, by the end of the body, its close, or its reset.
async fn stream_response_body<B: AsRef<[u8]> + Send + 'static>(
    mut stream: impl futures::Stream<Item = Result<B, String>> + Unpin,
    declared: Option<u64>,
    store: Arc<BodyStore>,
    limit: Option<u64>,
    md5: bool,
) -> Result<Streamed, BlobError> {
    if let (Some(max), Some(actual)) = (limit, declared)
        && actual > max
    {
        return Err(BlobError::TooLarge { max, actual });
    }
    let (parts, mut taken) = tokio::sync::mpsc::channel::<Part<B>>(1);
    let writer = tokio::task::spawn_blocking(move || {
        let mut writer = store.stage()?;
        let mut digest = md5.then(Md5::new);
        let mut length = 0_u64;
        loop {
            match taken.blocking_recv() {
                Some(Part::Bytes(chunk)) => {
                    length = length
                        .checked_add(chunk.as_ref().len() as u64)
                        .ok_or_else(|| BlobError::Internal("body length overflow".into()))?;
                    if let Some(max) = limit.filter(|max| length > *max) {
                        return Err(BlobError::TooLarge {
                            max,
                            actual: length,
                        });
                    }
                    if let Some(digest) = &mut digest {
                        digest.update(chunk.as_ref());
                    }
                    writer.write_all(chunk.as_ref())?;
                },
                Some(Part::End) => break,
                None => return Err(BlobError::Network("the fetch stopped".into())),
            }
        }
        Ok(Streamed {
            body: BlobBody {
                length,
                hold: store.publish_staged(writer)?,
            },
            md5: digest.map(|digest| hex::encode(digest.finalize())),
        })
    });
    let read = async {
        loop {
            let part = match stream.next().await {
                Some(Err(error)) => return Err(BlobError::Network(error)),
                Some(Ok(chunk)) => Part::Bytes(chunk),
                None => Part::End,
            };
            let end = matches!(part, Part::End);
            // A writer that is gone has the error of the fetch.
            if parts.send(part).await.is_err() || end {
                return Ok(());
            }
        }
    }
    .await;
    drop(parts);
    let written = writer
        .await
        .map_err(|error| BlobError::Internal(format!("blob task failed: {error}")))?;
    read.and(written)
}

/// `md5` as the MD5 of the bytes of a blob request: 32 lowercase
/// hexadecimal digits, as the host compares it to the digest of the body.
fn valid_md5(md5: &str) -> Result<&str, String> {
    if md5.len() == 32
        && md5
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        Ok(md5)
    } else {
        Err(format!(
            "the MD5 of a blob request is 32 lowercase hexadecimal digits, not {md5:?}"
        ))
    }
}

/// `version` as the version of the bytes of a blob request: 1 to
/// [`MAX_VERSION_BYTES`] bytes without a control character.
fn valid_version(version: &str) -> Result<&str, String> {
    if !version.is_empty()
        && version.len() <= MAX_VERSION_BYTES
        && !version.chars().any(char::is_control)
    {
        Ok(version)
    } else {
        Err(format!(
            "the version of a blob request is 1 to {MAX_VERSION_BYTES} bytes without a control character, not {version:?}"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(
        chunks: &[&'static [u8]],
    ) -> impl futures::Stream<Item = Result<&'static [u8], String>> + Unpin + use<> {
        futures::stream::iter(chunks.iter().map(|chunk| Ok(*chunk)).collect::<Vec<_>>())
    }

    fn files(dir: &std::path::Path) -> usize {
        std::fs::read_dir(dir).unwrap().count()
    }

    /// The limit that a mount declares bounds a body by the bytes that it
    /// declares and by the bytes that come, and a body above it leaves
    /// nothing in the store. No size bounds the body of a mount without a
    /// limit.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_body_above_the_limit_of_its_mount_is_refused_and_leaves_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(BodyStore::open(tmp.path()).unwrap());
        let stream = |declared, limit| {
            stream_response_body(
                body(&[b"abc", b"de"]),
                declared,
                Arc::clone(&store),
                limit,
                false,
            )
        };
        assert!(matches!(
            stream(Some(5), Some(4)).await,
            Err(BlobError::TooLarge { max: 4, actual: 5 })
        ));
        assert!(matches!(
            stream(None, Some(4)).await,
            Err(BlobError::TooLarge { max: 4, actual: 5 })
        ));
        assert_eq!(files(tmp.path()), 0);

        assert_eq!(BlobLimits::default().max_fetch_blob_bytes, None);
        let whole = stream(None, None)
            .await
            .expect("a body without a limit")
            .body;
        assert_eq!(whole.length, 5);
        assert_eq!(
            store.read_range(whole.hold.id(), 5, 0, 5).unwrap(),
            b"abcde"
        );
        assert_eq!(stream(Some(5), Some(5)).await.unwrap().body.length, 5);
    }

    /// The bytes of a body that fails are no body of the store, and a body
    /// that the fetch asks the MD5 of has it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_body_that_fails_leaves_nothing_and_a_body_has_its_md5() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(BodyStore::open(tmp.path()).unwrap());
        let failing = futures::stream::iter([Ok(&b"abc"[..]), Err("reset".to_string())]);
        let failed = stream_response_body(failing, None, Arc::clone(&store), None, false).await;
        assert!(matches!(failed, Err(BlobError::Network(message)) if message == "reset"));
        assert_eq!(files(tmp.path()), 0);
        let same = stream_response_body(body(&[b"a", b"bc"]), None, Arc::clone(&store), None, true)
            .await
            .expect("the bytes of a body");
        assert_eq!(same.body.length, 3);
        assert_eq!(
            same.md5.as_deref(),
            Some("900150983cd24fb0d6963f7d28e17f72")
        );
        assert_eq!(files(tmp.path()), 1);
    }
}
