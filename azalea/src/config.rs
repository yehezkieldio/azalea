//! # Module overview
//! Configuration management for the Azalea application.
//!
//! ## Data flow
//! `AppConfig::load` → [`AppConfig::validate`] → [`crate::App::new`].
//!
//! ## Design rationale
//! Validation lives here to keep the main entrypoint free of configuration
//! edge-case handling.
//!
//! ## References
//! - TOML spec: <https://toml.io/en/>

pub use crate::ids::ApplicationId;
use azalea_core::config::EngineSettings;
use figment::{
    Figment,
    providers::{Format, Serialized, Toml},
};
use serde::Deserialize;
use serde_json::{Map, Number, Value};
use std::fmt;
use std::time::Duration;

/// Default configuration file path.
const CONFIG_FILE: &str = "azalea.config.toml";
const DOTENV_FILE: &str = ".env";

/// Secret token used to authenticate with Discord.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct DiscordToken(Box<str>);

impl DiscordToken {
    pub fn new(value: String) -> Self {
        Self(value.into_boxed_str())
    }

    #[cfg(test)]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0.into()
    }
}

impl fmt::Debug for DiscordToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DiscordToken(<redacted>)")
    }
}

/// Auth-only settings loaded from environment sources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthConfig {
    pub discord_token: DiscordToken,
}

/// Fully loaded startup configuration.
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub app: AppConfig,
    pub auth: AuthConfig,
}

/// Runtime configuration.
///
/// ## Constraints
/// Thread counts must be non-zero; stack size is clamped by validation.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(default)]
pub struct RuntimeConfig {
    pub worker_threads: usize,
    pub max_blocking_threads: usize,
    pub thread_stack_size: usize,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        let cpus = std::thread::available_parallelism()
            .map(|value| value.get())
            .unwrap_or(4);
        Self {
            worker_threads: cpus,
            max_blocking_threads: cpus * 4,
            thread_stack_size: 2 * 1024 * 1024,
        }
    }
}

/// Top-level application configuration.
///
/// ## Business rules
/// An application id is required, either via config file or environment.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(default)]
pub struct AppConfig {
    pub application_id: ApplicationId,
    pub runtime: RuntimeConfig,
    #[serde(flatten)]
    pub engine: EngineSettings,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            application_id: ApplicationId::new(0),
            runtime: RuntimeConfig::default(),
            engine: EngineSettings::default(),
        }
    }
}

impl AppConfig {
    /// Load configuration from file + env sources using figment layering.
    ///
    /// Merge order (low → high):
    /// 1. `azalea.config.toml`
    /// 2. `.env`
    /// 3. process environment
    pub fn load() -> anyhow::Result<LoadedConfig> {
        let config_file_contents = match std::fs::read_to_string(CONFIG_FILE) {
            Ok(contents) => {
                tracing::info!(path = CONFIG_FILE, "Loaded configuration");
                Some(contents)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                tracing::info!(path = CONFIG_FILE, "Config file not found, using defaults");
                None
            }
            Err(err) => return Err(err.into()),
        };

        let dotenv_entries = parse_dotenv_file(DOTENV_FILE)?;
        let process_env = std::env::vars().collect::<Vec<_>>();
        let sources = ConfigSources {
            config_file_contents,
            dotenv_entries,
            process_env,
        };
        load_from_sources(sources)
    }

    /// Validate configuration values and enforce application constraints.
    ///
    /// ## Preconditions
    /// - Settings are already deserialized into structured types.
    ///
    /// ## Postconditions
    /// - All runtime constraints and core engine limits are satisfied.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.application_id.get() == 0 {
            anyhow::bail!(
                "application_id must be set (via config, APPLICATION_ID, or AZALEA_APPLICATION_ID)"
            );
        }

        if self.runtime.worker_threads == 0 {
            anyhow::bail!("runtime.worker_threads must be at least 1");
        }

        if self.runtime.max_blocking_threads == 0 {
            anyhow::bail!("runtime.max_blocking_threads must be at least 1");
        }

        if self.runtime.thread_stack_size < 1024 * 1024 {
            anyhow::bail!("runtime.thread_stack_size must be at least 1 MiB");
        }

        self.engine.validate()?;
        Ok(())
    }

    /// Convenience wrapper for the upload timeout used by Discord HTTP requests.
    ///
    /// ## Rationale
    /// Keeps HTTP timeout configuration aligned with pipeline settings.
    pub fn upload_timeout(&self) -> Duration {
        self.engine.upload_timeout()
    }
}

