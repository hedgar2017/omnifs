//! Engine/instance/mount lifecycle for one WASM provider.
//!
//! `Runtime` manages the Wasmtime store lifetime, provider initialization,
//! executor handles (HTTP, Git, and Blob), and cache/mount lifecycle.
//! Typed operation execution is in `ops::lifecycle`; WASI store plumbing is in `wasi`.

use crate::authority::RuntimeAuthority;
use crate::blob::{BlobExecutor, BlobLimits};
use crate::cache::MountResources;
use crate::callouts::{CalloutHost, TestCallouts, TestSignal};
use crate::cloner::GitCloner;
use crate::git;
use crate::http::HttpStack;
use crate::instance::Instance;
use crate::invalidation::InvalidationState;
use crate::tree_refs::TreeRefs;
use omnifs_auth::{AuthBinding, CredentialHealth};
use omnifs_core::{MountName, ProviderId, ProviderRef};
use omnifs_provider::{ConfigMetadata, ProviderManifest};
use omnifs_wit::host::types as wit_types;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;

pub(crate) mod host;
pub(crate) mod instance;
pub(crate) mod registry;
pub(crate) mod wasi;
pub(crate) mod wasm;

#[allow(unused_imports)] // re-exported for callers via crate root
pub use host::{HostError, HostOnline, HostRuntimeOpen};

use crate::clock;
use crate::op_validate;
use crate::runtime::wasm::ComponentEngine;

pub(crate) const HTTP_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
// Host-side 429 cooldown when the provider error carried no Retry-After.
const RATE_LIMIT_DEFAULT_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(5);
// Upper bound so a hostile Retry-After cannot overflow `Instant` or wedge the
// window open indefinitely.
const RATE_LIMIT_MAX_COOLDOWN: std::time::Duration = std::time::Duration::from_hours(1);

/// Runtime for one mounted WASM provider component.
///
/// Manages the Wasmtime instance driver, host callout imports, cache state,
/// and operation id allocation.
pub struct Runtime {
    pub(crate) instance: Instance,
    pub(crate) mount_name: String,
    pub(crate) provider_name: String,
    provider_id: ProviderId,
    auth: Option<Arc<AuthBinding>>,
    next_operation_id: AtomicU64,
    pub resources: Arc<MountResources>,
    trees: Arc<TreeRefs>,
    pub(crate) invalidation: InvalidationState,
    pub(crate) namespace_flights: crate::ops::namespace::NamespaceFlights,
    pub(crate) feed_operations: Arc<crate::ops::feed::FeedOperations>,
    // Per-mount window, open until its instant with the error that opened
    // it, while the upstream of the mount limits its rate (from a 429's
    // Retry-After) or failed for now: an operation is served from what the
    // projection has until it closes. std Mutex: the critical section is a
    // single set/get with no await held across it.
    window: std::sync::Mutex<Option<(std::time::Instant, wit_types::ProviderError)>>,
    pub(crate) test_callouts: Option<std::sync::Mutex<mpsc::Receiver<TestSignal>>>,
    /// The blob executor of the mount, which answers a captured blob fetch
    /// of a test as a fetch.
    pub(crate) blob: BlobExecutor,
}

/// Validated state-neutral input for one provider runtime.
#[derive(Clone)]
pub struct RuntimeMountConfig {
    pub name: MountName,
    pub provider: ProviderRef,
    pub config: serde_json::Value,
    pub max_fetch_blob_bytes: Option<u64>,
}

