//! Shared HTTP transport for provider callouts.
//!
//! `HttpStack` owns the reqwest client, auth resolver, and resolved authority
//! checker. It is the single place where method parsing, header
//! construction, body handing, and network error mapping happen for
//! provider HTTP traffic. The HTTP and blob callout paths both build on
//! this so their auth and authority semantics cannot drift.
//!
//! Encapsulation contract: `reqwest::Client`, `reqwest::Method`,
//! `reqwest::header::HeaderMap`, and `reqwest::RequestBuilder` stay
//! hidden inside this module. `reqwest::Response` intentionally crosses
//! the boundary so the blob executor can stream the body to disk
//! without re-buffering. That is the only reqwest type any caller ever
//! sees.

use crate::authority::{ApprovedHttpTransport, RuntimeAuthority};
use crate::cache::identity::BlobRequestId;
use crate::callouts::{
    callout_denied, callout_internal, callout_invalid, callout_network, record_outcome,
};
use crate::log_redaction::{LogUrl, WitHeaders};
use dashmap::DashMap;
use omnifs_auth::{AuthBinding, RefreshOutcome};
use omnifs_wit::host::types as wit_types;
use reqwest::Url;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;

pub(crate) fn www_authenticate(headers: &reqwest::header::HeaderMap) -> Option<String> {
    let values = headers
        .get_all(reqwest::header::WWW_AUTHENTICATE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect::<Vec<_>>();
    (!values.is_empty()).then(|| values.join(", "))
}

pub struct HttpStack {
    https_client: reqwest::Client,
    unix_clients: DashMap<PathBuf, reqwest::Client>,
    auth: Option<Arc<AuthBinding>>,
    authority: Arc<RuntimeAuthority>,
}

/// The most redirects that the host follows for one request.
pub(crate) const MAX_REDIRECTS: usize = 10;

/// Whether `status` is of a successful answer, whose body is content: the
/// one owner of that in the host.
pub(crate) fn successful(status: u16) -> bool {
    (200..300).contains(&status)
}

/// The bound of the wait for an answer: for the whole of it, body included
/// and each redirect with it, or for the status of each of its hops alone,
/// where the caller bounds the wait for its body.
#[derive(Clone, Copy)]
pub(crate) enum Wait {
    Whole(Duration),
    Status(Duration),
}

/// Request after URL, method, authority, and provider-header validation.
/// Host-injected auth is added only when dispatching and is not identity data.
pub(crate) struct ValidatedRequest {
    method: reqwest::Method,
    transport: ApprovedHttpTransport,
    original_url: String,
    canonical_url: String,
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
    follow_redirects: bool,
}

/// The identity of the bytes that a blob request asks for.
#[derive(Clone, Copy)]
pub(crate) struct BlobPin<'a> {
    pub md5: Option<&'a str>,
    pub version: Option<&'a str>,
}

impl ValidatedRequest {
    pub(crate) fn blob_request_id(
        &self,
        auth: Option<&AuthBinding>,
        pin: BlobPin<'_>,
    ) -> BlobRequestId {
        BlobRequestId::new(
            auth.map(omnifs_auth::AuthBinding::credential_id),
            self.method.as_str(),
            &self.canonical_url,
            &self.headers,
            self.body.as_deref(),
            self.follow_redirects,
            pin.md5,
            pin.version,
        )
    }

    pub(crate) fn original_url(&self) -> &str {
        &self.original_url
    }

    pub(crate) fn is_get(&self) -> bool {
        self.method == reqwest::Method::GET
    }

    /// Whether the provider wrote the header `name` of the request.
    pub(crate) fn has_header(&self, name: &str) -> bool {
        self.headers.iter().any(|(header, _)| header == name)
    }
}

/// Headers of the host for the hop of a request to `url`: the validators of
/// the answer that the host keeps of that URL.
pub(crate) struct HopHeaders {
    pub url: String,
    pub headers: Vec<(String, String)>,
}