#[derive(Debug, Clone)]
struct ConfigSources {
    config_file_contents: Option<String>,
    dotenv_entries: Vec<(String, String)>,
    process_env: Vec<(String, String)>,
}

#[derive(Debug, Clone, Copy)]
struct EnvBinding {
    key: &'static str,
    aliases: &'static [&'static str],
    path: &'static [&'static str],
}

impl EnvBinding {
    fn matches(self, key: &str) -> bool {
        self.key == key || self.aliases.contains(&key)
    }
}

const ENV_BINDINGS: &[EnvBinding] = &[
    EnvBinding {
        key: "APPLICATION_ID",
        aliases: &[],
        path: &["application_id"],
    },
    EnvBinding {
        key: "WORKER_THREADS",
        aliases: &[],
        path: &["runtime", "worker_threads"],
    },
    EnvBinding {
        key: "MAX_BLOCKING_THREADS",
        aliases: &[],
        path: &["runtime", "max_blocking_threads"],
    },
    EnvBinding {
        key: "THREAD_STACK_SIZE",
        aliases: &[],
        path: &["runtime", "thread_stack_size"],
    },
    EnvBinding {
        key: "DOWNLOAD",
        aliases: &[],
        path: &["concurrency", "download"],
    },
    EnvBinding {
        key: "UPLOAD",
        aliases: &[],
        path: &["concurrency", "upload"],
    },
    EnvBinding {
        key: "TRANSCODE",
        aliases: &[],
        path: &["concurrency", "transcode"],
    },
    EnvBinding {
        key: "YTDLP",
        aliases: &[],
        path: &["concurrency", "ytdlp"],
    },
    EnvBinding {
        key: "PIPELINE",
        aliases: &[],
        path: &["concurrency", "pipeline"],
    },
    EnvBinding {
        key: "POOL_MAX_IDLE_PER_HOST",
        aliases: &[],
        path: &["http", "pool_max_idle_per_host"],
    },
    EnvBinding {
        key: "POOL_IDLE_TIMEOUT_SECS",
        aliases: &[],
        path: &["http", "pool_idle_timeout_secs"],
    },
    EnvBinding {
        key: "CONNECT_TIMEOUT_SECS",
        aliases: &[],
        path: &["http", "connect_timeout_secs"],
    },
    EnvBinding {
        key: "TIMEOUT_SECS",
        aliases: &[],
        path: &["http", "timeout_secs"],
    },
    EnvBinding {
        key: "TEMP_DIR",
        aliases: &[],
        path: &["storage", "temp_dir"],
    },
    EnvBinding {
        key: "DEDUP_TTL_HOURS",
        aliases: &[],
        path: &["storage", "dedup_ttl_hours"],
    },
    EnvBinding {
        key: "DEDUP_CACHE_SIZE",
        aliases: &[],
        path: &["storage", "dedup_cache_size"],
    },
    EnvBinding {
        key: "DEDUP_PERSISTENT",
        aliases: &[],
        path: &["storage", "dedup_persistent"],
    },
    EnvBinding {
        key: "DEDUP_DB_PATH",
        aliases: &[],
        path: &["storage", "dedup_db_path"],
    },
    EnvBinding {
        key: "DEDUP_FLUSH_INTERVAL_SECS",
        aliases: &[],
        path: &["storage", "dedup_flush_interval_secs"],
    },
    EnvBinding {
        key: "METRICS_DB_PATH",
        aliases: &[],
        path: &["storage", "metrics_db_path"],
    },
    EnvBinding {
        key: "METRICS_ENABLED",
        aliases: &[],
        path: &["storage", "metrics_enabled"],
    },
    EnvBinding {
        key: "RESOLVER_CACHE_TTL_SECS",
        aliases: &[],
        path: &["storage", "resolver_cache_ttl_secs"],
    },
    EnvBinding {
        key: "RESOLVER_CACHE_SIZE",
        aliases: &[],
        path: &["storage", "resolver_cache_size"],
    },
    EnvBinding {
        key: "RESOLVER_NEGATIVE_TTL_SECS",
        aliases: &[],
        path: &["storage", "resolver_negative_ttl_secs"],
    },
    EnvBinding {
        key: "QUALITY_PRESET",
        aliases: &[],
        path: &["transcode", "quality_preset"],
    },
    EnvBinding {
        key: "HARDWARE_ACCELERATION",
        aliases: &[],
        path: &["transcode", "hardware_acceleration"],
    },
    EnvBinding {
        key: "FFMPEG_THREADS",
        aliases: &[],
        path: &["transcode", "ffmpeg_threads"],
    },
    EnvBinding {
        key: "VAAPI_DEVICE",
        aliases: &[],
        path: &["transcode", "vaapi_device"],
    },
    EnvBinding {
        key: "MAX_UPLOAD_BYTES",
        aliases: &[],
        path: &["transcode", "max_upload_bytes"],
    },
    EnvBinding {
        key: "CONTAINER_OVERHEAD_RATIO",
        aliases: &[],
        path: &["transcode", "container_overhead_ratio"],
    },
    EnvBinding {
        key: "VBR_SAFETY_MARGIN",
        aliases: &[],
        path: &["transcode", "vbr_safety_margin"],
    },
    EnvBinding {
        key: "TRANSCODE_TARGET_RATIO",
        aliases: &[],
        path: &["transcode", "transcode_target_ratio"],
    },
    EnvBinding {
        key: "SPLIT_TARGET_RATIO",
        aliases: &[],
        path: &["transcode", "split_target_ratio"],
    },
    EnvBinding {
        key: "AUDIO_VBR_PADDING",
        aliases: &[],
        path: &["transcode", "audio_vbr_padding"],
    },
    EnvBinding {
        key: "MIN_BITRATE_KBPS",
        aliases: &[],
        path: &["transcode", "min_bitrate_kbps"],
    },
    EnvBinding {
        key: "MAX_SINGLE_VIDEO_DURATION_SECS",
        aliases: &[],
        path: &["transcode", "max_single_video_duration_secs"],
    },
    EnvBinding {
        key: "FFPROBE_TIMEOUT_SECS",
        aliases: &[],
        path: &["transcode", "ffprobe_timeout_secs"],
    },
    EnvBinding {
        key: "FFMPEG_TIMEOUT_SECS",
        aliases: &[],
        path: &["transcode", "ffmpeg_timeout_secs"],
    },
    EnvBinding {
        key: "QUEUE_BACKPRESSURE_TIMEOUT_MS",
        aliases: &[],
        path: &["pipeline", "queue_backpressure_timeout_ms"],
    },
    EnvBinding {
        key: "DOWNLOAD_TIMEOUT_SECS",
        aliases: &[],
        path: &["pipeline", "download_timeout_secs"],
    },
    EnvBinding {
        key: "UPLOAD_TIMEOUT_SECS",
        aliases: &[],
        path: &["pipeline", "upload_timeout_secs"],
    },
    EnvBinding {
        key: "MIN_DISK_SPACE_BYTES",
        aliases: &[],
        path: &["pipeline", "min_disk_space_bytes"],
    },
    EnvBinding {
        key: "MAX_DOWNLOAD_BYTES",
        aliases: &[],
        path: &["pipeline", "max_download_bytes"],
    },
    EnvBinding {
        key: "RESOLVER_TIMEOUT_SECS",
        aliases: &[],
        path: &["pipeline", "resolver_timeout_secs"],
    },
    EnvBinding {
        key: "YTDLP_TIMEOUT_SECS",
        aliases: &[],
        path: &["pipeline", "ytdlp_timeout_secs"],
    },
    EnvBinding {
        key: "USER_RATE_LIMIT_REQUESTS",
        aliases: &[],
        path: &["pipeline", "user_rate_limit_requests"],
    },
    EnvBinding {
        key: "USER_RATE_LIMIT_WINDOW_SECS",
        aliases: &[],
        path: &["pipeline", "user_rate_limit_window_secs"],
    },
    EnvBinding {
        key: "CHANNEL_RATE_LIMIT_REQUESTS",
        aliases: &[],
        path: &["pipeline", "channel_rate_limit_requests"],
    },
    EnvBinding {
        key: "CHANNEL_RATE_LIMIT_WINDOW_SECS",
        aliases: &[],
        path: &["pipeline", "channel_rate_limit_window_secs"],
    },
    EnvBinding {
        key: "PARALLEL_SEGMENT_THRESHOLD",
        aliases: &[],
        path: &["pipeline", "parallel_segment_threshold"],
    },
    EnvBinding {
        key: "FFMPEG",
        aliases: &["FFMPEG_PATH"],
        path: &["binaries", "ffmpeg"],
    },
    EnvBinding {
        key: "FFPROBE",
        aliases: &["FFPROBE_PATH"],
        path: &["binaries", "ffprobe"],
    },
    EnvBinding {
        key: "YTDLP_PATH",
        aliases: &[],
        path: &["binaries", "ytdlp"],
    },
];

