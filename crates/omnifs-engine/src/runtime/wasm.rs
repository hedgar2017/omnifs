//! Shared Wasmtime component-engine ownership.

use std::path::Path;

use wasmtime::component::Component;
use wasmtime::{Cache, CacheConfig, Config, Engine};

/// The production Wasmtime engine used to load provider components.
#[derive(Clone)]
pub struct ComponentEngine {
    inner: Engine,
}

impl ComponentEngine {
    /// Create the production component engine.
    ///
    /// `cache_dir`, when present, stores Wasmtime's compiled artifacts with
    /// the profile, and a cache that cannot open there is the error of the
    /// engine: no compilation falls back to another place or to none.
    pub fn new(cache_dir: Option<&Path>) -> wasmtime::Result<Self> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        config.wasm_component_model_async(true);
        config.wasm_component_model_more_async_builtins(true);
        config.wasm_component_model_async_stackful(true);
        config.concurrency_support(true);
        if let Some(dir) = cache_dir {
            let mut cache_config = CacheConfig::new();
            cache_config.with_directory(dir);
            config.cache(Some(Cache::new(cache_config)?));
        }
        Ok(Self {
            inner: Engine::new(&config)?,
        })
    }

    /// Load one provider component through the production engine.
    pub fn load(&self, component_bytes: &[u8]) -> wasmtime::Result<Component> {
        Component::new(&self.inner, component_bytes)
    }

    pub(crate) fn inner(&self) -> &Engine {
        &self.inner
    }
}

#[cfg(test)]
mod cache_tests {
    use super::ComponentEngine;

    /// A cache that cannot open at its directory, as a directory under a
    /// file, is the error of the engine, not an engine without a cache.
    #[test]
    fn a_cache_that_cannot_open_is_the_error_of_the_engine() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, b"not a directory").unwrap();
        assert!(ComponentEngine::new(Some(&file.join("cache"))).is_err());
        assert!(ComponentEngine::new(Some(&dir.path().join("cache"))).is_ok());
    }
}
