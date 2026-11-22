//! Assembles one durable serving generation from durable state.
//!
//! [`GenerationDraft::load`] reads the durable serving snapshot;
//! [`GenerationDraft::prepare`] resolves every mounted provider and
//! credential, binds auth, and returns a [`GenerationBuild`] ready to
//! publish. The single mutation lease means every batch is followed by
//! exactly one load/prepare/activate pass, so the draft carries no staging
//! API of its own.

mod auth_fingerprint;
mod credential_codec;
mod refresh_sink;
mod revocation;

pub(crate) use revocation::{PreparedCredentialRevocation, prepare_credential_revocation};

use anyhow::Context as _;
use auth_fingerprint::{auth_fingerprint, earlier_auth_fingerprint};
use credential_codec::{CredentialPayload, decode_payload, encode_payload};
use omnifs_api::{
    CredentialClientOverrides, CredentialMaterial, CredentialSubmission, SecretBytes,
};
use omnifs_auth::{
    AuthBinding, AuthKind, AuthScheme, CredentialEntry, CredentialId, CredentialService,
    DurableCredentialSnapshot, OAuthClient, OAuthRequest, OAuthRuntimeOverrides, RefreshSink,
};
use omnifs_core::{
    AuthRuntimeFingerprint, CredentialGeneration, CredentialVersion, MountRevision, ProviderId,
};
use omnifs_engine::{
    CredentialProvenance, GenerationProvenance, HostOnline, MountBuildInput, MountBuildState,
    MountProvenance, MountTable, PreparedGeneration, ProviderBuildInput, PublishReadyGeneration,
    RuntimeMountConfig,
};
use omnifs_state::{
    CredentialDocument, CredentialState, SecretMaterial, StateStore, StoredCredential, StoredMount,
    StoredProvider,
};
use refresh_sink::StateRefreshSink;
use secrecy::SecretString;
use std::collections::HashMap;
use std::sync::Arc;
use time::OffsetDateTime;

/// The durable serving snapshot as of `load`: every active mount and
/// credential a fresh generation should be built from. A mutation batch
/// commits durably first; `load` then re-reads the result, so the draft
/// always reflects the batch's outcome rather than staging it separately.
pub(crate) struct GenerationDraft {
    revision: MountRevision,
    mounts: Vec<StoredMount>,
    credentials: Vec<CredentialRuntime>,
    pending_refreshes: Vec<PendingRefresh>,
}

impl GenerationDraft {
    pub(crate) async fn load(state: &StateStore) -> anyhow::Result<Self> {
        let durable = state.serving_snapshot().await?;
        let mut credentials = Vec::new();
        let mut pending_refreshes = Vec::new();
        for credential in durable.credentials {
            match credential.summary.state {
                CredentialState::Active => {},
                CredentialState::PendingRepublish => {
                    pending_refreshes.push(PendingRefresh {
                        id: credential.summary.id.clone(),
                        version: credential.summary.version,
                        generation: credential.summary.generation,
                    });
                },
                CredentialState::Blocked
                | CredentialState::RevocationPending
                | CredentialState::RevocationUnknown
                | CredentialState::Deleted => continue,
            }
            credentials.push(CredentialRuntime::from_stored(credential)?);
        }
        Ok(Self {
            revision: durable.revision,
            mounts: durable.mounts,
            credentials,
            pending_refreshes,
        })
    }

    pub(crate) fn has_pending_refreshes(&self) -> bool {
        !self.pending_refreshes.is_empty()
    }

    /// The mounts this draft was loaded with, so a caller can check whether
    /// any of them pins a given provider before deciding to rebuild.
    pub(crate) fn mounts(&self) -> &[StoredMount] {
        &self.mounts
    }

    pub(crate) fn provenance(&self) -> GenerationProvenance {
        GenerationProvenance::new(
            self.revision,
            self.mounts
                .iter()
                .map(|mount| MountProvenance {
                    name: mount.document.name.clone(),
                    version: mount.version,
                })
                .collect(),
            self.credentials
                .iter()
                .map(|credential| CredentialProvenance {
                    id: credential.id.clone(),
                    version: credential.version,
                    generation: credential.generation,
                })
                .collect(),
        )
    }