/// The answer of an upstream to a request: the response of the URL that
/// answered, which `redirected` names when it is another than the one of
/// the request.
pub(crate) struct Answer {
    pub response: reqwest::Response,
    pub redirected: Option<String>,
}

impl HttpStack {
    pub(crate) fn auth_binding_for_url(&self, url: &str) -> Option<&AuthBinding> {
        self.auth
            .as_deref()
            .filter(|binding| binding.applies_to_url(url))
    }

    pub fn new(
        auth: Option<Arc<AuthBinding>>,
        authority: Arc<RuntimeAuthority>,
    ) -> Result<Self, reqwest::Error> {
        let https_client = Self::client_builder().build()?;
        Ok(Self::with_https_client(auth, authority, https_client))
    }

    fn client_builder() -> reqwest::ClientBuilder {
        reqwest::Client::builder()
            .user_agent("omnifs")
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
    }

    #[doc(hidden)]
    pub fn with_https_client(
        auth: Option<Arc<AuthBinding>>,
        authority: Arc<RuntimeAuthority>,
        https_client: reqwest::Client,
    ) -> Self {
        Self {
            https_client,
            unix_clients: DashMap::new(),
            auth,
            authority,
        }
    }

    /// Validate `method`, `url`, and `headers` against the authority of the
    /// mount. A request that follows redirects is a `GET` or a `HEAD` over
    /// HTTPS: the host follows no other.
    pub(crate) fn validate(
        &self,
        method: &str,
        url: &str,
        headers: &[wit_types::Header],
        body: Option<&[u8]>,
        follow_redirects: bool,
    ) -> Result<ValidatedRequest, wit_types::CalloutResult> {
        let transport = self
            .authority
            .approve_http(url)
            .map_err(|error| callout_denied(error.to_string()))?;
        let method = reqwest::Method::from_str(method)
            .map_err(|_| callout_denied(format!("unsupported HTTP method: {method}")))?;
        if follow_redirects
            && !(matches!(transport, ApprovedHttpTransport::Https(_))
                && (method == reqwest::Method::GET || method == reqwest::Method::HEAD))
        {
            return Err(callout_invalid(format!(
                "the host follows the redirects of a GET or a HEAD over HTTPS alone, not of {method} {}",
                LogUrl(url)
            )));
        }
        let mut normalized = Vec::with_capacity(headers.len());
        for header in headers {
            let name = HeaderName::from_str(&header.name).map_err(|error| {
                callout_internal(format!(
                    "invalid request header name `{}`: {error}",
                    header.name
                ))
            })?;
            HeaderValue::from_str(&header.value).map_err(|error| {
                callout_internal(format!(
                    "invalid request header value for `{}`: {error}",
                    name.as_str()
                ))
            })?;
            normalized.push((name.as_str().to_string(), header.value.clone()));
        }
        normalized.sort_by(|left, right| left.0.cmp(&right.0));
        let canonical_url = transport.canonical_url().to_string();
        Ok(ValidatedRequest {
            method,
            transport,
            original_url: url.to_string(),
            canonical_url,
            headers: normalized,
            body: body.map(ToOwned::to_owned),
            follow_redirects,
        })
    }