fn load_from_sources(sources: ConfigSources) -> anyhow::Result<LoadedConfig> {
    let mut figment = Figment::new();

    if let Some(contents) = sources.config_file_contents {
        figment = figment.merge(Toml::string(&contents));
    }

    if let Some(dotenv_overrides) = build_env_overrides(&sources.dotenv_entries) {
        figment = figment.merge(Serialized::defaults(dotenv_overrides));
    }

    if let Some(process_overrides) = build_env_overrides(&sources.process_env) {
        figment = figment.merge(Serialized::defaults(process_overrides));
    }

    let config: AppConfig = figment.extract()?;
    config.validate()?;

    let token = resolve_discord_token(&sources.dotenv_entries, &sources.process_env)?;
    Ok(LoadedConfig {
        app: config,
        auth: AuthConfig {
            discord_token: DiscordToken::new(token),
        },
    })
}

fn build_env_overrides(entries: &[(String, String)]) -> Option<Value> {
    let mut root = Map::new();

    for (key, raw_value) in entries {
        let Some(path) = config_path_from_env_key(key) else {
            continue;
        };
        if path.is_empty() {
            continue;
        }

        let value = parse_env_scalar(raw_value);
        insert_nested(&mut root, &path, value);
    }

    if root.is_empty() {
        None
    } else {
        Some(Value::Object(root))
    }
}