    /// Resolve every mounted provider and credential, bind auth, and build a
    /// complete durable `MountTable` generation.
    ///
    /// A provider store failure fails preparation outright: it is
    /// indistinguishable in principle from any other durable-state failure
    /// and must not be reported as "this mount's provider is unretained".
    /// Likewise, a `build_auth_binding` failure fails preparation rather
    /// than degrading the mount to `AuthRequired`. A mount whose credential
    /// lookup finds nothing, or finds one whose auth runtime fingerprint is
    /// not the one of the auth scheme of its provider, degrades alone to
    /// `MountBuildState::AuthRequired`: a missing or stale credential.
    pub(crate) async fn prepare(
        self,
        state: &Arc<StateStore>,
        host: &HostOnline,
    ) -> anyhow::Result<GenerationBuild> {
        let provenance = self.provenance();
        let Self {
            mounts,
            credentials: draft_credentials,
            pending_refreshes,
            ..
        } = self;

        let mut providers: HashMap<ProviderId, Result<LoadedProvider, String>> = HashMap::new();
        for mount in &mounts {
            let id = mount.document.provider.id;
            if let std::collections::hash_map::Entry::Vacant(entry) = providers.entry(id) {
                let provider = state
                    .load_provider(id)
                    .await
                    .with_context(|| format!("load provider {id}"))?;
                entry.insert(match provider {
                    Some(provider) => LoadedProvider::try_from(provider),
                    None => Err("its provider artifact is not retained".to_owned()),
                });
            }
        }

        let mut credentials = HashMap::new();
        let mut durable_snapshots = Vec::new();
        for runtime in draft_credentials {
            durable_snapshots.push((
                runtime.id.clone(),
                DurableCredentialSnapshot {
                    entry: runtime.entry.clone(),
                    version: runtime.version,
                },
            ));
            credentials.insert(runtime.id.clone(), runtime);
        }
        let credentials = Arc::new(credentials);
        let refresh_sink: Arc<dyn RefreshSink> = Arc::new(StateRefreshSink::new(
            Arc::clone(state),
            Arc::clone(&credentials),
        ));
        let service = Arc::new(CredentialService::new(
            durable_snapshots,
            OAuthClient::new()?,
            refresh_sink,
        ));

        let mut inputs = Vec::with_capacity(mounts.len());
        for mount in mounts {
            let provider = providers
                .get(&mount.document.provider.id)
                .expect("every mount provider was loaded")
                .as_ref();
            inputs.push(build_mount_input(mount, provider, &credentials, &service)?);
        }
        let table = Arc::new(MountTable::prepare_durable(host, inputs)?);
        Ok(GenerationBuild {
            generation: PreparedGeneration::new(
                table,
                tokio::runtime::Handle::current(),
                provenance,
            ),
            pending_refreshes: PendingRefreshes(pending_refreshes),
        })
    }
}

struct PendingRefresh {
    id: CredentialId,
    version: CredentialVersion,
    generation: CredentialGeneration,
}

/// A durable serving generation built from a [`GenerationDraft`], plus the
/// credentials awaiting activation once it publishes.
pub(crate) struct GenerationBuild {
    generation: PreparedGeneration,
    pending_refreshes: PendingRefreshes,
}

/// The named pieces of a [`GenerationBuild`], split apart at the point a
/// caller is ready to publish.
pub(crate) struct GenerationParts {
    pub(crate) ready: PublishReadyGeneration,
    pub(crate) revision: MountRevision,
    pub(crate) pending_refreshes: PendingRefreshes,
}

impl GenerationBuild {
    pub(crate) fn into_parts(self) -> GenerationParts {
        let revision = self.generation.provenance().revision();
        GenerationParts {
            ready: self.generation.activate(),
            revision,
            pending_refreshes: self.pending_refreshes,
        }
    }
}

/// Credentials that finished a refresh while `PendingRepublish` and now need
/// to be marked active now that the generation carrying them has published.
pub(crate) struct PendingRefreshes(Vec<PendingRefresh>);

impl PendingRefreshes {
    pub(crate) async fn activate(self, state: &StateStore) -> anyhow::Result<()> {
        for pending in self.0 {
            state
                .activate_refreshed_credential(pending.id, pending.version, pending.generation)
                .await
                .context("activate refreshed credential")?;
        }
        Ok(())
    }
}

