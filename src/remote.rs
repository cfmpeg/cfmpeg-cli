use crate::error::{CfmpegError, Result};
use serde::{Deserialize, Serialize};

pub const PROFILE_BALANCED: &str = "balanced";
pub const PROFILE_HIGHCPU: &str = "highcpu";
pub const PROFILE_GPU: &str = "gpu";

pub const GPU_OFF: &str = "off";
pub const GPU_PREFER: &str = "prefer";
pub const GPU_REQUIRED: &str = "required";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteExecutionOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpu: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u32>,
}

impl RemoteExecutionOptions {
    pub fn is_empty(&self) -> bool {
        self.profile.is_none()
            && self.cpu.is_none()
            && self.memory_mb.is_none()
            && self.gpu.is_none()
            && self.timeout_seconds.is_none()
    }

    pub fn requests_gpu_execution(&self) -> bool {
        match self.gpu.as_deref() {
            Some(GPU_OFF) => false,
            Some(_) => true,
            None => self.profile.as_deref() == Some(PROFILE_GPU),
        }
    }

    pub fn merge_defaults(&self, defaults: &Self) -> Self {
        Self {
            profile: self.profile.clone().or_else(|| defaults.profile.clone()),
            cpu: self.cpu.or(defaults.cpu),
            memory_mb: self.memory_mb.or(defaults.memory_mb),
            gpu: self.gpu.clone().or_else(|| defaults.gpu.clone()),
            timeout_seconds: self.timeout_seconds.or(defaults.timeout_seconds),
        }
    }

    pub fn requires_strict_remote(&self) -> bool {
        self.gpu.as_deref() == Some(GPU_REQUIRED)
    }
}

pub fn parse_profile(value: &str) -> Result<String> {
    match value {
        PROFILE_BALANCED | PROFILE_HIGHCPU | PROFILE_GPU => Ok(value.to_string()),
        _ => Err(CfmpegError::ParseError(format!(
            "invalid value for --cf-profile: {value} (expected balanced, highcpu, or gpu)"
        ))),
    }
}

pub fn parse_gpu_mode(value: &str) -> Result<String> {
    match value {
        GPU_OFF | GPU_PREFER | GPU_REQUIRED => Ok(value.to_string()),
        _ => Err(CfmpegError::ParseError(format!(
            "invalid value for --cf-gpu: {value} (expected off, prefer, or required)"
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
    use super::{
        RemoteExecutionOptions, GPU_OFF, GPU_PREFER, GPU_REQUIRED, PROFILE_BALANCED, PROFILE_GPU,
    };

    #[test]
    fn detects_gpu_requests_from_mode_or_profile() {
        assert!(RemoteExecutionOptions {
            gpu: Some(GPU_PREFER.to_string()),
            ..RemoteExecutionOptions::default()
        }
        .requests_gpu_execution());

        assert!(RemoteExecutionOptions {
            gpu: Some(GPU_REQUIRED.to_string()),
            ..RemoteExecutionOptions::default()
        }
        .requests_gpu_execution());

        assert!(RemoteExecutionOptions {
            profile: Some(PROFILE_GPU.to_string()),
            ..RemoteExecutionOptions::default()
        }
        .requests_gpu_execution());

        assert!(!RemoteExecutionOptions {
            profile: Some(PROFILE_GPU.to_string()),
            gpu: Some(GPU_OFF.to_string()),
            ..RemoteExecutionOptions::default()
        }
        .requests_gpu_execution());

        assert!(!RemoteExecutionOptions {
            profile: Some(PROFILE_BALANCED.to_string()),
            ..RemoteExecutionOptions::default()
        }
        .requests_gpu_execution());
    }
}
