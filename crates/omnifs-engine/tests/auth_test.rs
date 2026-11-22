use omnifs_auth::{
    AuthBinding, AuthManifest, AuthScheme, CredentialId, CredentialService, OAuthClient, OAuthFlow,
    OAuthRequest, OAuthRuntimeOverrides, OauthScheme, PkceManualCodeConfig, RefreshCandidate,
    RefreshOutcome, RefreshPersistError, RefreshPersistence, RefreshSink, StaticTokenScheme,
    TokenEndpointAuthMethod,
};
use omnifs_auth::{CredentialEntry, DurableCredentialSnapshot};
use omnifs_core::CredentialVersion;
use omnifs_engine::test_support::authority::RuntimeAuthority;
use omnifs_engine::test_support::blob::{BlobExecutor, BlobLimits};
use omnifs_engine::test_support::cache::{Caches, mount};
use omnifs_engine::test_support::http::HttpStack;
use omnifs_wit::host::types as wit_types;
use secrecy::{ExposeSecret, SecretString};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use time::OffsetDateTime;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;

fn github_pat_manifest() -> AuthManifest {
    AuthManifest {
        schemes: vec![AuthScheme::StaticToken(StaticTokenScheme {
            key: "pat".to_string(),
            header_name: Some("Authorization".to_string()),
            value_prefix: "Bearer ".to_string(),
            description: "pat".to_string(),
            inject_domains: vec!["api.github.com".to_string()],
            creation_url: None,
            validation: None,
            ambient_sources: Vec::new(),
        })],
    }
}

fn static_binding(manifest: &AuthManifest, entry: Option<CredentialEntry>) -> Arc<AuthBinding> {
    let id = CredentialId::new("github", "pat", "default").unwrap();
    let service = Arc::new(durable_service(
        id.clone(),
        entry,
        Arc::new(TestRefreshSink::default()),
    ));
    let scheme = manifest
        .resolve_static_scheme(Some("pat"))
        .expect("static scheme");
    Arc::new(
        service
            .bind_static(
                id,
                scheme.inject_domains.clone(),
                scheme
                    .header_name
                    .clone()
                    .unwrap_or_else(|| "Authorization".to_owned()),
                scheme.value_prefix.clone(),
            )
            .expect("configured auth"),
    )
}

fn oauth_binding_from_manifest(
    manifest: &AuthManifest,
    entry: Option<CredentialEntry>,
    client_id: Option<String>,
    sink: Arc<dyn RefreshSink>,
) -> Arc<AuthBinding> {
    let id = CredentialId::new("test-provider", "oauth", "default").unwrap();
    let service = Arc::new(durable_service(id.clone(), entry, sink));
    let scheme = manifest
        .resolve_oauth_scheme(Some("oauth"))
        .expect("oauth scheme");
    let request = OAuthRequest::from_runtime(
        scheme.clone(),
        OAuthRuntimeOverrides {
            client_id,
            ..OAuthRuntimeOverrides::default()
        },
    )
    .expect("oauth request");
    Arc::new(
        service
            .bind_oauth(
                id,
                request,
                scheme.inject_domains.clone(),
                scheme
                    .inject_header_name
                    .clone()
                    .unwrap_or_else(|| "Authorization".to_owned()),
                scheme.inject_value_prefix.clone(),
            )
            .expect("configured oauth"),
    )
}

#[tokio::test]
async fn test_missing_credential_fails_closed() {
    let manager = static_binding(&github_pat_manifest(), None);
    let error = manager
        .authorization_for("https://api.github.com/repos")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("no credential is stored"));
}

#[tokio::test]
async fn test_static_token_injection_from_durable_snapshot() {
    let manager = static_binding(
        &github_pat_manifest(),
        Some(CredentialEntry::static_token(SecretString::from(
            "ghp_store_token".to_string(),
        ))),
    );

    assert_eq!(
        manager
            .authorization_for("https://api.github.com/repos")
            .await
            .unwrap(),
        Some((
            "Authorization".to_string(),
            "Bearer ghp_store_token".to_string()
        ))
    );
}