pub(crate) async fn prepare_credential_document(
    state: &StateStore,
    submission: CredentialSubmission,
) -> anyhow::Result<CredentialDocument> {
    let provider = state
        .load_provider(submission.provider)
        .await?
        .with_context(|| format!("provider {} is not retained", submission.provider))?;
    let id = CredentialId::new(
        provider.reference.meta.name.to_string(),
        submission.scheme.clone(),
        submission.account_label.clone(),
    )?;
    let payload = CredentialPayload {
        material: submission.material,
        overrides: submission.overrides,
    };
    let manifest = provider
        .manifest
        .as_ref()
        .map_err(|error| anyhow::anyhow!("provider manifest is invalid: {error}"))?;
    let fingerprint = auth_fingerprint(
        submission.provider,
        manifest
            .auth
            .as_ref()
            .and_then(|manifest| manifest.scheme(&submission.scheme))
            .with_context(|| format!("provider declares no auth scheme `{}`", submission.scheme))?,
        &payload.overrides,
    )?;
    let kind = validate_payload(manifest, &submission.scheme, &payload)?;
    let scopes = material_scopes(&payload.material);
    let material = encode_payload(&payload)?;
    Ok(CredentialDocument {
        id,
        provider: submission.provider,
        kind,
        auth_fingerprint: fingerprint,
        scopes,
        material: SecretMaterial::new(material),
    })
}

struct CredentialRuntime {
    id: CredentialId,
    provider: ProviderId,
    kind: AuthKind,
    fingerprint: AuthRuntimeFingerprint,
    version: CredentialVersion,
    generation: CredentialGeneration,
    entry: CredentialEntry,
    overrides: Arc<CredentialClientOverrides>,
}

impl CredentialRuntime {
    fn from_stored(stored: StoredCredential) -> anyhow::Result<Self> {
        let payload = decode_payload(stored.material.expose())?;
        let entry = decode_entry(&payload.material)?;
        Ok(Self {
            id: stored.summary.id,
            provider: stored.summary.provider,
            kind: stored.summary.kind,
            fingerprint: stored.summary.auth_fingerprint,
            version: stored.summary.version,
            generation: stored.summary.generation,
            entry,
            overrides: Arc::new(payload.overrides),
        })
    }
}

struct LoadedProvider {
    manifest: omnifs_provider::ProviderManifest,
    bytes: Arc<[u8]>,
}

impl TryFrom<StoredProvider> for LoadedProvider {
    type Error = String;

    /// The provider of a valid manifest; the error of an invalid one.
    fn try_from(provider: StoredProvider) -> Result<Self, String> {
        Ok(Self {
            manifest: provider
                .manifest
                .map_err(|error| format!("its provider manifest is invalid: {error}"))?,
            bytes: Arc::from(provider.bytes.into_boxed_slice()),
        })
    }
}

/// The facts of a stored credential that its status shows, none of them a
/// secret: the scopes that the upstream granted to an OAuth credential, and
/// the subject that its client asserts in place of itself.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct CredentialFacts {
    pub(crate) scopes: Vec<String>,
    pub(crate) subject: Option<String>,
}

/// The facts of `stored`, none for a deleted credential, which has no
/// material.
///
/// Credential material stays daemon-owned. Callers receive only these narrow
/// presentation facts, never the access or refresh token.
pub(crate) fn credential_facts(stored: &StoredCredential) -> anyhow::Result<CredentialFacts> {
    if stored.summary.state == CredentialState::Deleted {
        return Ok(CredentialFacts::default());
    }
    let payload = decode_payload(stored.material.expose())?;
    Ok(CredentialFacts {
        scopes: material_scopes(&payload.material),
        subject: payload.overrides.subject,
    })
}

/// The scopes that the upstream granted to the stored OAuth credential.
pub(crate) fn credential_scopes(stored: &StoredCredential) -> anyhow::Result<Vec<String>> {
    credential_facts(stored).map(|facts| facts.scopes)
}