    /// Dispatch `request` and return the answer of its upstream. The answer
    /// of a redirect status is the answer itself, unless the request follows
    /// redirects: then each `301`, `302`, `303`, `307`, and `308` with a
    /// `Location` is a hop to that URL, for at most [`MAX_REDIRECTS`] hops.
    /// A hop is to an HTTPS URL that the authority of the mount approves,
    /// with the credential that the mount declares for that URL alone, and
    /// with the headers and the body of the provider for a hop to the
    /// origin of the request alone. A hop that the host refuses, and one
    /// too many, is an error that is not retryable and names the host of
    /// the hop: never the redirect as the answer. `host` gives the headers
    /// of the host for the hop to its URL.
    pub(crate) async fn send_validated(
        &self,
        request: &ValidatedRequest,
        host: Option<&HopHeaders>,
        wait: Wait,
    ) -> Result<Answer, wit_types::CalloutResult> {
        let whole = match wait {
            Wait::Whole(timeout) => Some(tokio::time::Instant::now() + timeout),
            Wait::Status(_) => None,
        };
        let mut transport = request.transport.clone();
        let mut url = request.original_url.clone();
        let mut redirected = None;
        for hop in 0..=MAX_REDIRECTS {
            let own = hop == 0 || same_origin(&request.transport, &transport);
            let mut headers = if own {
                request.headers.clone()
            } else {
                Vec::new()
            };
            if let Some(host) = host.filter(|host| host.url == url) {
                headers.extend(host.headers.iter().cloned());
            }
            let wait = match (wait, whole) {
                (Wait::Whole(_), Some(deadline)) => Wait::Whole(
                    deadline
                        .checked_duration_since(tokio::time::Instant::now())
                        .filter(|left| !left.is_zero())
                        .ok_or_else(|| {
                            callout_network(format!(
                                "{} did not answer in time",
                                LogUrl(&request.original_url)
                            ))
                        })?,
                ),
                (wait, _) => wait,
            };
            let response = self
                .send_authorized(
                    &request.method,
                    &transport,
                    &url,
                    &headers,
                    request.body.as_deref().filter(|_| own),
                    wait,
                )
                .await?;
            let location = if request.follow_redirects {
                redirect_location(&response)
            } else {
                None
            };
            let Some(location) = location else {
                return Ok(Answer {
                    response,
                    redirected,
                });
            };
            let target = redirect_target(&transport, location).ok_or_else(|| {
                callout_denied(format!(
                    "{} redirects to a location that is no URL",
                    LogUrl(&url)
                ))
            })?;
            let target_host = target.host_str().unwrap_or_default().to_owned();
            if target.scheme() != "https" {
                return Err(callout_denied(format!(
                    "{} redirects to the host {target_host} over {}, and a redirect leaves no HTTPS",
                    LogUrl(&url),
                    target.scheme()
                )));
            }
            if hop == MAX_REDIRECTS {
                return Err(callout_denied(format!(
                    "{} redirects more than {MAX_REDIRECTS} times, the last to the host {target_host}",
                    LogUrl(&request.original_url)
                )));
            }
            transport = self.authority.approve_http(target.as_str()).map_err(|error| {
                callout_denied(format!(
                    "{} redirects to the host {target_host}, which the mount does not approve: {error}",
                    LogUrl(&url)
                ))
            })?;
            url = target.into();
            redirected = Some(url.clone());
        }
        unreachable!("the last hop returns its answer or its error")
    }

    /// Dispatch a request to `url` with its credential, once more after a
    /// refresh of a credential that the upstream rejected.
    async fn send_authorized(
        &self,
        method: &reqwest::Method,
        transport: &ApprovedHttpTransport,
        url: &str,
        headers: &[(String, String)],
        body: Option<&[u8]>,
        wait: Wait,
    ) -> Result<reqwest::Response, wit_types::CalloutResult> {
        let send = || self.send_once(method.clone(), transport, url, headers, body, wait);
        let response = send().await?;
        let rejection = match &self.auth {
            Some(auth) => {
                auth.report_rejected_for_response(
                    url,
                    response.status().as_u16(),
                    www_authenticate(response.headers()),
                )
                .await
            },
            None => RefreshOutcome::NotApplicable,
        };
        let (Some(auth), RefreshOutcome::Refreshed) = (&self.auth, &rejection) else {
            return match rejection {
                RefreshOutcome::RefreshFailed(error) => {
                    Err(callout_denied(format!("auth refresh failed: {error}")))
                },
                _ => Ok(response),
            };
        };
        // The answer to the refreshed credential is of the binding too: an
        // upstream that rejects it again is known at once, not at the next
        // request. It is the answer of the request, without another send.
        let response = send().await?;
        match auth
            .report_rejected_for_response(
                url,
                response.status().as_u16(),
                www_authenticate(response.headers()),
            )
            .await
        {
            RefreshOutcome::RefreshFailed(error) => {
                Err(callout_denied(format!("auth refresh failed: {error}")))
            },
            _ => Ok(response),
        }
    }

