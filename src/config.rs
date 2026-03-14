use crate::error::{CfmpegError, Result};
use crate::remote::{
    parse_cpu_cores, parse_gpu_mode, parse_memory_mb, parse_profile, parse_timeout_seconds,
    RemoteExecutionOptions,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const DEFAULT_API_BASE: &str = "https://api.cfmpeg.dev/v1";
const DEFAULT_WEB_BASE: &str = "https://cfmpeg.dev";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub api_key: Option<String>,
    pub api_base: String,
    pub local_fallback: bool,
    pub remote_profile: Option<String>,
    pub remote_cpu: Option<u16>,
    pub remote_memory_mb: Option<u32>,
    pub remote_gpu: Option<String>,
    pub remote_timeout_seconds: Option<u32>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            api_key: None,
            api_base: DEFAULT_API_BASE.to_string(),
            local_fallback: true,
            remote_profile: None,
            remote_cpu: None,
            remote_memory_mb: None,
            remote_gpu: None,
            remote_timeout_seconds: None,
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = Self::config_path()?;

        if !path.exists() {
            return Ok(Self::default());
        }

        let contents = std::fs::read_to_string(&path)?;

        toml::from_str(&contents).map_err(|error| {
            CfmpegError::Config(format!("failed to parse {}: {error}", path.display()))
        })
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::config_path()?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let contents = toml::to_string_pretty(self)
            .map_err(|error| CfmpegError::Config(format!("failed to serialize config: {error}")))?;

        std::fs::write(path, contents)?;

        Ok(())
    }

    pub fn config_path() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("config.toml"))
    }

    pub fn require_api_key(&self) -> Result<String> {
        self.api_key().ok_or(CfmpegError::NotAuthenticated)
    }

    pub fn api_key(&self) -> Option<String> {
        std::env::var("CFMPEG_API_KEY")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| self.api_key.clone())
    }

    pub fn api_key_from_env(&self) -> bool {
        std::env::var("CFMPEG_API_KEY")
            .ok()
            .is_some_and(|value| !value.trim().is_empty())
    }

    pub fn api_base(&self) -> String {
        std::env::var("CFMPEG_API_BASE")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| self.api_base.clone())
    }

    pub fn web_base(&self) -> String {
        std::env::var("CFMPEG_WEB_BASE")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| derive_web_base(&self.api_base()))
    }

    pub fn dashboard_api_keys_url(&self) -> String {
        format!(
            "{}/dashboard/api-keys",
            self.web_base().trim_end_matches('/')
        )
    }

    pub fn dashboard_billing_url(&self) -> String {
        format!(
            "{}/dashboard/billing",
            self.web_base().trim_end_matches('/')
        )
    }

    pub fn remote_execution_defaults(&self) -> Result<RemoteExecutionOptions> {
        Ok(RemoteExecutionOptions {
            profile: option_from_env("CFMPEG_REMOTE_PROFILE")
                .map(|value| parse_profile(&value))
                .transpose()?
                .or_else(|| self.remote_profile.clone()),
            cpu: option_from_env("CFMPEG_REMOTE_CPU")
                .map(|value| parse_cpu_cores(&value))
                .transpose()?
                .or(self.remote_cpu),
            memory_mb: option_from_env("CFMPEG_REMOTE_MEMORY")
                .map(|value| parse_memory_mb(&value))
                .transpose()?
                .or(self.remote_memory_mb),
            gpu: option_from_env("CFMPEG_REMOTE_GPU")
                .map(|value| parse_gpu_mode(&value))
                .transpose()?
                .or_else(|| self.remote_gpu.clone()),
            timeout_seconds: option_from_env("CFMPEG_REMOTE_TIMEOUT")
                .map(|value| parse_timeout_seconds(&value))
                .transpose()?
                .or(self.remote_timeout_seconds),
        })
    }

    fn config_dir() -> Result<PathBuf> {
        if let Ok(override_dir) = std::env::var("CFMPEG_CONFIG_DIR") {
            let trimmed = override_dir.trim();
            if !trimmed.is_empty() {
                return Ok(PathBuf::from(trimmed));
            }
        }

        let base = dirs::config_dir().ok_or_else(|| {
            CfmpegError::Config("unable to determine the system config directory".to_string())
        })?;

        Ok(base.join("cfmpeg"))
    }
}

fn derive_web_base(api_base: &str) -> String {
    let trimmed = api_base.trim_end_matches('/');
    let without_version = trimmed
        .strip_suffix("/v1")
        .or_else(|| trimmed.strip_suffix("/api/v1"))
        .unwrap_or(trimmed);

    if let Some(rest) = without_version.strip_prefix("https://api.") {
        return format!("https://{rest}");
    }

    if let Some(rest) = without_version.strip_prefix("http://api.") {
        return format!("http://{rest}");
    }

    if without_version.contains("://") {
        return without_version.to_string();
    }

    DEFAULT_WEB_BASE.to_string()
}

fn option_from_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::Config;
    use uuid::Uuid;

    fn unique_config_dir() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("cfmpeg-config-{}", Uuid::new_v4()))
    }

    #[test]
    fn api_key_prefers_environment_override() {
        let config = Config {
            api_key: Some("config-key".to_string()),
            ..Config::default()
        };

        unsafe {
            std::env::set_var("CFMPEG_API_KEY", "env-key");
        }

        assert_eq!(config.api_key().as_deref(), Some("env-key"));

        unsafe {
            std::env::remove_var("CFMPEG_API_KEY");
        }
    }

    #[test]
    fn config_path_uses_override_directory() {
        let config_dir = unique_config_dir();

        unsafe {
            std::env::set_var("CFMPEG_CONFIG_DIR", &config_dir);
        }

        let path = Config::config_path().expect("config path");

        assert_eq!(path, config_dir.join("config.toml"));

        unsafe {
            std::env::remove_var("CFMPEG_CONFIG_DIR");
        }
    }

    #[test]
    fn web_base_defaults_to_public_site_for_default_api_base() {
        let config = Config::default();

        assert_eq!(config.web_base(), "https://cfmpeg.dev");
        assert_eq!(
            config.dashboard_api_keys_url(),
            "https://cfmpeg.dev/dashboard/api-keys"
        );
    }

    #[test]
    fn web_base_uses_local_api_origin_when_overridden() {
        let config = Config {
            api_base: "http://127.0.0.1:8000/v1".to_string(),
            ..Config::default()
        };

        assert_eq!(config.web_base(), "http://127.0.0.1:8000");
        assert_eq!(
            config.dashboard_billing_url(),
            "http://127.0.0.1:8000/dashboard/billing"
        );
    }

    #[test]
    fn remote_execution_defaults_merge_config_values() {
        let config = Config {
            remote_profile: Some("balanced".to_string()),
            remote_cpu: Some(4),
            remote_memory_mb: Some(8192),
            remote_gpu: Some("prefer".to_string()),
            remote_timeout_seconds: Some(1800),
            ..Config::default()
        };

        let remote = config
            .remote_execution_defaults()
            .expect("remote execution defaults");

        assert_eq!(remote.profile.as_deref(), Some("balanced"));
        assert_eq!(remote.cpu, Some(4));
        assert_eq!(remote.memory_mb, Some(8192));
        assert_eq!(remote.gpu.as_deref(), Some("prefer"));
        assert_eq!(remote.timeout_seconds, Some(1800));
    }
}
