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

use crate::op_validate;
use crate::runtime::wasm::ComponentEngine;

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
    // Per-mount window, open until the instant that the upstream of the
    // mount states for its rate limit, with the error that opened it. std
    // Mutex: the critical section is a single set/get with no await held
    // across it.
    window: std::sync::Mutex<Option<(std::time::Instant, wit_types::ProviderError)>>,
    // The feeds whose check failed, by the id of their object, with the
    // number of the sync of the failure and its error.
    failures: std::sync::Mutex<std::collections::HashMap<Vec<u8>, (u64, wit_types::ProviderError)>>,
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
            failures: std::sync::Mutex::new(std::collections::HashMap::new()),
            test_callouts: test_rx.map(std::sync::Mutex::new),
            blob,
        };
        let transition = crate::effect_apply::EffectApplier::new(&runtime.resources)
            .lower_effects(
                &initialize_effects,
                runtime
                    .resources
                    .due_in()
                    .map_err(|error| BuildError::ProviderProtocol(error.to_string()))?,
            )
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

    /// Open the window of the mount after an answer that limits its rate
    /// for the `retry_after` that its upstream states: within it, an
    /// operation is served from the projection, and one that lacks its fact
    /// has the error of the window without a request. An answer without a
    /// time of the upstream opens no window: the feed or the request that it
    /// failed is not asked again in the sync ([`Self::record_failure`]).
    pub(crate) fn note_rate_limited(&self, retry_after: Option<std::time::Duration>) {
        let Some(retry_after) = retry_after else {
            return;
        };
        let Some(until) = std::time::Instant::now().checked_add(retry_after) else {
            return;
        };
        let error = wit_types::ProviderError {
            kind: wit_types::ErrorKind::RateLimited,
            message: "the upstream limits the rate of the mount".into(),
            retryable: true,
            retry_after: None,
        };
        let mut window = self.window.lock().unwrap();
        if window.as_ref().is_none_or(|(open, _)| *open < until) {
            *window = Some((until, error));
        }
    }

    /// The window of the mount, if open: the time until the instant that its
    /// upstream stated, and the error that opened it. The wall clock is read
    /// to compare it with that instant alone.
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

    /// Record that the check of the feed of the object `id` failed with
    /// `error` in the last sync begun: the feed is not asked again until the
    /// next sync, and an operation that lacks its fact has the error
    /// without a request in place of a request of each retry.
    pub(crate) fn record_failure(&self, id: &[u8], error: &wit_types::ProviderError) -> Result<()> {
        let sync = self
            .resources
            .sync_count()
            .map_err(|error| EngineError::ProviderProtocol(error.to_string()))?;
        self.failures
            .lock()
            .unwrap()
            .insert(id.to_vec(), (sync, error.clone()));
        Ok(())
    }

    /// Let the failure `error` of an answer of the feed of the object `id`
    /// stand for the rest of the sync: each operation that needs a fact of
    /// the feed has it without a request, and the feed is asked again in
    /// the next sync. A rate limit with the time of its upstream holds the
    /// feed back by the window of the mount alone, after which the feed is
    /// asked once more in the same sync.
    pub(crate) fn stand_failure(&self, id: &[u8], error: &wit_types::ProviderError) -> Result<()> {
        if error.kind == wit_types::ErrorKind::RateLimited && error.retry_after.is_some() {
            return Ok(());
        }
        self.record_failure(id, error)?;
        self.resources
            .defer_feed(id)
            .map_err(|error| EngineError::ProviderProtocol(error.to_string()))
    }

    /// The error of the check of the feed of the object `id` that failed in
    /// the last sync begun, none for a feed that did not.
    pub(crate) fn failure(&self, id: &[u8]) -> Result<Option<wit_types::ProviderError>> {
        let sync = self
            .resources
            .sync_count()
            .map_err(|error| EngineError::ProviderProtocol(error.to_string()))?;
        Ok(self
            .failures
            .lock()
            .unwrap()
            .get(id)
            .filter(|(failed_in, _)| *failed_in == sync)
            .map(|(_, error)| error.clone()))
    }

    /// The number of the first sync in which a fact written now is due.
    pub(crate) fn due_in(&self) -> Result<u64> {
        self.resources
            .due_in()
            .map_err(|error| EngineError::ProviderProtocol(error.to_string()))
    }

    /// Begin a sync of the mount ([`crate::cache::MountResources::begin_sync`]),
    /// after which no failure of an earlier sync holds a request back.
    pub fn begin_sync(&self) -> Result<u64> {
        let sync = self
            .resources
            .begin_sync()
            .map_err(|error| EngineError::ProviderProtocol(error.to_string()))?;
        self.failures.lock().unwrap().clear();
        Ok(sync)
    }

    /// End the sync `number` of the mount.
    pub fn end_sync(&self, number: u64) -> Result<()> {
        self.resources
            .end_sync(number)
            .map_err(|error| EngineError::ProviderProtocol(error.to_string()))
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