#[tokio::test]
async fn test_auth_manifest_backed_static_token_injection() {
    let manifest = AuthManifest {
        schemes: vec![AuthScheme::StaticToken(StaticTokenScheme {
            key: "pat".to_string(),
            header_name: Some("X-Test-Token".to_string()),
            value_prefix: "token ".to_string(),
            description: "test token".to_string(),
            inject_domains: vec!["api.example.com".to_string()],
            creation_url: None,
            validation: None,
            ambient_sources: Vec::new(),
        })],
    };
    let manager = static_binding(
        &manifest,
        Some(CredentialEntry::static_token(SecretString::from(
            "secret".to_string(),
        ))),
    );

    assert_eq!(
        manager
            .authorization_for("https://api.example.com/repos")
            .await
            .unwrap(),
        Some(("X-Test-Token".to_string(), "token secret".to_string()))
    );
    assert_eq!(
        manager
            .authorization_for("https://other.example.com/repos")
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn test_auth_manifest_backed_static_token_missing_credential_fails_closed() {
    let manifest = AuthManifest {
        schemes: vec![AuthScheme::StaticToken(StaticTokenScheme {
            key: "pat".to_string(),
            header_name: None,
            value_prefix: "Bearer ".to_string(),
            description: "test token".to_string(),
            inject_domains: vec!["api.example.com".to_string()],
            creation_url: None,
            validation: None,
            ambient_sources: Vec::new(),
        })],
    };

    let manager = static_binding(&manifest, None);

    let error = manager
        .authorization_for("https://api.example.com/repos")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("no credential is stored"));
}

#[tokio::test]
async fn refresh_without_stored_oauth_credential_reports_no_credential() {
    let tokens = FakeTokenServer::start(false).await;
    let (auth, _sink, _key) =
        oauth_binding_without_credential(tokens.endpoint(), "localhost".to_string());

    assert_eq!(
        auth.report_rejected_for_response("https://localhost/resource", 401, None,)
            .await,
        RefreshOutcome::NoCredential
    );
    assert_eq!(tokens.refreshes(), 0);
}

#[tokio::test]
async fn test_execute_fetch_returns_denied_when_auth_is_required_but_missing() {
    // Create a mount binding with a config that requires auth for api.github.com
    // but has no stored credential. Authorization must fail closed before the
    // request is dispatched.
    let auth = static_binding(&github_pat_manifest(), None);

    assert!(
        auth.authorization_for("https://api.github.com/repos")
            .await
            .is_err()
    );

    let authority = RuntimeAuthority::for_test(&["api.github.com"], &[], &[]);
    let stack = HttpStack::new(Some(auth), authority).unwrap();

    let req = wit_types::HttpRequest {
        method: "GET".to_string(),
        url: "https://api.github.com/repos".to_string(),
        headers: Vec::new(),
        body: None,
        follow_redirects: false,
    };
    match stack.fetch(&req, Duration::from_secs(30)).await {
        wit_types::CalloutResult::CalloutError(wit_types::CalloutError {
            kind: wit_types::ErrorKind::Denied,
            retryable: false,
            ..
        }) => {},
        other => panic!("expected denied error, got {other:?}"),
    }
}

/// A static token that the upstream answers with 401 at a callout requires
/// re-authentication in the health of its mount binding from that answer
/// on, with the answer itself for the provider, and is no rejected one
/// again once the upstream takes it.
#[tokio::test]
async fn a_static_token_that_the_upstream_rejects_at_a_callout_requires_reauthentication() {
    let api = FakeHttpsApiServer::start("Bearer taken", "ok").await;
    let manifest = AuthManifest {
        schemes: vec![AuthScheme::StaticToken(StaticTokenScheme {
            inject_domains: vec![FakeHttpsApiServer::domain()],
            ..match github_pat_manifest().schemes.remove(0) {
                AuthScheme::StaticToken(scheme) => scheme,
                _ => unreachable!("the manifest of a static token"),
            }
        })],
    };
    let status_with = |token: &'static str| {
        let manifest = &manifest;
        let api = &api;
        async move {
            let auth = static_binding(
                manifest,
                Some(CredentialEntry::static_token(SecretString::from(token))),
            );
            assert_eq!(
                auth.health(),
                omnifs_auth::CredentialHealth::StaticUnvalidated
            );
            let stack = HttpStack::with_https_client(
                Some(Arc::clone(&auth)),
                RuntimeAuthority::for_test(&[FakeHttpsApiServer::domain().as_str()], &[], &[]),
                test_https_client(),
            );
            let request = wit_types::HttpRequest {
                method: "GET".to_string(),
                url: api.url(),
                headers: Vec::new(),
                body: None,
                follow_redirects: false,
            };
            match stack.fetch(&request, Duration::from_secs(5)).await {
                wit_types::CalloutResult::HttpResponse(response) => {
                    (response.status, auth.health())
                },
                other => panic!("expected the answer of the upstream, got {other:?}"),
            }
        }
    };
    assert_eq!(
        status_with("revoked").await,
        (401, omnifs_auth::CredentialHealth::NeedsConsent)
    );
    assert_eq!(
        status_with("taken").await,
        (200, omnifs_auth::CredentialHealth::StaticUnvalidated)
    );
}

#[tokio::test]
async fn oauth_401_refreshes_and_retries_once() {
    let tokens = FakeTokenServer::start(false).await;
    let api = FakeHttpsApiServer::start("Bearer access-refresh-1", "ok").await;
    let (auth, sink, _key) = oauth_binding(tokens.endpoint(), FakeHttpsApiServer::domain());
    auth.authorization_for(&api.url())
        .await
        .unwrap()
        .expect("oauth authorization header");

    let stack = HttpStack::with_https_client(
        Some(Arc::clone(&auth)),
        RuntimeAuthority::for_test(&[FakeHttpsApiServer::domain().as_str()], &[], &[]),
        test_https_client(),
    );

    let req = wit_types::HttpRequest {
        method: "GET".to_string(),
        url: api.url(),
        headers: Vec::new(),
        body: None,
        follow_redirects: false,
    };

    match stack.fetch(&req, Duration::from_secs(5)).await {
        wit_types::CalloutResult::HttpResponse(response) => {
            assert_eq!(response.status, 200);
            assert_eq!(response.body, b"ok");
        },
        other => panic!("expected successful response, got {other:?}"),
    }

    assert_eq!(api.calls(), 2);
    assert_eq!(tokens.refreshes(), 1);
    assert_eq!(sink.calls.load(Ordering::SeqCst), 1);
    let candidates = sink.candidates.lock().unwrap();
    assert_eq!(
        candidates[0].refreshed.access_token().expose_secret(),
        "access-refresh-1"
    );
}

#[tokio::test]
async fn concurrent_oauth_refreshes_coalesce_inside_one_process() {
    let tokens = FakeTokenServer::start(false).await;
    let (auth, _sink, _key) = oauth_binding(tokens.endpoint(), "localhost".to_string());
    auth.authorization_for("https://localhost/resource")
        .await
        .unwrap()
        .expect("oauth authorization header");

    let results = futures::future::join_all((0..8).map(|_| {
        let auth = Arc::clone(&auth);
        async move {
            auth.report_rejected_for_response("https://localhost/resource", 401, None)
                .await
        }
    }))
    .await;

    assert!(
        results
            .into_iter()
            .all(|result| result == RefreshOutcome::Refreshed)
    );
    assert_eq!(tokens.refreshes(), 1);
}