/// The input of a mount: unavailable with the error of its provider, whose
/// artifact is not retained or whose manifest is invalid.
fn build_mount_input(
    mount: StoredMount,
    provider: Result<&LoadedProvider, &String>,
    credentials: &HashMap<CredentialId, CredentialRuntime>,
    service: &Arc<CredentialService>,
) -> anyhow::Result<MountBuildInput> {
    let canonical = Arc::from(mount.canonical_without_artifact()?.into_boxed_slice());
    let config = RuntimeMountConfig {
        name: mount.document.name.clone(),
        provider: mount.document.provider.clone(),
        config: mount.document.config,
        max_fetch_blob_bytes: mount
            .document
            .limits
            .and_then(|limits| limits.max_fetch_blob_bytes),
    };
    let provider = match provider {
        Ok(provider) => provider,
        Err(reason) => {
            return Ok(MountBuildInput {
                config,
                canonical,
                provider: None,
                state: MountBuildState::ProviderUnavailable {
                    reason: reason.clone(),
                },
            });
        },
    };
    let provider_input = Some(ProviderBuildInput {
        bytes: Arc::clone(&provider.bytes),
        manifest: provider.manifest.clone(),
    });
    let state = match mount.document.credential.as_ref() {
        None => MountBuildState::Active {
            auth: None,
            credential_generation: None,
        },
        Some(id) => {
            // A credential is bound to a mount with the auth scheme of its
            // issue alone: it stays bound on another artifact of its
            // provider with that scheme, and is stale without it, as one
            // whose fingerprint an earlier omnifs took otherwise.
            let bound = match credentials.get(id) {
                Some(credential) if issued_scheme(provider, credential)? => Some(credential),
                _ => None,
            };
            match bound {
                None => MountBuildState::AuthRequired,
                Some(credential) => {
                    let auth = build_auth_binding(provider, credential, service)?;
                    MountBuildState::Active {
                        auth: Some(auth),
                        credential_generation: Some(credential.generation),
                    }
                },
            }
        },
    };
    Ok(MountBuildInput {
        config,
        canonical,
        provider: provider_input,
        state,
    })
}

/// Whether `provider` declares the auth scheme that `credential` was issued
/// against with the artifact of its issue: by the fingerprint of this
/// omnifs, or by the one that an earlier omnifs stored with it.
fn issued_scheme(
    provider: &LoadedProvider,
    credential: &CredentialRuntime,
) -> anyhow::Result<bool> {
    let Some(scheme) = provider
        .manifest
        .auth
        .as_ref()
        .and_then(|manifest| manifest.scheme(credential.id.scheme()))
    else {
        return Ok(false);
    };
    Ok(
        auth_fingerprint(credential.provider, scheme, &credential.overrides)?
            == credential.fingerprint
            || earlier_auth_fingerprint(credential.provider, scheme, &credential.overrides)?
                == credential.fingerprint,
    )
}

fn build_auth_binding(
    provider: &LoadedProvider,
    credential: &CredentialRuntime,
    service: &Arc<CredentialService>,
) -> anyhow::Result<Arc<AuthBinding>> {
    let scheme = provider
        .manifest
        .auth
        .as_ref()
        .and_then(|manifest| manifest.scheme(credential.id.scheme()))
        .context("credential scheme is absent from the pinned provider")?;
    let binding = match (scheme, credential.kind) {
        (AuthScheme::StaticToken(scheme), AuthKind::StaticToken) => service.bind_static(
            credential.id.clone(),
            scheme.inject_domains.clone(),
            scheme
                .header_name
                .clone()
                .unwrap_or_else(|| "Authorization".to_owned()),
            scheme.value_prefix.clone(),
        )?,
        (AuthScheme::Oauth(scheme), AuthKind::OAuth) => {
            let request = OAuthRequest::from_runtime(
                scheme.clone(),
                runtime_overrides(&credential.overrides)?,
            )?;
            service.bind_oauth(
                credential.id.clone(),
                request,
                scheme.inject_domains.clone(),
                scheme
                    .inject_header_name
                    .clone()
                    .unwrap_or_else(|| "Authorization".to_owned()),
                scheme.inject_value_prefix.clone(),
            )?
        },
        _ => anyhow::bail!("credential kind does not match its provider scheme"),
    };
    Ok(Arc::new(binding))
}