fn config_path_from_env_key(key: &str) -> Option<Vec<String>> {
    let normalized = key.trim().to_ascii_uppercase();
    if normalized.is_empty() || normalized == "DISCORD_TOKEN" {
        return None;
    }

    if normalized == "APPLICATION_ID" {
        return Some(vec!["application_id".to_string()]);
    }

    let suffix = normalized.strip_prefix("AZALEA_")?;
    let path = ENV_BINDINGS
        .iter()
        .find(|binding| binding.matches(suffix))
        .map(|binding| binding.path)?;
    Some(path.iter().map(|part| (*part).to_string()).collect())
}

fn insert_nested(root: &mut Map<String, Value>, path: &[String], value: Value) {
    let Some((head, tail)) = path.split_first() else {
        return;
    };

    if tail.is_empty() {
        root.insert(head.clone(), value);
        return;
    }

    let entry = root
        .entry(head.clone())
        .or_insert_with(|| Value::Object(Map::new()));
    if !entry.is_object() {
        *entry = Value::Object(Map::new());
    }

    if let Value::Object(child) = entry {
        insert_nested(child, tail, value);
    }
}

fn parse_env_scalar(raw_value: &str) -> Value {
    let value = raw_value.trim();
    if value.eq_ignore_ascii_case("true") {
        return Value::Bool(true);
    }
    if value.eq_ignore_ascii_case("false") {
        return Value::Bool(false);
    }
    if let Ok(parsed) = value.parse::<i64>() {
        return Value::Number(parsed.into());
    }
    if let Ok(parsed) = value.parse::<u64>() {
        return Value::Number(parsed.into());
    }
    if let Ok(parsed) = value.parse::<f64>()
        && let Some(number) = Number::from_f64(parsed)
    {
        return Value::Number(number);
    }
    Value::String(value.to_string())
}

fn parse_dotenv_file(path: &str) -> anyhow::Result<Vec<(String, String)>> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };
    parse_dotenv_contents(&contents)
}

fn parse_dotenv_contents(contents: &str) -> anyhow::Result<Vec<(String, String)>> {
    let mut entries = Vec::new();
    for (line_number, raw_line) in contents.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((raw_key, raw_value)) = line.split_once('=') else {
            anyhow::bail!("invalid .env entry at line {}", line_number + 1);
        };

        let key = raw_key.trim();
        if key.is_empty() {
            anyhow::bail!("invalid .env key at line {}", line_number + 1);
        }

        let mut value = raw_value.trim().to_string();
        if ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
            && value.len() >= 2
        {
            value = value[1..value.len() - 1].to_string();
        }

        entries.push((key.to_string(), value));
    }
    Ok(entries)
}

