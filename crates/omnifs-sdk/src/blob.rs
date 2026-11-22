//! Typed async blob callout builders.
//!
//! Blobs keep large bodies host-side by design.
//! `cx.http().get(url).into_blob().send()` (or the typed
//! [`crate::endpoint::RequestBuilder::into_blob`]) lands the body of a
//! successful answer in the body store of the host and returns a
//! [`BlobAnswer`] without it, whose [`BlobAnswer::error_for_status`] gives
//! the [`BlobRef`] of the blob. The host derives the identity of the request
//! from its validated facts, so providers never name cache entries.
//!
//! A stored blob is consumed by:
//! - [`crate::projection::FileProjection::blob`]: serve the bytes verbatim
//!   from a file route; the host reads them without guest involvement.

use crate::error::{ProviderError, Result};
use omnifs_wit::provider::types::BlobFetched;

/// Handle of a blob in the body store of the host. Valid only for the
/// operation that fetched it: hand it to a file of that operation, and do
/// not persist it or derive paths from it. The host, not this id, finds the
/// answer that it keeps for a blob request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlobId(pub(crate) u64);

impl BlobId {
    /// Return the raw WIT `blob-id` value.
    pub fn raw(self) -> u64 {
        self.0
    }
}

/// The answer of a blob fetch, of any status, without its body: the host
/// stores the body of a successful answer alone.
/// [`Self::error_for_status`] gives the blob of a successful answer.
#[derive(Clone, Debug)]
pub struct BlobAnswer {
    /// HTTP status returned by the upstream fetch.
    pub status: u16,
    /// Response headers preserved for provider inspection.
    pub response_headers: Vec<(String, String)>,
    answered: crate::http::AnsweredBy,
    upstream: bool,
    content: Option<(BlobId, u64)>,
}

impl BlobAnswer {
    /// The answer of the host to a blob fetch of `url`.
    pub(crate) fn new(url: String, fetched: BlobFetched) -> Self {
        Self {
            status: fetched.status,
            response_headers: fetched
                .response_headers
                .into_iter()
                .map(|header| (header.name, header.value))
                .collect(),
            answered: crate::http::AnsweredBy::new(url, fetched.redirected),
            upstream: fetched.upstream,
            content: fetched
                .content
                .map(|content| (BlobId(content.blob), content.size)),
        }
    }

    /// The URL that answered: the one of the request, or the last one of
    /// the redirects that the host followed for it.
    pub fn url(&self) -> &str {
        &self.answered.url
    }

    /// Whether another URL than the one of the request answered.
    pub fn redirected(&self) -> bool {
        self.answered.redirected
    }

    /// Whether the upstream answered this fetch: the host answers a request
    /// that it keeps the answer of from its projection, which tells nothing
    /// of the upstream now.
    pub fn upstream(&self) -> bool {
        self.upstream
    }

    /// The header `name` of the answer.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.response_headers
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// The answer as a response without a body, for the policy of an
    /// endpoint.
    pub(crate) fn response(&self) -> Result<http::Response<Vec<u8>>> {
        let mut response = http::Response::builder().status(self.status);
        for (name, value) in &self.response_headers {
            response = response.header(name, value);
        }
        response
            .extension(self.answered.clone())
            .body(Vec::new())
            .map_err(|error| ProviderError::internal(error.to_string()))
    }

    /// The blob of a successful status, and the error of any other, as
    /// [`crate::http::ResponseExt::error_for_status`] gives it for an
    /// inline answer ([`ProviderError::from_http_answer`]).
    pub fn error_for_status(self) -> Result<BlobRef> {
        let Some((id, size)) = self.content else {
            return Err(ProviderError::from_http_answer(
                self.status,
                self.response_headers
                    .iter()
                    .map(|(name, value)| (name.as_str(), value.as_str())),
            ));
        };
        Ok(BlobRef {
            id,
            size,
            content_type: self.header("content-type").map(ToOwned::to_owned),
            etag: self.header("etag").map(ToOwned::to_owned),
            answer: self,
        })
    }
}

/// The blob of a successful answer of a blob fetch: its body stays in the
/// body store of the host. Use [`Self::id`] when passing the blob to
/// [`crate::projection::FileProjection::blob`].
#[derive(Clone, Debug)]
pub struct BlobRef {
    /// Handle of the blob, of the operation that fetched it.
    pub id: BlobId,
    /// Blob size in bytes.
    pub size: u64,
    /// Response `Content-Type`, when present.
    pub content_type: Option<String>,
    /// Response `ETag`, when present.
    pub etag: Option<String>,
    /// The answer of the blob.
    pub answer: BlobAnswer,
}

#[cfg(test)]
mod tests {
    use super::BlobAnswer;
    use crate::error::ProviderErrorKind;
    use core::time::Duration;
    use omnifs_wit::provider::types::{BlobContent, BlobFetched, Header};

    fn answer(status: u16, headers: &[(&str, &str)], redirected: Option<&str>) -> BlobAnswer {
        BlobAnswer::new(
            "https://files.test/a".into(),
            BlobFetched {
                status,
                response_headers: headers
                    .iter()
                    .map(|(name, value)| Header {
                        name: (*name).into(),
                        value: (*value).into(),
                    })
                    .collect(),
                redirected: redirected.map(Into::into),
                upstream: true,
                content: crate::error::is_success(status)
                    .then_some(BlobContent { blob: 1, size: 4 }),
            },
        )
    }

    #[test]
    fn a_blob_of_a_successful_status_alone_is_content() {
        for status in [200, 206, 299] {
            let blob = answer(status, &[("ETag", "\"v\"")], None)
                .error_for_status()
                .unwrap();
            assert_eq!(
                (blob.size, blob.etag.as_deref()),
                (4, Some("\"v\"")),
                "{status}"
            );
        }
        for status in [100, 199, 300, 302, 304, 399, 404, 500] {
            assert!(
                answer(status, &[], None).error_for_status().is_err(),
                "{status}"
            );
        }
        assert_eq!(
            answer(404, &[], None)
                .error_for_status()
                .unwrap_err()
                .kind(),
            ProviderErrorKind::NotFound
        );
    }

    /// The error of the status of a blob has the `Retry-After` of its
    /// answer, as the one of an inline answer.
    #[test]
    fn the_error_of_a_blob_has_the_retry_after_of_its_answer() {
        let error = answer(429, &[("Retry-After", "17")], None)
            .error_for_status()
            .unwrap_err();
        assert_eq!(error.kind(), ProviderErrorKind::RateLimited);
        assert_eq!(error.retry_after(), Some(Duration::from_secs(17)));
        let error = answer(
            503,
            &[
                ("retry-after", "Sun, 06 Nov 1994 08:50:37 GMT"),
                ("date", "Sun, 06 Nov 1994 08:49:37 GMT"),
            ],
            None,
        )
        .error_for_status()
        .unwrap_err();
        assert_eq!(error.kind(), ProviderErrorKind::Network);
        assert_eq!(error.retry_after(), Some(Duration::from_secs(60)));
    }

    #[test]
    fn a_blob_names_the_url_that_answered() {
        let asked = answer(200, &[], None);
        assert_eq!(
            (asked.url(), asked.redirected()),
            ("https://files.test/a", false)
        );
        let other = answer(200, &[], Some("https://cdn.test/b"));
        assert_eq!(
            (other.url(), other.redirected()),
            ("https://cdn.test/b", true)
        );
    }
}
