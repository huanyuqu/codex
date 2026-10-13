//! Finds the Responses endpoint and API key to evaluate with, from flags, the environment, or a
//! Codex home directory.

use std::path::Path;

use anyhow::Context as _;
use serde::Deserialize;

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// Where requests go and how they authenticate.
#[derive(Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub base_url: String,
    pub api_key: String,
}

impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize, Default)]
struct CodexConfig {
    model_provider: Option<String>,
    #[serde(default)]
    model_providers: std::collections::HashMap<String, ProviderConfig>,
}

#[derive(Deserialize, Default)]
struct ProviderConfig {
    base_url: Option<String>,
    env_key: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
struct AuthFile {
    openai_api_key: Option<String>,
}

/// Resolves the endpoint. The base URL comes from `base_url`, then the active model provider in
/// `codex_home/config.toml`, then `OPENAI_BASE_URL`. The key comes from `api_key_env`, then the
/// provider's `env_key`, then the API key stored in `codex_home/auth.json`.
pub fn resolve_endpoint(
    codex_home: &Path,
    base_url: Option<String>,
    api_key_env: &str,
) -> anyhow::Result<Endpoint> {
    let config: CodexConfig = std::fs::read_to_string(codex_home.join("config.toml"))
        .ok()
        .and_then(|text| toml::from_str(&text).ok())
        .unwrap_or_default();
    let provider = config
        .model_provider
        .as_ref()
        .and_then(|id| config.model_providers.get(id));

    let base_url = base_url
        .or_else(|| provider.and_then(|provider| provider.base_url.clone()))
        .or_else(|| std::env::var("OPENAI_BASE_URL").ok())
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());

    let from_env = |name: &str| std::env::var(name).ok().filter(|key| !key.is_empty());
    let api_key = from_env(api_key_env)
        .or_else(|| {
            provider
                .and_then(|provider| provider.env_key.as_deref())
                .and_then(from_env)
        })
        .or_else(|| {
            std::fs::read_to_string(codex_home.join("auth.json"))
                .ok()
                .and_then(|text| serde_json::from_str::<AuthFile>(&text).ok())
                .and_then(|auth| auth.openai_api_key)
        })
        .with_context(|| {
            format!(
                "no API key: set {api_key_env} or log in to Codex with an API key in {}",
                codex_home.display()
            )
        })?;
    Ok(Endpoint {
        base_url: base_url.trim_end_matches('/').to_string(),
        api_key,
    })
}
