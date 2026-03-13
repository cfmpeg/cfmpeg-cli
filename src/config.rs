use crate::error::{CfmpegError, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const DEFAULT_API_BASE: &str = "https://api.cfmpeg.dev/v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub api_key: Option<String>,
    pub api_base: String,
    pub local_fallback: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            api_key: None,
            api_base: DEFAULT_API_BASE.to_string(),
            local_fallback: true,
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
}