struct RuntimeBuildInput<'a> {
    wasm: Arc<[u8]>,
    config: &'a RuntimeMountConfig,
    manifest: &'a ProviderManifest,
    auth: Option<Arc<AuthBinding>>,
    resources: Arc<MountResources>,
    trees: Arc<TreeRefs>,
    publish_initialize_effects: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("wasmtime: {0}")]
    Wasmtime(#[from] wasmtime::Error),
    #[error("http client: {0}")]
    HttpClient(#[from] reqwest::Error),
    #[error("invalid config: {0}")]
    InvalidConfig(String),
    #[error("authority: {0}")]
    Authority(#[from] crate::authority::AuthorityError),
    #[error("cache: {0}")]
    Cache(String),
    #[error("provider protocol: {0}")]
    ProviderProtocol(String),
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("wasmtime: {0}")]
    Wasmtime(#[from] wasmtime::Error),
    #[error("provider protocol: {0}")]
    ProviderProtocol(String),
    #[error("provider returned error: {0:?}")]
    ProviderError(wit_types::ProviderError),
}

pub(crate) type Result<T> = std::result::Result<T, EngineError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderErrorClass {
    NotFound,
    NotDirectory,
    IsDirectory,
    /// The upstream took no credential of the request, as a `401`.
    Unauthenticated,
    PermissionDenied,
    InvalidInput,
    TooLarge,
    RateLimited,
    Timeout,
    Network,
    Internal,
}

impl EngineError {
    pub(crate) fn provider_class(&self) -> Option<ProviderErrorClass> {
        let Self::ProviderError(error) = self else {
            return None;
        };
        Some(error.kind.into())
    }

    pub(crate) fn is_provider_rate_limited(&self) -> bool {
        self.provider_class() == Some(ProviderErrorClass::RateLimited)
    }

    pub(crate) fn is_provider_not_found_or_invalid_input(&self) -> bool {
        matches!(
            self.provider_class(),
            Some(ProviderErrorClass::NotFound | ProviderErrorClass::InvalidInput)
        )
    }
}

impl From<wit_types::ErrorKind> for ProviderErrorClass {
    fn from(kind: wit_types::ErrorKind) -> Self {
        match kind {
            wit_types::ErrorKind::NotFound => Self::NotFound,
            wit_types::ErrorKind::NotADirectory => Self::NotDirectory,
            wit_types::ErrorKind::NotAFile => Self::IsDirectory,
            wit_types::ErrorKind::PermissionDenied => Self::Unauthenticated,
            wit_types::ErrorKind::Denied => Self::PermissionDenied,
            wit_types::ErrorKind::InvalidInput => Self::InvalidInput,
            wit_types::ErrorKind::TooLarge => Self::TooLarge,
            wit_types::ErrorKind::RateLimited => Self::RateLimited,
            wit_types::ErrorKind::Network => Self::Network,
            wit_types::ErrorKind::Timeout => Self::Timeout,
            wit_types::ErrorKind::VersionMismatch | wit_types::ErrorKind::Internal => {
                Self::Internal
            },
        }
    }
}

impl From<EngineError> for BuildError {
    fn from(err: EngineError) -> Self {
        match err {
            EngineError::Wasmtime(e) => Self::Wasmtime(e),
            EngineError::ProviderProtocol(msg) => Self::ProviderProtocol(msg),
            EngineError::ProviderError(e) => {
                Self::ProviderProtocol(format!("provider error during build: {e:?}"))
            },
        }
    }
}

impl Runtime {
    #[must_use]
    pub fn mount_name(&self) -> &str {
        &self.mount_name
    }

    pub fn provider_name(&self) -> &str {
        &self.provider_name
    }

    pub fn provider_id(&self) -> ProviderId {
        self.provider_id
    }

    pub fn auth_health(&self) -> Option<CredentialHealth> {
        self.auth.as_ref().map(|binding| binding.health())
    }

    pub(crate) fn auth_binding(&self) -> Option<&Arc<AuthBinding>> {
        self.auth.as_ref()
    }

    #[allow(clippy::too_many_lines)]
    fn build(
        engine: &ComponentEngine,
        input: RuntimeBuildInput<'_>,
        cloner: Arc<GitCloner>,
        capture_test_callouts: bool,
    ) -> std::result::Result<Self, BuildError> {
        let RuntimeBuildInput {
            wasm,
            config,
            manifest,
            auth,
            resources,
            trees,
            publish_initialize_effects,
        } = input;
        let (test_callouts, test_rx) = if capture_test_callouts {
            let (test_callouts, rx) = TestCallouts::channel();
            (Some(test_callouts), Some(rx))
        } else {
            (None, None)
        };
        let mount_name = config.name.as_str();
        let config_bytes = serde_json::to_vec(&config.config)
            .map_err(|error| BuildError::InvalidConfig(error.to_string()))?;
        let config_metadata = manifest.config.as_ref();

        validate_instance_config(config_metadata, config, mount_name)?;

        let authority = RuntimeAuthority::resolve(manifest, Some(&config.config))?;
        let park_signal = test_callouts.as_ref().map(TestCallouts::park_signal);
        let instance = Instance::new(
            engine,
            wasm,
            config_bytes,
            Arc::clone(&authority),
            park_signal,
        )?;

        let (init_result, initialize_effects) = instance.initialize().map_err(BuildError::from)?;
        op_validate::validate_initialize(&init_result, &initialize_effects, |_| false).map_err(
            |message| {
                BuildError::ProviderProtocol(format!(
                    "initialize returned invalid result: {message}"
                ))
            },
        )?;
        let initialize_effects = init_result
            .map(|()| initialize_effects)
            .map_err(EngineError::ProviderError)
            .map_err(BuildError::from)?;
        let git = git::GitExecutor::new(cloner, Arc::clone(&authority), trees.clone(), mount_name);

        let blob_limits = BlobLimits::from_max_fetch_bytes(config.max_fetch_blob_bytes);
        let http = Arc::new(HttpStack::new(auth.clone(), authority)?);
        let blob = BlobExecutor::new(Arc::clone(&http), Arc::clone(&resources), blob_limits);
        let mut callout_host = CalloutHost::new(Arc::clone(&http), git.clone(), blob.clone());
        if let Some(test_callouts) = test_callouts {
            callout_host = callout_host.with_test_callouts(test_callouts);
        }
        let feed_operations = Arc::new(crate::ops::feed::FeedOperations::default());
        instance
            .set_host(
                callout_host,
                crate::ops::feed::ProjectionReads::new(
                    Arc::clone(&resources),
                    Arc::clone(&feed_operations),
                ),
            )
            .map_err(BuildError::from)?;
        let runtime = Self {
            instance,
            mount_name: mount_name.to_string(),
            provider_name: config.provider.meta.name.to_string(),
            provider_id: config.provider.id,
            auth,
            next_operation_id: AtomicU64::new(1),
            resources,
            trees,
            invalidation: InvalidationState::default(),
            namespace_flights: crate::ops::namespace::NamespaceFlights::new(),
            feed_operations,
            window: std::sync::Mutex::new(None),
            test_callouts: test_rx.map(std::sync::Mutex::new),
            blob,
        };
        let transition = crate::effect_apply::EffectApplier::new(&runtime.resources)
            .lower_effects(&initialize_effects, clock::now_millis())
            .map_err(|error| BuildError::ProviderProtocol(error.to_string()))?;
        if publish_initialize_effects {
            runtime
                .publish_transition(transition, runtime.resources.current_epoch())
                .map_err(|error| BuildError::ProviderProtocol(error.to_string()))?;
        } else if !transition.is_empty() {
            return Err(BuildError::ProviderProtocol(
                "provider initialization must not mutate projection state".to_owned(),
            ));
        }
        Ok(runtime)
    }

    pub fn shutdown(&self) -> Result<()> {
        self.instance.shutdown()
    }

    pub fn call_close_file(&self, handle: u64) -> Result<()> {
        self.instance.close_file(handle)
    }

    /// Arm the mount's rate-limit window after a 429. `retry_after` is the
    /// provider error's structured Retry-After (seconds) if present.
    pub(crate) fn note_rate_limited(&self, retry_after: Option<std::time::Duration>) {
        let cooldown = retry_after
            .unwrap_or(RATE_LIMIT_DEFAULT_COOLDOWN)
            .min(RATE_LIMIT_MAX_COOLDOWN);
        self.open_window(
            cooldown,
            wit_types::ProviderError {
                kind: wit_types::ErrorKind::RateLimited,
                message: "the upstream limits the rate of the mount".into(),
                retryable: true,
                retry_after: None,
            },
        );
    }

    /// Arm the window of the mount after a check of a feed that failed for
    /// now with `error`, at the network, a timeout, or the upstream: the next
    /// check waits as long as after an answer.
    pub(crate) fn note_failed_for_now(&self, error: &wit_types::ProviderError) {
        self.open_window(
            std::time::Duration::from_millis(crate::clock::DYNAMIC_TTL_MILLIS),
            error.clone(),
        );
    }

    fn open_window(&self, cooldown: std::time::Duration, error: wit_types::ProviderError) {
        let until = std::time::Instant::now() + cooldown;
        let mut window = self.window.lock().unwrap();
        if window.as_ref().is_none_or(|(open, _)| *open < until) {
            *window = Some((until, error));
        }
    }

    /// The window of the mount, if currently open: the time until it closes
    /// and the error that opened it. Lazily clears an expired window.
    pub(crate) fn window(&self) -> Option<(std::time::Duration, wit_types::ProviderError)> {
        let mut guard = self.window.lock().unwrap();
        let now = std::time::Instant::now();
        match &*guard {
            Some((until, error)) if *until > now => Some((*until - now, error.clone())),
            Some(_) => {
                *guard = None;
                None
            },
            None => None,
        }
    }

    /// The instant the mount's window closes, if currently open.
    pub fn rate_limited_until(&self) -> Option<std::time::Instant> {
        self.window()
            .map(|(remaining, _)| std::time::Instant::now() + remaining)
    }

    pub(crate) fn next_operation_id(&self) -> u64 {
        self.next_operation_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Resolve a host-issued Git tree handle for the private namespace facade.
    pub(crate) fn tree_ref(&self, tree_ref: u64) -> Option<crate::tree_refs::TreeRef> {
        self.trees.resolve(tree_ref)
    }
}

fn validate_instance_config(
    metadata: Option<&ConfigMetadata>,
    config: &RuntimeMountConfig,
    mount_name: &str,
) -> std::result::Result<(), BuildError> {
    let Some(metadata) = metadata else {
        return Ok(());
    };

    match metadata.validate_config(&config.config) {
        Ok(()) => Ok(()),
        Err(error) => Err(BuildError::InvalidConfig(format!(
            "config for mount {mount_name} failed validation: {error}"
        ))),
    }
}