    async fn send_once(
        &self,
        reqwest_method: reqwest::Method,
        transport: &ApprovedHttpTransport,
        url: &str,
        headers: &[(String, String)],
        body: Option<&[u8]>,
        wait: Wait,
    ) -> Result<reqwest::Response, wit_types::CalloutResult> {
        let auth_header = match &self.auth {
            Some(auth) => auth
                .authorization_for(url)
                .await
                .map_err(|error| callout_denied(format!("auth authorization failed: {error}")))?,
            None => None,
        };

        let header_map = match build_header_map(
            auth_header
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str())),
            headers
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str())),
        ) {
            Ok(header_map) => header_map,
            Err(message) => return Err(callout_internal(message)),
        };

        let (client, request_url) = self.client_and_url_for(transport)?;
        let mut request = client
            .request(reqwest_method, request_url)
            .headers(header_map);
        if let Some(body) = body {
            request = request.body(body.to_vec());
        }
        match wait {
            Wait::Whole(timeout) => request.timeout(timeout).send().await,
            Wait::Status(timeout) => tokio::time::timeout(timeout, request.send())
                .await
                .map_err(|_| callout_network(idle_message(timeout)))?,
        }
        .map_err(|e| callout_network(e.to_string()))
    }

    fn client_and_url_for(
        &self,
        transport: &ApprovedHttpTransport,
    ) -> Result<(reqwest::Client, Url), wit_types::CalloutResult> {
        match transport {
            ApprovedHttpTransport::Https(url) => Ok((self.https_client.clone(), url.clone())),
            ApprovedHttpTransport::Unix {
                socket,
                request_url,
                ..
            } => {
                let client = self.unix_client_for(socket).map_err(|e| {
                    callout_internal(format!(
                        "failed to build unix client for {}: {e}",
                        socket.display()
                    ))
                })?;
                Ok((client, request_url.clone()))
            },
        }
    }

    fn unix_client_for(&self, socket: &Path) -> Result<reqwest::Client, reqwest::Error> {
        if let Some(existing) = self.unix_clients.get(socket) {
            return Ok(existing.clone());
        }
        let client = Self::client_builder()
            .unix_socket(socket.to_path_buf())
            .build()?;
        Ok(self
            .unix_clients
            .entry(socket.to_path_buf())
            .or_insert(client)
            .clone())
    }

    /// In-memory HTTP fetch: dispatch the request and decode the
    /// response body into a `CalloutResult::HttpResponse`.
    #[tracing::instrument(target = "omnifs_callout", skip_all, fields(
        method = req.method.as_str(),
        url = %LogUrl(&req.url),
        request_headers = %WitHeaders(&req.headers),
        request_body_bytes = req.body.as_ref().map_or(0, Vec::len),
        status = tracing::field::Empty,
        response_headers = tracing::field::Empty,
        response_body_bytes = tracing::field::Empty,
        error.kind = tracing::field::Empty,
        error.message = tracing::field::Empty,
        error.retryable = tracing::field::Empty,
    ))]
    pub async fn fetch(
        &self,
        req: &wit_types::HttpRequest,
        timeout: Duration,
    ) -> wit_types::CalloutResult {
        let result = match self.answer(req, timeout).await {
            Ok(response) => wit_types::CalloutResult::HttpResponse(response),
            Err(early) => early,
        };
        record_outcome(&result);
        result
    }

    async fn answer(
        &self,
        req: &wit_types::HttpRequest,
        timeout: Duration,
    ) -> Result<wit_types::HttpResponse, wit_types::CalloutResult> {
        let request = self.validate(
            &req.method,
            &req.url,
            &req.headers,
            req.body.as_deref(),
            req.follow_redirects,
        )?;
        let Answer {
            response,
            redirected,
        } = self
            .send_validated(&request, None, Wait::Whole(timeout))
            .await?;
        let status = response.status().as_u16();
        let headers = wit_headers(decode_response_headers(response.headers()));
        let body = response
            .bytes()
            .await
            .map_err(|error| callout_network(error.to_string()))?;
        Ok(wit_types::HttpResponse {
            status,
            headers,
            body: body.to_vec(),
            redirected,
        })
    }
}