#[tokio::test]
async fn fetch_blob_uses_same_oauth_retry_path() {
    let tokens = FakeTokenServer::start(false).await;
    let api = FakeHttpsApiServer::start("Bearer access-refresh-1", "blob-body").await;
    let (auth, _sink, _key) = oauth_binding(tokens.endpoint(), FakeHttpsApiServer::domain());
    assert!(auth.applies_to_url(&api.url()));
    assert!(!auth.applies_to_url("https://127.0.0.1/resource"));
    auth.authorization_for(&api.url())
        .await
        .unwrap()
        .expect("oauth authorization header");

    let stack = Arc::new(HttpStack::with_https_client(
        Some(auth),
        RuntimeAuthority::for_test(&[FakeHttpsApiServer::domain().as_str()], &[], &[]),
        test_https_client(),
    ));
    let temp = tempfile::tempdir().unwrap();
    let caches = Caches::open(temp.path()).unwrap();
    let name = omnifs_core::MountName::new("test").unwrap();
    let resources = mount(&caches, &name, b"auth-test").unwrap();
    let executor = BlobExecutor::new(stack, resources, BlobLimits::default());

    let result = executor
        .fetch(
            &wit_types::BlobFetchRequest {
                method: "GET".to_string(),
                url: api.url(),
                headers: Vec::new(),
                body: None,
                md5: None,
                version: None,
                follow_redirects: false,
            },
            0,
        )
        .await;

    match result {
        wit_types::CalloutResult::BlobFetched(blob) => {
            assert_eq!(blob.status, 200);
            assert_eq!(size(&blob), Some(9));
        },
        other => panic!("expected blob fetch, got {other:?}"),
    }
    assert_eq!(api.calls(), 2);
    assert_eq!(tokens.refreshes(), 1);
}

/// A blob fetch with an MD5 answers with the bytes of that MD5 alone: other
/// bytes are the error of the fetch, which the host keeps nothing of, and
/// the bytes of each MD5 are the blob of their own request.
#[tokio::test]
async fn fetch_blob_answers_the_bytes_of_the_md5_that_the_provider_asks_for() {
    let tokens = FakeTokenServer::start(false).await;
    let api = FakeHttpsApiServer::start("Bearer access-refresh-1", "blob-body").await;
    let (auth, _sink, _key) = oauth_binding(tokens.endpoint(), FakeHttpsApiServer::domain());
    let stack = Arc::new(HttpStack::with_https_client(
        Some(auth),
        RuntimeAuthority::for_test(&[FakeHttpsApiServer::domain().as_str()], &[], &[]),
        test_https_client(),
    ));
    let temp = tempfile::tempdir().unwrap();
    let caches = Caches::open(temp.path()).unwrap();
    let name = omnifs_core::MountName::new("test").unwrap();
    let resources = mount(&caches, &name, b"auth-test").unwrap();
    let executor = BlobExecutor::new(stack, resources, BlobLimits::default());
    let fetch = |md5: &str| {
        let request = wit_types::BlobFetchRequest {
            method: "GET".to_string(),
            url: api.url(),
            headers: Vec::new(),
            body: None,
            md5: Some(md5.to_string()),
            version: None,
            follow_redirects: false,
        };
        let executor = &executor;
        async move { executor.fetch(&request, 0).await }
    };

    let other = fetch("00000000000000000000000000000000").await;
    assert!(
        matches!(
            &other,
            wit_types::CalloutResult::CalloutError(error)
                if error.message.contains("3d93da86c9045cb1b17e96790cd19789")
                    && error.kind == wit_types::ErrorKind::VersionMismatch
                    && !error.retryable
        ),
        "{other:?}"
    );
    let calls = api.calls();
    let again = fetch("00000000000000000000000000000000").await;
    assert!(
        matches!(again, wit_types::CalloutResult::CalloutError(_)),
        "{again:?}"
    );
    assert!(api.calls() > calls, "the refused bytes answer no request");

    let calls = api.calls();
    let asked = fetch("3d93da86c9045cb1b17e96790cd19789").await;
    assert!(
        matches!(&asked, wit_types::CalloutResult::BlobFetched(blob) if size(blob) == Some(9) && blob.upstream),
        "{asked:?}"
    );
    assert!(api.calls() > calls);
    let calls = api.calls();
    let held = fetch("3d93da86c9045cb1b17e96790cd19789").await;
    assert!(
        matches!(&held, wit_types::CalloutResult::BlobFetched(blob) if size(blob) == Some(9) && !blob.upstream),
        "{held:?}"
    );
    assert_eq!(api.calls(), calls, "the host holds the bytes of the MD5");
}

#[tokio::test]
async fn oauth_refresh_failure_surfaces_denied_and_preserves_snapshot() {
    let tokens = FakeTokenServer::start(true).await;
    let api = FakeHttpsApiServer::start("Bearer never-used", "ok").await;
    let (auth, sink, _key) = oauth_binding(tokens.endpoint(), FakeHttpsApiServer::domain());
    auth.authorization_for(&api.url())
        .await
        .unwrap()
        .expect("oauth authorization header");

    let stack = HttpStack::with_https_client(
        Some(Arc::clone(&auth)),
        RuntimeAuthority::for_test(&[FakeHttpsApiServer::domain().as_str()], &[], &[]),
        test_https_client(),
    );

    let req = wit_types::HttpRequest {
        method: "GET".to_string(),
        url: api.url(),
        headers: Vec::new(),
        body: None,
        follow_redirects: false,
    };

    match stack.fetch(&req, Duration::from_secs(5)).await {
        wit_types::CalloutResult::CalloutError(wit_types::CalloutError {
            kind: wit_types::ErrorKind::Denied,
            retryable: false,
            ..
        }) => {},
        other => panic!("expected denied refresh failure, got {other:?}"),
    }

    assert_eq!(api.calls(), 1);
    assert_eq!(tokens.refreshes(), 0);
    assert_eq!(sink.calls.load(Ordering::SeqCst), 0);
    let err = auth.authorization_for(&api.url()).await.unwrap_err();
    assert!(err.to_string().contains("needs re-authentication"));
}

