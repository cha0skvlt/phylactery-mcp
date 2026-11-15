//! Configuration resolution.
//!
//! The AOE registry (`~/.config/cha0s/settings.json`) is the single source of truth for the
//! database DSN and the Ollama endpoint. Environment variables override it. There is no second
//! `.env` file: a second source of truth is a second way to be wrong.

use std::path::PathBuf;

use serde_json::Value;

pub const DEFAULT_OLLAMA_URL: &str = "http://127.0.0.1:11434";
pub const DEFAULT_EMBED_URL: &str = DEFAULT_OLLAMA_URL;
pub const DEFAULT_EMBED_MODEL: &str = "nomic-embed-text-v2-moe";
pub const DEFAULT_AGENT_ID: &str = "agent";
pub const DEFAULT_POOL_MAX: usize = 8;
pub const DEFAULT_STATEMENT_TIMEOUT_MS: u64 = 10_000;
pub const EMBED_DIM: usize = 768;
pub const DEFAULT_BACKUP_DIR: &str = "backups";
pub const DEFAULT_MIGRATIONS_DIR: &str = "migrations";
pub const BACKUP_KEY: &str = "phylactery-auto";

/// Which embeddings API the endpoint speaks.
///
/// The server computes no vectors itself: it asks an external endpoint for them, and this is the
/// dialect it asks in. `Ollama` is the default because it is the one that runs on the same machine
/// as the database; `OpenAi` covers every cloud provider and every local server that speaks the
/// `/v1/embeddings` shape.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EmbedApi {
    /// `POST {url}/api/embed` → `{"embeddings": [[…]]}`
    #[default]
    Ollama,
    /// `POST {url}/v1/embeddings` → `{"data": [{"embedding": […]}]}`
    OpenAi,
}

impl EmbedApi {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "ollama" => Some(Self::Ollama),
            "openai" | "openai-compatible" | "v1" => Some(Self::OpenAi),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ollama => "ollama",
            Self::OpenAi => "openai",
        }
    }
}

/// Registry alias. One name, no historical variants: the registry entry is `phylactery`.
pub const REGISTRY_NAMES: [&str; 1] = ["phylactery"];

#[derive(Clone, Debug)]
pub struct Config {
    pub dsn: String,
    pub dsn_source: String,
    pub ollama_url: String,
    pub ollama_source: String,
    pub embed_api: EmbedApi,
    pub embed_url: String,
    pub embed_source: String,
    pub embed_api_key: Option<String>,
    pub embed_model: String,
    pub agent_id: String,
    pub pool_max: usize,
    pub statement_timeout_ms: u64,
    pub registry_path: PathBuf,
    pub backup_dir: PathBuf,
    pub migrations_dir: PathBuf,
    pub admin_dsn: Option<String>,
    /// Recipients for backup encryption. Two keys mean one lost key is survivable.
    pub backup_keys: Vec<String>,
    /// The graph covers memory written from this moment on. Older rows stay vectors only.
    pub kg_since: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("no DSN: set PHYLACTERY_DSN or agent.databases.<registry>.dsn in {0}")]
    MissingDsn(String),
}

impl Config {
    /// Resolve the registry path (`PHYLACTERY_SETTINGS` or the AOE default) and load from it.
    pub fn load() -> Result<Self, ConfigError> {
        let path = env_string("PHYLACTERY_SETTINGS")
            .map(PathBuf::from)
            .unwrap_or_else(registry_path);
        Self::load_from(path)
    }