/// The message of the error of an upstream that sent nothing for `bound`.
pub(crate) fn idle_message(bound: Duration) -> String {
    format!(
        "the upstream sent nothing for {} seconds",
        bound.as_secs_f64()
    )
}

/// The `Location` of an answer that redirects: of the status `301`, `302`,
/// `303`, `307`, or `308`.
fn redirect_location(response: &reqwest::Response) -> Option<&HeaderValue> {
    if !matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
        return None;
    }
    response.headers().get(reqwest::header::LOCATION)
}

/// The URL of `location` against the URL of the hop over `transport` that
/// answered it.
fn redirect_target(transport: &ApprovedHttpTransport, location: &HeaderValue) -> Option<Url> {
    let ApprovedHttpTransport::Https(url) = transport else {
        return None;
    };
    url.join(location.to_str().ok()?).ok()
}

/// Whether the hop over `hop` is to the origin of the request over
/// `request`: the same scheme, host, and port.
fn same_origin(request: &ApprovedHttpTransport, hop: &ApprovedHttpTransport) -> bool {
    let (ApprovedHttpTransport::Https(request), ApprovedHttpTransport::Https(hop)) = (request, hop)
    else {
        return false;
    };
    request.host() == hop.host() && request.port_or_known_default() == hop.port_or_known_default()
}

pub(crate) fn wit_headers(headers: Vec<(String, String)>) -> Vec<wit_types::Header> {
    headers
        .into_iter()
        .map(|(name, value)| wit_types::Header { name, value })
        .collect()
}

/// Combine auth and request headers into a single `HeaderMap`. Invalid
/// header names or values are reported with their source so the
/// caller's diagnostics distinguish a bad provider header from a bad
/// host-injected auth header.
fn build_header_map<'a, A, R>(auth_headers: A, request_headers: R) -> Result<HeaderMap, String>
where
    A: IntoIterator<Item = (&'a str, &'a str)>,
    R: IntoIterator<Item = (&'a str, &'a str)>,
{
    let mut header_map = HeaderMap::new();
    append_headers(&mut header_map, auth_headers, "auth")?;
    append_headers(&mut header_map, request_headers, "request")?;
    Ok(header_map)
}

fn append_headers<'a, I>(header_map: &mut HeaderMap, headers: I, source: &str) -> Result<(), String>
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    for (name, value) in headers {
        let header_name = HeaderName::from_str(name)
            .map_err(|error| format!("invalid {source} header name `{name}`: {error}"))?;
        let header_value = HeaderValue::from_str(value).map_err(|error| {
            format!(
                "invalid {source} header value for `{}`: {error}",
                header_name.as_str()
            )
        })?;
        header_map.append(header_name, header_value);
    }
    Ok(())
}

/// Decode a `HeaderMap` from reqwest into the WIT-friendly `(name,
/// value)` shape, dropping non-UTF8 values rather than failing the
/// whole response (provider headers are UTF-8 only by contract).
pub(crate) fn decode_response_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(name, value)| match value.to_str() {
            Ok(value) => Some((name.as_str().to_string(), value.to_string())),
            Err(error) => {
                warn!(
                    header = %name,
                    err = %error,
                    "dropping non-UTF8 response header because provider headers are UTF-8 only"
                );
                None
            },
        })
        .collect()
}