#[tokio::test]
async fn oauth_config_client_id_overrides_missing_manifest_default_for_refresh() {
    let tokens = FakeTokenServer::start_with_expected_client_id(false, Some("byo-client")).await;

    let mut manifest = oauth_manifest(tokens.endpoint(), "localhost".to_string());
    let AuthScheme::Oauth(scheme) = &mut manifest.schemes[0] else {
        panic!("expected oauth scheme");
    };
    scheme.default_client_id = None;

    let auth = oauth_binding_from_manifest(
        &manifest,
        Some(oauth_entry(
            "old-access",
            "refresh-1",
            OffsetDateTime::now_utc() + time::Duration::seconds(3600),
        )),
        Some("byo-client".to_owned()),
        Arc::new(TestRefreshSink::default()),
    );
    assert_eq!(
        auth.report_rejected_for_response("https://localhost/resource", 401, None,)
            .await,
        RefreshOutcome::Refreshed
    );
    assert_eq!(tokens.refreshes(), 1);
}

fn oauth_manifest(token_endpoint: String, inject_domain: String) -> AuthManifest {
    AuthManifest {
        schemes: vec![AuthScheme::Oauth(OauthScheme {
            key: "oauth".to_string(),
            display_name: "test oauth".to_string(),
            authorization_endpoint: "http://localhost/authorize".to_string(),
            token_endpoint,
            revocation_endpoint: None,
            default_client_id: Some("client-id".to_string()),
            default_scopes: vec!["read".to_string()],
            flow: OAuthFlow::PkceManualCode(PkceManualCodeConfig {
                redirect_uri: "http://localhost/callback".to_string(),
            }),
            token_endpoint_auth: TokenEndpointAuthMethod::None,
            refresh_token_rotates: true,
            extra_authorize_params: vec![],
            extra_token_params: vec![],
            inject_domains: vec![inject_domain],
            inject_header_name: None,
            inject_value_prefix: "Bearer ".to_string(),
        })],
    }
}

fn oauth_binding(
    token_endpoint: String,
    inject_domain: String,
) -> (Arc<AuthBinding>, Arc<TestRefreshSink>, CredentialId) {
    let key = CredentialId::new("test-provider", "oauth", "default").unwrap();
    let sink = Arc::new(TestRefreshSink::default());
    let auth = oauth_binding_from_manifest(
        &oauth_manifest(token_endpoint, inject_domain),
        Some(oauth_entry(
            "old-access",
            "refresh-1",
            OffsetDateTime::now_utc() + time::Duration::seconds(3600),
        )),
        None,
        Arc::clone(&sink) as Arc<dyn RefreshSink>,
    );
    (auth, sink, key)
}

fn oauth_binding_without_credential(
    token_endpoint: String,
    inject_domain: String,
) -> (Arc<AuthBinding>, Arc<TestRefreshSink>, CredentialId) {
    let key = CredentialId::new("test-provider", "oauth", "default").unwrap();
    let sink = Arc::new(TestRefreshSink::default());
    let auth = oauth_binding_from_manifest(
        &oauth_manifest(token_endpoint, inject_domain),
        None,
        None,
        Arc::clone(&sink) as Arc<dyn RefreshSink>,
    );
    (auth, sink, key)
}

fn durable_service(
    id: CredentialId,
    entry: Option<CredentialEntry>,
    sink: Arc<dyn RefreshSink>,
) -> CredentialService {
    let snapshots = entry.map(|entry| {
        (
            id,
            DurableCredentialSnapshot {
                entry,
                version: CredentialVersion::initial(),
            },
        )
    });
    CredentialService::new(snapshots, OAuthClient::new().expect("oauth client"), sink)
}

struct TestRefreshSink {
    calls: AtomicUsize,
    candidates: std::sync::Mutex<Vec<RefreshCandidate>>,
}

impl Default for TestRefreshSink {
    fn default() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            candidates: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl RefreshSink for TestRefreshSink {
    fn persist<'a>(
        &'a self,
        candidate: RefreshCandidate,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<RefreshPersistence, RefreshPersistError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let version = candidate.expected_version.next().expect("version advances");
            self.candidates.lock().unwrap().push(candidate);
            Ok(RefreshPersistence::Active { version })
        })
    }
}

fn oauth_entry(
    access_token: &str,
    refresh_token: &str,
    expires_at: OffsetDateTime,
) -> CredentialEntry {
    CredentialEntry::oauth(
        SecretString::from(access_token.to_string()),
        Some(SecretString::from(refresh_token.to_string())),
        Some(expires_at),
        "Bearer",
        vec!["read".to_string()],
    )
}

fn test_https_client() -> reqwest::Client {
    reqwest::ClientBuilder::new()
        .danger_accept_invalid_certs(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

#[derive(Clone)]
struct FakeTokenServer {
    endpoint: String,
    refreshes: Arc<AtomicUsize>,
    expected_client_id: Option<String>,
}

impl FakeTokenServer {
    async fn start(fail: bool) -> Self {
        Self::start_with_expected_client_id(fail, None).await
    }

    async fn start_with_expected_client_id(fail: bool, expected_client_id: Option<&str>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = Self {
            endpoint: format!("http://{addr}/token"),
            refreshes: Arc::new(AtomicUsize::new(0)),
            expected_client_id: expected_client_id.map(str::to_owned),
        };
        let task_server = server.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let task_server = task_server.clone();
                tokio::spawn(async move {
                    let request = read_http_request(&mut stream).await;
                    assert!(request.starts_with("POST /token "));
                    let body = request.split("\r\n\r\n").nth(1).unwrap_or_default();
                    let params: std::collections::HashMap<String, String> =
                        url::form_urlencoded::parse(body.as_bytes())
                            .into_owned()
                            .collect();
                    assert_eq!(
                        params.get("grant_type").map(String::as_str),
                        Some("refresh_token")
                    );
                    if let Some(expected_client_id) = &task_server.expected_client_id {
                        assert_eq!(
                            params.get("client_id").map(String::as_str),
                            Some(expected_client_id.as_str())
                        );
                    }
                    if fail {
                        let body = r#"{"error":"invalid_grant","error_description":"revoked"}"#;
                        write_http_response(
                            &mut stream,
                            "400 Bad Request",
                            "application/json",
                            body,
                        )
                        .await;
                        return;
                    }
                    let id = task_server.refreshes.fetch_add(1, Ordering::SeqCst) + 1;
                    let body = serde_json::json!({
                        "access_token": format!("access-refresh-{id}"),
                        "refresh_token": format!("refresh-rotated-{id}"),
                        "expires_in": 3600,
                        "token_type": "Bearer",
                        "scope": "read",
                    })
                    .to_string();
                    write_http_response(&mut stream, "200 OK", "application/json", &body).await;
                });
            }
        });
        server
    }

    fn endpoint(&self) -> String {
        self.endpoint.clone()
    }

    fn refreshes(&self) -> usize {
        self.refreshes.load(Ordering::SeqCst)
    }
}

