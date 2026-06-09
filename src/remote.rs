use crate::error::{CfmpegError, Result};
use serde::{Deserialize, Serialize};

pub const PROFILE_BALANCED: &str = "balanced";
pub const PROFILE_HIGHCPU: &str = "highcpu";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteExecutionOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u32>,
    #[serde(skip)]
    pub strict_remote: bool,
}

impl RemoteExecutionOptions {
    pub fn is_empty(&self) -> bool {
        self.profile.is_none()
            && self.cpu.is_none()
            && self.memory_mb.is_none()
            && self.timeout_seconds.is_none()
    }

    pub fn merge_defaults(&self, defaults: &Self) -> Self {
        Self {
            profile: self.profile.clone().or_else(|| defaults.profile.clone()),
            cpu: self.cpu.or(defaults.cpu),
            memory_mb: self.memory_mb.or(defaults.memory_mb),
            timeout_seconds: self.timeout_seconds.or(defaults.timeout_seconds),
            strict_remote: self.strict_remote,
        }
    }

    pub fn requires_strict_remote(&self) -> bool {
        self.strict_remote
    }
}

pub fn parse_profile(value: &str) -> Result<String> {
    match value {
        PROFILE_BALANCED | PROFILE_HIGHCPU => Ok(value.to_string()),
        "gpu" => Err(CfmpegError::ParseError(
            "GPU execution is not currently available; use `--cf-profile highcpu` instead"
                .to_string(),
        )),
        _ => Err(CfmpegError::ParseError(format!(
            "invalid value for --cf-profile: {value} (expected balanced or highcpu)"
        ))),
    }
}

pub fn parse_cpu_cores(value: &str) -> Result<u16> {
    let parsed = value.parse::<u16>().map_err(|_| {
        CfmpegError::ParseError(format!(
            "invalid value for --cf-cpu: {value} (expected a positive integer)"
        ))
    })?;

    if parsed == 0 {
        return Err(CfmpegError::ParseError(
            "invalid value for --cf-cpu: expected a positive integer".to_string(),
        ));
    }

    Ok(parsed)
}

pub fn parse_memory_mb(value: &str) -> Result<u32> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(CfmpegError::ParseError(
            "invalid value for --cf-memory: expected a size like 4096, 4g, or 512m".to_string(),
        ));
    }

    let split_at = trimmed
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(trimmed.len());
    let (number_part, unit_part) = trimmed.split_at(split_at);
    let number = number_part.parse::<u32>().map_err(|_| {
        CfmpegError::ParseError(format!(
            "invalid value for --cf-memory: {value} (expected a size like 4096, 4g, or 512m)"
        ))
    })?;

    if number == 0 {
        return Err(CfmpegError::ParseError(
            "invalid value for --cf-memory: expected a positive size".to_string(),
        ));
    }

    let multiplier = match unit_part.to_ascii_lowercase().as_str() {
        "" | "m" | "mb" | "mib" => 1,
        "g" | "gb" | "gib" => 1024,
        _ => {
            return Err(CfmpegError::ParseError(format!(
                "invalid value for --cf-memory: {value} (expected a size like 4096, 4g, or 512m)"
            )))
        }
    };

    number.checked_mul(multiplier).ok_or_else(|| {
        CfmpegError::ParseError(format!(
            "invalid value for --cf-memory: {value} is too large"
        ))
    })
}

pub fn parse_timeout_seconds(value: &str) -> Result<u32> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(CfmpegError::ParseError(
            "invalid value for --cf-timeout: expected a duration like 3600, 90s, 15m, or 2h"
                .to_string(),
        ));
    }

    let split_at = trimmed
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(trimmed.len());
    let (number_part, unit_part) = trimmed.split_at(split_at);
    let number = number_part.parse::<u32>().map_err(|_| {
        CfmpegError::ParseError(format!(
            "invalid value for --cf-timeout: {value} (expected a duration like 3600, 90s, 15m, or 2h)"
        ))
    })?;

    if number == 0 {
        return Err(CfmpegError::ParseError(
            "invalid value for --cf-timeout: expected a positive duration".to_string(),
        ));
    }

    let multiplier = match unit_part.to_ascii_lowercase().as_str() {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        _ => {
            return Err(CfmpegError::ParseError(format!(
                "invalid value for --cf-timeout: {value} (expected a duration like 3600, 90s, 15m, or 2h)"
            )))
        }
    };

    number.checked_mul(multiplier).ok_or_else(|| {
        CfmpegError::ParseError(format!(
            "invalid value for --cf-timeout: {value} is too large"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::{RemoteExecutionOptions, PROFILE_BALANCED};

    #[test]
    fn remote_execution_is_empty_without_profile_cpu_memory_or_timeout() {
        assert!(RemoteExecutionOptions::default().is_empty());

        assert!(!RemoteExecutionOptions {
            profile: Some(PROFILE_BALANCED.to_string()),
            ..RemoteExecutionOptions::default()
        }
        .is_empty());
    }

    #[test]
    fn explicit_remote_requests_require_remote_execution() {
        assert!(RemoteExecutionOptions {
            cpu: Some(8),
            strict_remote: true,
            ..RemoteExecutionOptions::default()
        }
        .requires_strict_remote());
    }

    #[test]
    fn strict_remote_without_resource_overrides_still_requires_remote_execution() {
        assert!(RemoteExecutionOptions {
            strict_remote: true,
            ..RemoteExecutionOptions::default()
        }
        .requires_strict_remote());
    }

    #[test]
    fn default_remote_preferences_do_not_require_remote_execution() {
        let merged = RemoteExecutionOptions::default().merge_defaults(&RemoteExecutionOptions {
            timeout_seconds: Some(3600),
            ..RemoteExecutionOptions::default()
        });

        assert!(!merged.requires_strict_remote());
    }
}
