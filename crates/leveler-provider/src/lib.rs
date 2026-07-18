//! `leveler-provider` — provider wiring and the concrete model runtime.
//!
//! Owns provider configuration (with env expansion), the model catalog,
//! HTTP transport with retry, the [`ProviderRegistry`] that implements
//! [`leveler_model::ModelRuntime`], and basic model probing.
#![forbid(unsafe_code)]

pub mod catalog;
pub mod config;
pub mod probe;
pub mod registry;
mod transport;

pub use catalog::{ModelConfigFile, load_model_config};
pub use config::{
    ConfigError, ProviderConfig, RetryConfig, Timeouts, expand_env, expand_env_with,
    load_provider_config, resolve_api_key,
};
pub use probe::{BasicProbeReport, probe_basic};
pub use registry::{ProviderRegistry, RegistryError, RegistryInputs};