#[derive(Clone)]
struct FakeHttpsApiServer {
    url: String,
    calls: Arc<AtomicUsize>,
}

impl FakeHttpsApiServer {
    async fn start(success_authorization: &'static str, success_body: &'static str) -> Self {
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let cert_der = cert.cert.der().clone();
        let key_der = tokio_rustls::rustls::pki_types::PrivateKeyDer::Pkcs8(
            tokio_rustls::rustls::pki_types::PrivatePkcs8KeyDer::from(
                cert.key_pair.serialize_der(),
            ),
        );
        let config = tokio_rustls::rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der], key_der)
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let server = Self {
            url: format!("https://localhost:{}/resource", addr.port()),
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let task_server = server.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let acceptor = acceptor.clone();
                let task_server = task_server.clone();
                tokio::spawn(async move {
                    let Ok(mut stream) = acceptor.accept(stream).await else {
                        return;
                    };
                    let request = read_http_request(&mut stream).await;
                    task_server.calls.fetch_add(1, Ordering::SeqCst);
                    let authorization = authorization_header(&request);
                    if authorization.as_deref() == Some(success_authorization) {
                        write_http_response(&mut stream, "200 OK", "text/plain", success_body)
                            .await;
                    } else {
                        let response = concat!(
                            "HTTP/1.1 401 Unauthorized\r\n",
                            "www-authenticate: Bearer error=\"invalid_token\"\r\n",
                            "content-length: 0\r\n",
                            "connection: close\r\n",
                            "\r\n"
                        );
                        stream.write_all(response.as_bytes()).await.unwrap();
                    }
                });
            }
        });
        server
    }

    fn url(&self) -> String {
        self.url.clone()
    }

    fn domain() -> String {
        "localhost".to_string()
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

async fn read_http_request<R>(stream: &mut R) -> String
where
    R: AsyncRead + Unpin,
{
    let mut buf = vec![0; 8192];
    let read = stream.read(&mut buf).await.unwrap();
    String::from_utf8_lossy(&buf[..read]).into_owned()
}

async fn write_http_response<W>(stream: &mut W, status: &str, content_type: &str, body: &str)
where
    W: AsyncWrite + Unpin,
{
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await.unwrap();
}

fn authorization_header(request: &str) -> Option<String> {
    request.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("authorization")
            .then(|| value.trim().to_string())
    })
}

fn size(blob: &wit_types::BlobFetched) -> Option<u64> {
    blob.content.as_ref().map(|content| content.size)
}

/// What a scripted upstream does with a request: answer it, or send the
/// start of an answer and nothing after.
enum Reply {
    Answer(String),
    Stall(String),
}

fn reply(status: &str, headers: &[(&str, &str)], body: &str) -> Reply {
    let headers = headers
        .iter()
        .map(|(name, value)| format!("{name}: {value}\r\n"))
        .collect::<Vec<_>>()
        .concat();
    Reply::Answer(format!(
        "HTTP/1.1 {status}\r\ncontent-length: {}\r\n{headers}connection: close\r\n\r\n{body}",
        body.len()
    ))
}

/// A local HTTPS upstream that answers each request by its script and keeps
/// the requests that it took.
#[derive(Clone)]
struct Upstream {
    port: u16,
    requests: Arc<std::sync::Mutex<Vec<String>>>,
}