    /// Load from an explicit registry file. Tests use this to stay hermetic.
    pub fn load_from(registry_path: PathBuf) -> Result<Self, ConfigError> {
        let registry = std::fs::read_to_string(&registry_path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .unwrap_or(Value::Null);

        let (dsn, dsn_source) = resolve_dsn(
            std::env::var("PHYLACTERY_DSN").ok(),
            &registry,
            &registry_path,
        )?;

        let (ollama_url, ollama_source) =
            resolve_ollama(std::env::var("PHYLACTERY_OLLAMA_URL").ok(), &registry);

        let (embed_url, embed_source) = resolve_embed_url(
            std::env::var("PHYLACTERY_EMBED_URL").ok(),
            std::env::var("PHYLACTERY_OLLAMA_URL").ok(),
            &registry,
        );
        let embed_api = env_string("PHYLACTERY_EMBED_API")
            .and_then(|value| EmbedApi::parse(&value))
            .unwrap_or_default();

        Ok(Self {
            dsn,
            dsn_source,
            ollama_url,
            ollama_source,
            embed_api,
            embed_url,
            embed_source,
            embed_api_key: env_string("PHYLACTERY_EMBED_API_KEY"),
            embed_model: env_string("PHYLACTERY_EMBED_MODEL")
                .unwrap_or_else(|| DEFAULT_EMBED_MODEL.to_string()),
            agent_id: env_string("PHYLACTERY_AGENT_ID")
                .unwrap_or_else(|| DEFAULT_AGENT_ID.to_string()),
            pool_max: env_parse("PHYLACTERY_POOL_MAX")
                .unwrap_or(DEFAULT_POOL_MAX)
                .max(1),
            statement_timeout_ms: env_parse("PHYLACTERY_STATEMENT_TIMEOUT_MS")
                .unwrap_or(DEFAULT_STATEMENT_TIMEOUT_MS)
                .max(100),
            registry_path,
            backup_dir: env_string("PHYLACTERY_BACKUP_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(DEFAULT_BACKUP_DIR)),
            migrations_dir: env_string("PHYLACTERY_MIGRATIONS_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(DEFAULT_MIGRATIONS_DIR)),
            admin_dsn: env_string("PHYLACTERY_ADMIN_DSN").or_else(|| {
                registry
                    .pointer("/agent/databases/phylactery/admin_dsn")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            }),
            backup_keys: env_string("PHYLACTERY_BACKUP_KEYS")
                .map(|value| {
                    value
                        .split(',')
                        .map(str::trim)
                        .filter(|key| !key.is_empty())
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .filter(|keys| !keys.is_empty())
                .unwrap_or_else(|| vec![BACKUP_KEY.to_string(), "phylactery-backup".to_string()]),
            kg_since: env_string("PHYLACTERY_KG_SINCE")
                .and_then(|value| chrono::DateTime::parse_from_rfc3339(&value).ok())
                .map(|stamp| stamp.with_timezone(&chrono::Utc))
                .unwrap_or_else(|| {
                    chrono::DateTime::parse_from_rfc3339("2026-11-14T00:00:00Z")
                        .expect("kg_since default")
                        .with_timezone(&chrono::Utc)
                }),
        })
    }

    /// DSN with the password masked, safe for logs and diagnostics.
    pub fn redacted_dsn(&self) -> String {
        let dsn = self.dsn.as_str();
        let Some(authority_start) = dsn.find("://").map(|index| index + 3) else {
            return dsn.to_string();
        };
        let rest = &dsn[authority_start..];
        let Some(at) = rest.find('@') else {
            return dsn.to_string();
        };
        let credentials = &rest[..at];
        let Some(colon) = credentials.find(':') else {
            return dsn.to_string();
        };
        format!(
            "{}{}:***{}",
            &dsn[..authority_start],
            &credentials[..colon],
            &rest[at..]
        )
    }
}

pub fn registry_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".config/cha0s/settings.json")
}

/// DSN resolution, pure: environment first, then the registry, then a named failure.
pub fn resolve_dsn(
    env_dsn: Option<String>,
    registry: &Value,
    registry_path: &std::path::Path,
) -> Result<(String, String), ConfigError> {
    if let Some(value) = env_dsn
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        return Ok((value, "env:PHYLACTERY_DSN".to_string()));
    }
    for name in REGISTRY_NAMES {
        if let Some(value) = registry
            .pointer(&format!("/agent/databases/{name}/dsn"))
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
        {
            return Ok((value.to_string(), format!("registry:{name}")));
        }
    }
    Err(ConfigError::MissingDsn(registry_path.display().to_string()))
}

/// Ollama URL resolution, pure: environment first, then the registry, then the default.
pub fn resolve_ollama(env_url: Option<String>, registry: &Value) -> (String, String) {
    if let Some(value) = env_url
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        return (trim_slash(&value), "env:PHYLACTERY_OLLAMA_URL".to_string());
    }
    if let Some(value) = registry
        .pointer("/language_models/ollama/api_url")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
    {
        return (trim_slash(value), "registry:ollama".to_string());
    }
    (DEFAULT_OLLAMA_URL.to_string(), "default".to_string())
}

/// Embedding endpoint resolution: `PHYLACTERY_EMBED_URL`, then the legacy `PHYLACTERY_OLLAMA_URL`,
/// then the registry, then the default. The legacy name stays because a working setup must not
/// break when the vocabulary grows.
pub fn resolve_embed_url(
    env_url: Option<String>,
    legacy_url: Option<String>,
    registry: &Value,
) -> (String, String) {
    if let Some(value) = env_url
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        return (trim_slash(&value), "env:PHYLACTERY_EMBED_URL".to_string());
    }
    if let Some(value) = legacy_url
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        return (trim_slash(&value), "env:PHYLACTERY_OLLAMA_URL".to_string());
    }
    if let Some(value) = registry
        .pointer("/language_models/ollama/api_url")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
    {
        return (trim_slash(value), "registry:ollama".to_string());
    }
    (DEFAULT_EMBED_URL.to_string(), "default".to_string())
}

fn env_string(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn env_parse<T: std::str::FromStr>(key: &str) -> Option<T> {
    env_string(key).and_then(|value| value.parse().ok())
}

fn trim_slash(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_dsn(dsn: &str) -> Config {
        Config {
            dsn: dsn.to_string(),
            dsn_source: "test".to_string(),
            ollama_url: DEFAULT_OLLAMA_URL.to_string(),
            ollama_source: "test".to_string(),
            embed_api: EmbedApi::Ollama,
            embed_url: DEFAULT_EMBED_URL.to_string(),
            embed_source: "test".to_string(),
            embed_api_key: None,
            embed_model: DEFAULT_EMBED_MODEL.to_string(),
            agent_id: DEFAULT_AGENT_ID.to_string(),
            pool_max: 1,
            statement_timeout_ms: 1000,
            registry_path: PathBuf::from("/nonexistent/settings.json"),
            backup_dir: PathBuf::from("/tmp/phylactery-backups"),
            migrations_dir: PathBuf::from("/tmp/phylactery-migrations"),
            admin_dsn: None,
            backup_keys: vec!["phylactery-auto".to_string()],
            kg_since: chrono::DateTime::parse_from_rfc3339("2026-11-14T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        }
    }

    #[test]
    fn redacts_password_only() {
        let config = config_with_dsn("postgresql://user:secret@localhost:5432/phylactery");
        assert_eq!(
            config.redacted_dsn(),
            "postgresql://user:***@localhost:5432/phylactery"
        );
    }

    #[test]
    fn keeps_dsn_without_password() {
        let config = config_with_dsn("postgresql://phylactery_rw@localhost:5432/phylactery");
        assert_eq!(
            config.redacted_dsn(),
            "postgresql://phylactery_rw@localhost:5432/phylactery"
        );
    }
}