fn validate_payload(
    manifest: &omnifs_provider::ProviderManifest,
    scheme_key: &str,
    payload: &CredentialPayload,
) -> anyhow::Result<AuthKind> {
    let scheme = manifest
        .auth
        .as_ref()
        .and_then(|manifest| manifest.scheme(scheme_key))
        .with_context(|| format!("provider declares no auth scheme `{scheme_key}`"))?;
    let kind = classify_material(&payload.material);
    match (scheme, kind) {
        (AuthScheme::StaticToken(_), AuthKind::StaticToken) => {
            anyhow::ensure!(
                no_overrides(&payload.overrides),
                "static-token credentials do not accept OAuth overrides"
            );
        },
        (AuthScheme::Oauth(scheme), AuthKind::OAuth) => {
            OAuthRequest::from_runtime(scheme.clone(), runtime_overrides(&payload.overrides)?)?;
        },
        _ => anyhow::bail!("credential material does not match provider auth scheme"),
    }
    Ok(kind)
}

fn classify_material(material: &CredentialMaterial) -> AuthKind {
    match material {
        CredentialMaterial::StaticToken { .. } => AuthKind::StaticToken,
        CredentialMaterial::OAuth { .. } => AuthKind::OAuth,
    }
}

fn material_scopes(material: &CredentialMaterial) -> Vec<String> {
    match material {
        CredentialMaterial::StaticToken { .. } => Vec::new(),
        CredentialMaterial::OAuth { scopes, .. } => scopes.clone(),
    }
}

fn decode_entry(material: &CredentialMaterial) -> anyhow::Result<CredentialEntry> {
    Ok(match material {
        CredentialMaterial::StaticToken { token } => {
            CredentialEntry::static_token(secret_string(token)?)
        },
        CredentialMaterial::OAuth {
            access_token,
            refresh_token,
            expires_at_unix,
            token_type,
            scopes,
            upstream_identity,
        } => {
            let expires_at = expires_at_unix
                .map(OffsetDateTime::from_unix_timestamp)
                .transpose()
                .context("credential expiry is outside the supported timestamp range")?;
            let mut entry = CredentialEntry::oauth(
                secret_string(access_token)?,
                refresh_token.as_ref().map(secret_string).transpose()?,
                expires_at,
                token_type,
                scopes.clone(),
            );
            entry.set_upstream_identity(upstream_identity.clone());
            entry
        },
    })
}

fn secret_string(secret: &SecretBytes) -> anyhow::Result<SecretString> {
    Ok(SecretString::from(
        std::str::from_utf8(secret.expose())
            .context("credential token is not UTF-8")?
            .to_owned(),
    ))
}

fn runtime_overrides(
    overrides: &CredentialClientOverrides,
) -> anyhow::Result<OAuthRuntimeOverrides> {
    Ok(OAuthRuntimeOverrides {
        scopes: overrides.scopes.clone(),
        redirect_uri: overrides.redirect_uri.clone(),
        client_id: overrides.client_id.clone(),
        client_secret: overrides
            .client_secret
            .as_ref()
            .map(secret_string)
            .transpose()?,
        subject: overrides.subject.clone(),
    })
}