impl Upstream {
    async fn start(script: impl Fn(&str) -> Reply + Send + Sync + 'static) -> Self {
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let cert = rcgen::generate_simple_self_signed(vec![
            "localhost".to_string(),
            "files.test".to_string(),
        ])
        .unwrap();
        let key = tokio_rustls::rustls::pki_types::PrivateKeyDer::Pkcs8(
            tokio_rustls::rustls::pki_types::PrivatePkcs8KeyDer::from(
                cert.key_pair.serialize_der(),
            ),
        );
        let config = tokio_rustls::rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert.cert.der().clone()], key)
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let upstream = Self {
            port: listener.local_addr().unwrap().port(),
            requests: Arc::default(),
        };
        let requests = Arc::clone(&upstream.requests);
        let script = Arc::new(script);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (acceptor, requests, script) =
                    (acceptor.clone(), Arc::clone(&requests), Arc::clone(&script));
                tokio::spawn(async move {
                    let Ok(mut stream) = acceptor.accept(stream).await else {
                        return;
                    };
                    let request = read_http_request(&mut stream).await.to_ascii_lowercase();
                    requests.lock().unwrap().push(request.clone());
                    match script(&request) {
                        Reply::Answer(answer) => {
                            stream.write_all(answer.as_bytes()).await.unwrap();
                        },
                        Reply::Stall(start) => {
                            stream.write_all(start.as_bytes()).await.unwrap();
                            stream.flush().await.unwrap();
                            tokio::time::sleep(Duration::from_secs(60)).await;
                        },
                    }
                });
            }
        });
        upstream
    }

    fn url(&self, host: &str, path: &str) -> String {
        format!("https://{host}:{}{path}", self.port)
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

/// A client that takes the certificate of a local upstream and finds
/// `files.test` at the loopback address.
fn local_client() -> reqwest::Client {
    reqwest::ClientBuilder::new()
        .danger_accept_invalid_certs(true)
        .redirect(reqwest::redirect::Policy::none())
        .resolve("files.test", "127.0.0.1:0".parse().unwrap())
        .build()
        .unwrap()
}

/// A stack whose mount approves `domains` and declares its token for
/// `localhost` alone.
fn local_stack(domains: &[&str]) -> HttpStack {
    let manifest = AuthManifest {
        schemes: vec![AuthScheme::StaticToken(StaticTokenScheme {
            key: "pat".to_string(),
            header_name: Some("Authorization".to_string()),
            value_prefix: "Bearer ".to_string(),
            description: "pat".to_string(),
            inject_domains: vec!["localhost".to_string()],
            creation_url: None,
            validation: None,
            ambient_sources: Vec::new(),
        })],
    };
    let auth = static_binding(
        &manifest,
        Some(CredentialEntry::static_token(SecretString::from(
            "secret-token".to_string(),
        ))),
    );
    HttpStack::with_https_client(
        Some(auth),
        RuntimeAuthority::for_test(domains, &[], &["/tmp/omnifs-test.sock"]),
        local_client(),
    )
}

fn get(url: String, follow_redirects: bool) -> wit_types::HttpRequest {
    wit_types::HttpRequest {
        method: "GET".to_string(),
        url,
        headers: vec![wit_types::Header {
            name: "x-provider".to_string(),
            value: "of-the-origin".to_string(),
        }],
        body: None,
        follow_redirects,
    }
}

fn path_of(request: &str) -> &str {
    request.split(' ').nth(1).unwrap_or_default()
}

fn refusal(result: &wit_types::CalloutResult) -> &wit_types::CalloutError {
    match result {
        wit_types::CalloutResult::CalloutError(error) => error,
        other => panic!("expected an error, got {other:?}"),
    }
}

/// The answer of a redirect is the answer of a request that did not ask the
/// host to follow it.
#[tokio::test]
async fn a_redirect_is_the_answer_of_a_request_that_does_not_follow_it() {
    let upstream = Upstream::start(|request| match path_of(request) {
        "/start" => reply("302 Found", &[("location", "/moved")], ""),
        _ => reply("200 OK", &[], "moved"),
    })
    .await;
    let stack = local_stack(&["localhost"]);
    let result = stack
        .fetch(
            &get(upstream.url("localhost", "/start"), false),
            Duration::from_secs(5),
        )
        .await;
    match result {
        wit_types::CalloutResult::HttpResponse(response) => {
            assert_eq!((response.status, response.redirected), (302, None));
        },
        other => panic!("expected the redirect, got {other:?}"),
    }
    assert_eq!(upstream.requests().len(), 1);
}

/// A request that follows redirects has the answer of the last URL, which
/// its answer names. Each hop has the credential that the mount declares
/// for its URL alone, and the headers of the provider go to the origin of
/// the request alone.
#[tokio::test]
async fn a_request_follows_each_redirect_that_it_asked_for_with_the_credential_and_the_headers_of_its_hop()
 {
    let files = Upstream::start(|_| reply("200 OK", &[], "the file")).await;
    let target = files.url("files.test", "/file");
    let api = Upstream::start(move |request| match path_of(request) {
        "/start" => reply("302 Found", &[("location", "/next?x=1")], ""),
        _ => reply("307 Temporary Redirect", &[("location", &target)], ""),
    })
    .await;
    let stack = local_stack(&["localhost", "files.test"]);
    let result = stack
        .fetch(
            &get(api.url("localhost", "/start"), true),
            Duration::from_secs(5),
        )
        .await;
    match result {
        wit_types::CalloutResult::HttpResponse(response) => {
            assert_eq!(response.status, 200);
            assert_eq!(response.body, b"the file");
            assert_eq!(response.redirected, Some(files.url("files.test", "/file")));
        },
        other => panic!("expected the answer of the last URL, got {other:?}"),
    }
    let api_requests = api.requests();
    assert_eq!(
        api_requests.iter().map(|r| path_of(r)).collect::<Vec<_>>(),
        ["/start", "/next?x=1"]
    );
    for request in &api_requests {
        assert!(
            request.contains("authorization: bearer secret-token"),
            "{request}"
        );
        assert!(request.contains("x-provider: of-the-origin"), "{request}");
    }
    let file_requests = files.requests();
    assert_eq!(file_requests.len(), 1);
    assert!(
        !file_requests[0].contains("authorization"),
        "{}",
        file_requests[0]
    );
    assert!(
        !file_requests[0].contains("x-provider"),
        "{}",
        file_requests[0]
    );
}

/// A hop that the host refuses, and one too many, is an error that is not
/// retryable and names the host of the hop: never the redirect as content.
#[tokio::test]
async fn a_refused_hop_and_one_too_many_are_errors_that_name_the_host() {
    let upstream = Upstream::start(|request| match path_of(request) {
        "/loop" => reply("302 Found", &[("location", "/loop")], ""),
        "/elsewhere" => reply("302 Found", &[("location", "https://other.test/x")], ""),
        "/plain" => reply("301 Moved", &[("location", "http://localhost/x")], ""),
        "/socket" => reply(
            "301 Moved",
            &[("location", "unix:///tmp/omnifs-test.sock")],
            "",
        ),
        _ => reply("200 OK", &[], "ok"),
    })
    .await;
    let stack = local_stack(&["localhost"]);
    let fetch = |path: &str| {
        let request = get(upstream.url("localhost", path), true);
        let stack = &stack;
        async move { stack.fetch(&request, Duration::from_secs(5)).await }
    };

    let looped = fetch("/loop").await;
    let error = refusal(&looped);
    assert_eq!(
        (error.kind, error.retryable),
        (wit_types::ErrorKind::Denied, false)
    );
    assert!(
        error.message.contains("more than 10 times") && error.message.contains("localhost"),
        "{}",
        error.message
    );
    assert_eq!(
        upstream.requests().len(),
        11,
        "the request and its ten hops"
    );

    for (path, host, reason) in [
        ("/elsewhere", "other.test", "does not approve"),
        ("/plain", "localhost", "leaves no HTTPS"),
        ("/socket", "", "leaves no HTTPS"),
    ] {
        let before = upstream.requests().len();
        let refused = fetch(path).await;
        let error = refusal(&refused);
        assert_eq!(
            (error.kind, error.retryable),
            (wit_types::ErrorKind::Denied, false),
            "{path}"
        );
        assert!(
            error.message.contains(host) && error.message.contains(reason),
            "{path}: {}",
            error.message
        );
        assert_eq!(
            upstream.requests().len(),
            before + 1,
            "{path}: no hop went out"
        );
    }
}

/// The host follows the redirects of a `GET` or a `HEAD` over HTTPS alone:
/// another request that asks for them is refused before it goes out.
#[tokio::test]
async fn a_request_over_a_socket_or_of_another_method_follows_no_redirect() {
    let stack = local_stack(&["localhost"]);
    let mut post = get("https://localhost/x".to_string(), true);
    post.method = "POST".to_string();
    let socket = get(
        format!("unix://{}/x", hex::encode("/tmp/omnifs-test.sock")),
        true,
    );
    for request in [post, socket] {
        let refused = stack.fetch(&request, Duration::from_secs(5)).await;
        let error = refusal(&refused);
        assert_eq!(
            (error.kind, error.retryable),
            (wit_types::ErrorKind::InvalidInput, false),
            "{}: {}",
            request.url,
            error.message
        );
    }
}

/// The refresh of a credential that an upstream rejects is of the hop that
/// rejected it.
#[tokio::test]
async fn a_hop_that_rejects_its_credential_refreshes_it_and_asks_again() {
    let tokens = FakeTokenServer::start(false).await;
    let upstream = Upstream::start(|request| match path_of(request) {
        "/start" => reply("302 Found", &[("location", "/resource")], ""),
        _ if request.contains("authorization: bearer access-refresh-1") => {
            reply("200 OK", &[], "fresh")
        },
        _ => reply(
            "401 Unauthorized",
            &[("www-authenticate", "Bearer error=\"invalid_token\"")],
            "",
        ),
    })
    .await;
    let (auth, _sink, _key) = oauth_binding(tokens.endpoint(), "localhost".to_string());
    let stack = HttpStack::with_https_client(
        Some(auth),
        RuntimeAuthority::for_test(&["localhost"], &[], &[]),
        local_client(),
    );
    let mut request = get(upstream.url("localhost", "/start"), true);
    request.headers.clear();
    let result = stack.fetch(&request, Duration::from_secs(5)).await;
    assert!(
        matches!(&result, wit_types::CalloutResult::HttpResponse(response)
            if response.status == 200 && response.body == b"fresh"),
        "{result:?}"
    );
    assert_eq!(tokens.refreshes(), 1);
    assert_eq!(
        upstream
            .requests()
            .iter()
            .map(|r| path_of(r))
            .collect::<Vec<_>>(),
        ["/start", "/resource", "/resource"]
    );
}

/// The answer to a refreshed credential that the upstream rejects again is
/// reported to the credential at once, and is the answer of the request.
#[tokio::test]
async fn an_upstream_that_rejects_the_refreshed_credential_is_reported_with_its_answer() {
    let tokens = FakeTokenServer::start(false).await;
    let upstream = Upstream::start(|_| {
        reply(
            "401 Unauthorized",
            &[("www-authenticate", "Bearer error=\"invalid_token\"")],
            "",
        )
    })
    .await;
    let (auth, _sink, _key) = oauth_binding(tokens.endpoint(), "localhost".to_string());
    let stack = HttpStack::with_https_client(
        Some(auth),
        RuntimeAuthority::for_test(&["localhost"], &[], &[]),
        local_client(),
    );
    let mut request = get(upstream.url("localhost", "/resource"), false);
    request.headers.clear();
    let result = stack.fetch(&request, Duration::from_secs(5)).await;
    assert!(
        matches!(&result, wit_types::CalloutResult::HttpResponse(response) if response.status == 401),
        "{result:?}"
    );
    assert_eq!(upstream.requests().len(), 2);
    assert_eq!(
        tokens.refreshes(),
        2,
        "each rejection reached the credential"
    );
}

struct Blobs {
    _temp: tempfile::TempDir,
    caches: Arc<Caches>,
    executor: BlobExecutor,
}

fn blobs(stack: HttpStack) -> Blobs {
    let temp = tempfile::tempdir().unwrap();
    let caches = Caches::open(temp.path()).unwrap();
    let name = omnifs_core::MountName::new("test").unwrap();
    let resources = mount(&caches, &name, b"blob-test").unwrap();
    let executor = BlobExecutor::new(Arc::new(stack), resources, BlobLimits::default());
    Blobs {
        _temp: temp,
        caches,
        executor,
    }
}

fn blob_request(url: String) -> wit_types::BlobFetchRequest {
    wit_types::BlobFetchRequest {
        method: "GET".to_string(),
        url,
        headers: Vec::new(),
        body: None,
        md5: None,
        version: None,
        follow_redirects: false,
    }
}

fn fetched(result: wit_types::CalloutResult) -> wit_types::BlobFetched {
    match result {
        wit_types::CalloutResult::BlobFetched(blob) => blob,
        other => panic!("expected a blob, got {other:?}"),
    }
}

/// The MD5 and the version of a blob request are facts that the host
/// validates before it asks its upstream.
#[tokio::test]
async fn a_blob_request_of_an_md5_or_a_version_that_is_not_valid_is_refused() {
    let upstream = Upstream::start(|_| reply("200 OK", &[], "body")).await;
    let blobs = blobs(local_stack(&["localhost"]));
    let long = "v".repeat(257);
    for (md5, version) in [
        (Some("3D93DA86C9045CB1B17E96790CD19789"), None),
        (Some("3d93da86c9045cb1b17e96790cd1978"), None),
        (Some("version:7"), None),
        (None, Some("")),
        (None, Some("one\ntwo")),
        (None, Some(long.as_str())),
    ] {
        let mut request = blob_request(upstream.url("localhost", "/file"));
        request.md5 = md5.map(str::to_owned);
        request.version = version.map(str::to_owned);
        let refused = blobs.executor.fetch(&request, 0).await;
        let error = refusal(&refused);
        assert_eq!(
            (error.kind, error.retryable),
            (wit_types::ErrorKind::InvalidInput, false),
            "{md5:?} {version:?}"
        );
    }
    assert!(upstream.requests().is_empty());
    let mut request = blob_request(upstream.url("localhost", "/file"));
    request.version = Some("v".repeat(256));
    assert_eq!(
        size(&fetched(blobs.executor.fetch(&request, 0).await)),
        Some(4)
    );
}

/// The answer of another status than a successful one has no body in the
/// host, and answers no later request.
#[tokio::test]
async fn the_answer_of_a_failing_status_stores_no_body_and_answers_no_other_request() {
    let upstream =
        Upstream::start(|_| reply("500 Oops", &[("retry-after", "7")], "a failure")).await;
    let blobs = blobs(local_stack(&["localhost"]));
    let mut request = blob_request(upstream.url("localhost", "/file"));
    request.version = Some("1".to_string());
    for asked in 1..=2 {
        let blob = fetched(blobs.executor.fetch(&request, 0).await);
        assert_eq!((blob.status, blob.upstream), (500, true));
        assert!(blob.content.is_none());
        assert!(
            blob.response_headers
                .iter()
                .any(|header| header.name == "retry-after" && header.value == "7")
        );
        assert_eq!(upstream.requests().len(), asked);
        assert!(omnifs_engine::test_support::cache::bodies(&blobs.caches).is_empty());
    }
}

/// The host waits for the status of an answer, and for each next bytes of
/// its body, for its idle bound alone, and keeps nothing of a body that
/// stopped.
#[tokio::test]
async fn a_blob_fetch_ends_with_an_upstream_that_sends_nothing_for_the_idle_bound() {
    let upstream = Upstream::start(|request| match path_of(request) {
        "/silent" => Reply::Stall(String::new()),
        _ => Reply::Stall("HTTP/1.1 200 OK\r\ncontent-length: 10\r\n\r\nhalf".to_string()),
    })
    .await;
    let mut blobs = blobs(local_stack(&["localhost"]));
    blobs.executor = blobs.executor.with_idle_bound(Duration::from_millis(300));
    for path in ["/silent", "/half"] {
        let started = std::time::Instant::now();
        let stopped = blobs
            .executor
            .fetch(&blob_request(upstream.url("localhost", path)), 0)
            .await;
        let error = refusal(&stopped);
        assert_eq!(
            (error.kind, error.retryable),
            (wit_types::ErrorKind::Network, true),
            "{path}"
        );
        assert!(
            error.message.contains("sent nothing for 0.3 seconds"),
            "{}",
            error.message
        );
        assert!(started.elapsed() < Duration::from_secs(10), "{path}");
        assert!(omnifs_engine::test_support::cache::bodies(&blobs.caches).is_empty());
    }
}

/// A blob request without the identity of its bytes keeps the validators
/// of its answer: the host asks its upstream whether the answer stands, and
/// downloads the bytes again for another answer alone.
#[tokio::test]
async fn a_blob_request_without_a_version_asks_whether_its_kept_answer_stands() {
    let version = Arc::new(AtomicUsize::new(1));
    let upstream = Upstream::start({
        let version = Arc::clone(&version);
        move |request| {
            let etag = format!("\"v{}\"", version.load(Ordering::SeqCst));
            match path_of(request) {
                "/start" => reply("302 Found", &[("location", "/file")], ""),
                "/plain" => reply("200 OK", &[], "no validator"),
                _ if request.contains(&format!("if-none-match: {etag}")) => {
                    reply("304 Not Modified", &[("etag", &etag)], "")
                },
                _ => reply("200 OK", &[("etag", &etag)], &format!("content {etag}")),
            }
        }
    })
    .await;
    let blobs = blobs(local_stack(&["localhost"]));
    let mut request = blob_request(upstream.url("localhost", "/start"));
    request.follow_redirects = true;
    let file = Some(upstream.url("localhost", "/file"));

    let first = fetched(blobs.executor.fetch(&request, 1).await);
    assert_eq!(
        (first.status, first.upstream, size(&first)),
        (200, true, Some(12))
    );
    assert_eq!(first.redirected, file);
    assert_eq!(
        omnifs_engine::test_support::cache::bodies(&blobs.caches).len(),
        1
    );

    // The second fetch asks the URL that answered with its validator, and
    // takes `304` as the answer that the host keeps: no body comes again.
    let second = fetched(blobs.executor.fetch(&request, 2).await);
    assert_eq!(
        (second.status, second.upstream, size(&second)),
        (200, true, Some(12))
    );
    assert_eq!(second.redirected, file);
    let requests = upstream.requests();
    assert_eq!(
        requests.iter().map(|r| path_of(r)).collect::<Vec<_>>(),
        ["/start", "/file", "/start", "/file"]
    );
    assert!(!requests[1].contains("if-none-match") && !requests[2].contains("if-none-match"));
    assert!(
        requests[3].contains("if-none-match: \"v1\""),
        "{}",
        requests[3]
    );
    assert_eq!(
        omnifs_engine::test_support::cache::bodies(&blobs.caches).len(),
        1
    );

    // Another answer replaces the kept one.
    version.store(2, Ordering::SeqCst);
    let third = fetched(blobs.executor.fetch(&request, 3).await);
    assert_eq!((third.status, size(&third)), (200, Some(12)));
    assert_eq!(
        omnifs_engine::test_support::cache::bodies(&blobs.caches).len(),
        2
    );
    let fourth = fetched(blobs.executor.fetch(&request, 4).await);
    assert_eq!(size(&fourth), Some(12));
    assert!(
        upstream
            .requests()
            .last()
            .unwrap()
            .contains("if-none-match: \"v2\"")
    );

    // The answer without a validator is of its operation alone: the host
    // asks each time.
    let plain = blob_request(upstream.url("localhost", "/plain"));
    for _ in 0..2 {
        let blob = fetched(blobs.executor.fetch(&plain, 5).await);
        assert_eq!((blob.upstream, size(&blob)), (true, Some(12)));
    }
    assert_eq!(
        upstream
            .requests()
            .iter()
            .filter(|r| path_of(r) == "/plain")
            .count(),
        2
    );
}