fn resolve_discord_token(
    dotenv_entries: &[(String, String)],
    process_env: &[(String, String)],
) -> anyhow::Result<String> {
    if let Some((_, value)) = process_env
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("DISCORD_TOKEN"))
    {
        let token = value.trim();
        if !token.is_empty() {
            return Ok(token.to_string());
        }
        anyhow::bail!("discord token (`DISCORD_TOKEN`) cannot be empty");
    }

    if let Some((_, value)) = dotenv_entries
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("DISCORD_TOKEN"))
    {
        let token = value.trim();
        if !token.is_empty() {
            return Ok(token.to_string());
        }
        anyhow::bail!("discord token (`DISCORD_TOKEN`) cannot be empty");
    }

    anyhow::bail!("discord token (`DISCORD_TOKEN`) must be set via environment or .env")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;
    use proptest::prelude::*;
    use std::collections::HashSet;

    fn resolve_schema_ref<'a>(schema: &'a Value, mut node: &'a Value) -> Option<&'a Value> {
        while let Some(reference) = node.get("$ref").and_then(Value::as_str) {
            node = schema.pointer(reference.strip_prefix('#')?)?;
        }
        Some(node)
    }

    fn schema_has_config_path(schema: &Value, path: &[&str]) -> bool {
        let mut current = schema;
        for segment in path {
            let Some(resolved) = resolve_schema_ref(schema, current) else {
                return false;
            };
            let Some(properties) = resolved.get("properties").and_then(Value::as_object) else {
                return false;
            };
            let Some(next) = properties.get(*segment) else {
                return false;
            };
            current = next;
        }
        true
    }

    #[test]
    fn test_default_config_validates() {
        let config = AppConfig {
            application_id: ApplicationId::new(123456789),
            ..Default::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn load_precedence_defaults_toml_dotenv_env() -> anyhow::Result<()> {
        let loaded = load_from_sources(ConfigSources {
            config_file_contents: Some(
                r#"
                application_id = 11
                [runtime]
                worker_threads = 2
                "#
                .to_string(),
            ),
            dotenv_entries: vec![
                ("APPLICATION_ID".to_string(), "22".to_string()),
                ("AZALEA_WORKER_THREADS".to_string(), "4".to_string()),
                ("DISCORD_TOKEN".to_string(), "dotenv-token".to_string()),
            ],
            process_env: vec![
                ("APPLICATION_ID".to_string(), "33".to_string()),
                ("AZALEA_WORKER_THREADS".to_string(), "6".to_string()),
                ("DISCORD_TOKEN".to_string(), "env-token".to_string()),
            ],
        })?;

        assert_eq!(loaded.app.application_id.get(), 33);
        assert_eq!(loaded.app.runtime.worker_threads, 6);
        assert_eq!(loaded.auth.discord_token.as_str(), "env-token");
        Ok(())
    }

    #[test]
    fn load_supports_minimal_env_only() -> anyhow::Result<()> {
        let loaded = load_from_sources(ConfigSources {
            config_file_contents: None,
            dotenv_entries: Vec::new(),
            process_env: vec![
                ("APPLICATION_ID".to_string(), "123456789".to_string()),
                ("DISCORD_TOKEN".to_string(), "secret".to_string()),
            ],
        })?;

        assert_eq!(loaded.app.application_id.get(), 123456789);
        assert_eq!(loaded.auth.discord_token.as_str(), "secret");
        Ok(())
    }

    #[test]
    fn load_fails_without_token() -> anyhow::Result<()> {
        let result = load_from_sources(ConfigSources {
            config_file_contents: Some("application_id = 123".to_string()),
            dotenv_entries: Vec::new(),
            process_env: Vec::new(),
        });
        assert!(result.is_err());
        let err = match result {
            Ok(_) => anyhow::bail!("expected missing token error"),
            Err(err) => err,
        };

        assert!(err.to_string().contains("DISCORD_TOKEN"));
        Ok(())
    }

    #[test]
    fn load_supports_flat_env_paths() -> anyhow::Result<()> {
        let loaded = load_from_sources(ConfigSources {
            config_file_contents: Some(
                r#"
                application_id = 11
                [pipeline]
                max_download_bytes = 500
                "#
                .to_string(),
            ),
            dotenv_entries: Vec::new(),
            process_env: vec![
                ("APPLICATION_ID".to_string(), "123456789".to_string()),
                ("AZALEA_MAX_DOWNLOAD_BYTES".to_string(), "700".to_string()),
                ("DISCORD_TOKEN".to_string(), "secret".to_string()),
            ],
        })?;

        assert_eq!(loaded.app.engine.pipeline.max_download_bytes, 700);
        Ok(())
    }

    #[test]
    fn parse_dotenv_rejects_invalid_line() -> anyhow::Result<()> {
        let result = parse_dotenv_contents("NO_EQUALS_LINE");
        assert!(result.is_err());
        let err = match result {
            Ok(_) => anyhow::bail!("expected dotenv parse error"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("invalid .env entry"));
        Ok(())
    }

    #[test]
    fn parse_env_scalar_handles_common_types() {
        assert_eq!(parse_env_scalar("true"), Value::Bool(true));
        assert_eq!(parse_env_scalar("42"), Value::Number(42u64.into()));
        assert_eq!(
            parse_env_scalar("3.5"),
            Value::Number(Number::from_f64(3.5).expect("finite"))
        );
        assert_eq!(
            parse_env_scalar("hello"),
            Value::String("hello".to_string())
        );
    }

    #[test]
    fn config_path_maps_flat_keys() {
        assert_eq!(
            config_path_from_env_key("AZALEA_MAX_DOWNLOAD_BYTES"),
            Some(vec![
                "pipeline".to_string(),
                "max_download_bytes".to_string()
            ])
        );
        assert_eq!(
            config_path_from_env_key("AZALEA_WORKER_THREADS"),
            Some(vec!["runtime".to_string(), "worker_threads".to_string()])
        );
        assert_eq!(
            config_path_from_env_key("AZALEA_HARDWARE_ACCELERATION"),
            Some(vec![
                "transcode".to_string(),
                "hardware_acceleration".to_string()
            ])
        );
        assert_eq!(
            config_path_from_env_key("AZALEA_FFMPEG"),
            Some(vec!["binaries".to_string(), "ffmpeg".to_string()])
        );
        assert_eq!(
            config_path_from_env_key("AZALEA_YTDLP_PATH"),
            Some(vec!["binaries".to_string(), "ytdlp".to_string()])
        );
        assert_eq!(
            config_path_from_env_key("AZALEA_PIPELINE__MAX_DOWNLOAD_BYTES"),
            None
        );
        assert_eq!(config_path_from_env_key("DISCORD_TOKEN"), None);
    }

    #[test]
    fn env_registry_keys_are_unique() {
        let mut keys = HashSet::new();
        for binding in ENV_BINDINGS {
            assert!(
                keys.insert(binding.key),
                "duplicate env key {}",
                binding.key
            );
            for alias in binding.aliases {
                assert!(keys.insert(alias), "duplicate env alias {}", alias);
            }
        }
    }

    #[test]
    fn env_registry_paths_exist_in_schema() -> anyhow::Result<()> {
        let schema: Value = serde_json::from_str(include_str!("../../azalea.schema.json"))?;
        for binding in ENV_BINDINGS {
            assert!(
                schema_has_config_path(&schema, binding.path),
                "invalid config path for {}: {}",
                binding.key,
                binding.path.join(".")
            );
        }
        Ok(())
    }

    #[test]
    fn env_registry_aliases_resolve() {
        assert_eq!(
            config_path_from_env_key("AZALEA_FFMPEG_PATH"),
            Some(vec!["binaries".to_string(), "ffmpeg".to_string()])
        );
        assert_eq!(
            config_path_from_env_key("AZALEA_FFPROBE_PATH"),
            Some(vec!["binaries".to_string(), "ffprobe".to_string()])
        );
    }

    #[test]
    fn resolve_discord_token_rejects_empty_values() {
        let err = resolve_discord_token(
            &[("DISCORD_TOKEN".to_string(), "dotenv-token".to_string())],
            &[("DISCORD_TOKEN".to_string(), "   ".to_string())],
        )
        .expect_err("empty process token must be rejected");
        assert!(err.to_string().contains("cannot be empty"));
    }

    proptest! {
        #[test]
        fn app_id_aliases_always_map_to_application_id(key in prop_oneof![Just("APPLICATION_ID"), Just("AZALEA_APPLICATION_ID")]) {
            prop_assert_eq!(
                config_path_from_env_key(key),
                Some(vec!["application_id".to_string()])
            );
        }
    }
}