fn no_overrides(overrides: &CredentialClientOverrides) -> bool {
    overrides.client_id.is_none()
        && overrides.client_secret.is_none()
        && overrides.redirect_uri.is_none()
        && overrides.scopes.is_none()
        && overrides.subject.is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_auth::{RefreshCandidate, RefreshPersistError, RefreshPersistence};
    use omnifs_core::{MountName, MountVersion, MutationId, ProviderMeta, ProviderName};
    use std::future::Future;
    use std::pin::Pin;

    struct NoRefresh;

    impl RefreshSink for NoRefresh {
        fn persist<'a>(
            &'a self,
            _candidate: RefreshCandidate,
        ) -> Pin<
            Box<dyn Future<Output = Result<RefreshPersistence, RefreshPersistError>> + Send + 'a>,
        > {
            unreachable!("a static token has no refresh")
        }
    }

    fn provider() -> LoadedProvider {
        let manifest = serde_json::to_vec(&serde_json::json!({
            "id": "demo",
            "displayName": "Demo",
            "provider": "demo.wasm",
            "defaultMount": "demo",
            "projection": 1,
            "refreshIntervalSecs": 0,
            "capabilities": [{
                "kind": "domain",
                "value": "api.demo.test",
                "why": "Test credential injection."
            }],
            "auth": {
                "default": "pat",
                "schemes": [{
                    "staticToken": {
                        "key": "pat",
                        "valuePrefix": "Bearer ",
                        "description": "Demo token",
                        "injectDomains": ["api.demo.test"]
                    }
                }]
            }
        }))
        .unwrap();
        LoadedProvider {
            manifest: omnifs_provider::ProviderManifest::from_bytes(&manifest).unwrap(),
            bytes: Arc::from(Vec::new().into_boxed_slice()),
        }
    }

    fn mount(credential: &CredentialId) -> StoredMount {
        StoredMount {
            document: omnifs_state::MountDocument {
                name: MountName::new("demo").unwrap(),
                provider: omnifs_core::ProviderRef {
                    id: ProviderId::from_digest([7; 32]),
                    meta: ProviderMeta {
                        name: ProviderName::new("demo").unwrap(),
                        version: None,
                    },
                },
                credential: Some(credential.clone()),
                limits: None,
                config: serde_json::json!({}),
            },
            canonical: Vec::new(),
            version: MountVersion::from_digest([1; 32]),
            revision: MountRevision::new(1),
            last_mutation_id: MutationId::from_bytes([1; 16]),
        }
    }

    type Fingerprint = fn(
        ProviderId,
        &AuthScheme,
        &CredentialClientOverrides,
    ) -> anyhow::Result<AuthRuntimeFingerprint>;

    /// The state of a mount of `provider` with a stored credential of
    /// `kind` for its scheme `scheme`, with `overrides` and with the
    /// fingerprint that `fingerprint` gives for them.
    fn state_of_mount(
        provider: Result<&LoadedProvider, &String>,
        scheme: &AuthScheme,
        kind: AuthKind,
        overrides: CredentialClientOverrides,
        fingerprint: Fingerprint,
    ) -> MountBuildState {
        let key = scheme.key().unwrap();
        let mount = mount(&CredentialId::new("demo", key, "default").unwrap());
        let id = mount.document.credential.clone().unwrap();
        let provider_id = mount.document.provider.id;
        let entry = match kind {
            AuthKind::StaticToken => {
                CredentialEntry::static_token(SecretString::from("token".to_owned()))
            },
            AuthKind::OAuth => CredentialEntry::oauth(
                SecretString::from("access".to_owned()),
                None,
                None,
                "Bearer",
                Vec::new(),
            ),
        };
        let runtime = CredentialRuntime {
            id: id.clone(),
            provider: provider_id,
            kind,
            fingerprint: fingerprint(provider_id, scheme, &overrides).unwrap(),
            version: CredentialVersion::initial(),
            generation: CredentialGeneration::initial(),
            entry: entry.clone(),
            overrides: Arc::new(overrides),
        };
        let service = Arc::new(CredentialService::new(
            [(
                id.clone(),
                DurableCredentialSnapshot {
                    entry,
                    version: runtime.version,
                },
            )],
            OAuthClient::new().unwrap(),
            Arc::new(NoRefresh),
        ));
        let credentials = HashMap::from([(id.clone(), runtime)]);
        build_mount_input(mount, provider, &credentials, &service)
            .unwrap()
            .state
    }

    fn no_overrides_of_a_token() -> CredentialClientOverrides {
        CredentialClientOverrides {
            client_id: None,
            client_secret: None,
            redirect_uri: None,
            scopes: None,
            subject: None,
        }
    }

    /// The state of a mount of `provider()` with a stored token whose
    /// fingerprint `fingerprint` gives.
    fn state_with_fingerprint(fingerprint: Fingerprint) -> MountBuildState {
        let provider = provider();
        let scheme = provider
            .manifest
            .auth
            .as_ref()
            .unwrap()
            .scheme("pat")
            .unwrap();
        state_of_mount(
            Ok(&provider),
            scheme,
            AuthKind::StaticToken,
            no_overrides_of_a_token(),
            fingerprint,
        )
    }

    /// The subject of a stored credential is the one of its OAuth request,
    /// and no override of a static token.
    #[test]
    fn the_subject_of_a_credential_is_the_subject_of_its_oauth_request() {
        let overrides = CredentialClientOverrides {
            client_id: None,
            client_secret: None,
            redirect_uri: None,
            scopes: None,
            subject: Some("ada@example.test".to_owned()),
        };
        assert_eq!(
            runtime_overrides(&overrides).unwrap().subject.as_deref(),
            Some("ada@example.test")
        );
        assert!(!no_overrides(&overrides));
    }

    /// A credential whose fingerprint is not the one of the auth scheme of
    /// the provider of its mount leaves that mount alone without a
    /// credential.
    #[test]
    fn a_credential_of_another_fingerprint_leaves_its_mount_alone_without_one() {
        assert!(matches!(
            state_with_fingerprint(auth_fingerprint),
            MountBuildState::Active { auth: Some(_), .. }
        ));
        assert!(matches!(
            state_with_fingerprint(|_, _, _| Ok(AuthRuntimeFingerprint::from_digest([9; 32]))),
            MountBuildState::AuthRequired
        ));
    }

    /// A credential that an earlier omnifs stored, with the fingerprint of
    /// that omnifs for the auth scheme of the provider of its mount, stays
    /// bound to its mount: a new omnifs leaves no mount without its
    /// credential.
    #[test]
    fn a_credential_of_the_fingerprint_of_an_earlier_omnifs_stays_bound() {
        assert!(matches!(
            state_with_fingerprint(earlier_auth_fingerprint),
            MountBuildState::Active { auth: Some(_), .. }
        ));
    }

    /// The manifest of an artifact of an earlier omnifs, whose flow
    /// `jwtBearer` had the scopes in a claim and whose scheme had to declare
    /// an authorization endpoint.
    fn earlier_jwt_bearer_manifest() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "id": "demo",
            "displayName": "Demo",
            "provider": "demo.wasm",
            "defaultMount": "demo",
            "projection": 1,
            "refreshIntervalSecs": 0,
            "capabilities": [{
                "kind": "domain",
                "value": "api.demo.test",
                "why": "Test credential injection."
            }],
            "auth": {
                "default": "service-account",
                "schemes": [{
                    "oauth": {
                        "key": "service-account",
                        "displayName": "Demo service account",
                        "authorizationEndpoint": "https://auth.demo.test/authorize",
                        "tokenEndpoint": "https://auth.demo.test/token",
                        "defaultScopes": ["read"],
                        "flow": "jwtBearer",
                        "injectDomains": ["api.demo.test"],
                        "injectValuePrefix": "Bearer "
                    }
                }],
                "guidance": {
                    "service-account": { "setupSteps": ["Pass the client and its key."] }
                }
            }
        }))
        .unwrap()
    }

    /// A mount on an artifact of an earlier omnifs whose flow `jwtBearer`
    /// had another meaning is unavailable with the error of its manifest,
    /// which declares the authorization endpoint that the flow had to have
    /// then: it does not look ready with the credential of that omnifs, to
    /// fail at its next assertion.
    #[test]
    fn a_mount_of_an_artifact_of_the_earlier_jwt_bearer_grant_is_unavailable() {
        let document = earlier_jwt_bearer_manifest();
        let stored = StoredProvider {
            reference: omnifs_core::ProviderRef {
                id: ProviderId::from_digest([7; 32]),
                meta: ProviderMeta {
                    name: ProviderName::new("demo").unwrap(),
                    version: None,
                },
            },
            manifest: omnifs_provider::ProviderManifest::from_bytes(&document)
                .map_err(|error| error.to_string()),
            bytes: Vec::new(),
        };
        let provider = LoadedProvider::try_from(stored);
        // The scheme as that omnifs read it, for the fingerprint of its
        // credential.
        let earlier: omnifs_provider::ProviderManifest = serde_json::from_slice(&document).unwrap();
        let scheme = earlier
            .auth
            .as_ref()
            .unwrap()
            .scheme("service-account")
            .unwrap();
        let state = state_of_mount(
            provider.as_ref(),
            scheme,
            AuthKind::OAuth,
            CredentialClientOverrides {
                client_id: Some("robot@demo.test".to_owned()),
                client_secret: Some(SecretBytes::new(b"key".to_vec())),
                redirect_uri: None,
                scopes: None,
                subject: None,
            },
            earlier_auth_fingerprint,
        );
        let MountBuildState::ProviderUnavailable { reason } = state else {
            panic!("the mount is not unavailable");
        };
        assert_eq!(
            reason,
            "its provider manifest is invalid: invalid provider metadata: auth.schemes.service-account.authorizationEndpoint must not be declared for the flow of a JWT bearer grant, which has no authorization request"
        );
    }
}
