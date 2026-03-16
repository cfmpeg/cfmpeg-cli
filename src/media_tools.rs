use crate::error::{CfmpegError, Result};
use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

const FFMPEG_BINARY_ENV: &str = "CFMPEG_FFMPEG_BINARY";
pub fn ffmpeg_binary() -> Result<PathBuf> {
    resolve_binary("ffmpeg", FFMPEG_BINARY_ENV)
}

fn resolve_binary(binary_name: &str, env_var: &str) -> Result<PathBuf> {
    let current_exe = env::current_exe().ok();
    let env_override = env::var_os(env_var).map(PathBuf::from);
    let path_env = env::var_os("PATH");

    resolve_binary_path(binary_name, env_override.as_deref(), current_exe.as_deref(), path_env)
        .ok_or_else(|| {
            CfmpegError::Config(format!(
                "{binary_name} was not found. Install ffmpeg on PATH, set {env_var}, or use a cfmpeg release that bundles helper binaries."
            ))
        })
}

fn resolve_binary_path(
    binary_name: &str,
    env_override: Option<&Path>,
    current_exe: Option<&Path>,
    path_env: Option<OsString>,
) -> Option<PathBuf> {
    if let Some(path) = env_override.filter(|path| is_usable_binary(path)) {
        return Some(path.to_path_buf());
    }

    if let Some(current_exe) = current_exe {
        for candidate in adjacent_binary_candidates(binary_name, current_exe) {
            if is_usable_binary(&candidate) {
                return Some(candidate);
            }
        }
    }

    find_on_path(binary_name, path_env)
}

fn adjacent_binary_candidates(binary_name: &str, current_exe: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    if let Some(bin_dir) = current_exe.parent() {
        candidates.push(bin_dir.join(binary_name));

        if let Some(prefix_dir) = bin_dir.parent() {
            candidates.push(prefix_dir.join("libexec").join(binary_name));
        }
    }

    candidates
}

fn find_on_path(binary_name: &str, path_env: Option<OsString>) -> Option<PathBuf> {
    let path_env = path_env?;

    env::split_paths(&path_env)
        .map(|directory| directory.join(binary_name))
        .find(|candidate| is_usable_binary(candidate))
}

fn is_usable_binary(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::resolve_binary_path;
    use std::env;
    use std::ffi::OsString;
    use std::fs;
    use std::path::PathBuf;
    use uuid::Uuid;

    #[test]
    fn env_override_wins() {
        let fixture = temp_binary("ffmpeg-env");
        let current_exe = fixture.parent().expect("parent").join("cfmpeg");

        let resolved = resolve_binary_path(
            "ffmpeg",
            Some(&fixture),
            Some(&current_exe),
            Some(OsString::from("/does/not/matter")),
        );

        assert_eq!(resolved, Some(fixture.clone()));

        cleanup(&fixture);
    }

    #[test]
    fn resolves_sibling_binary_next_to_cfmpeg() {
        let fixture = temp_dir("cfmpeg-bin");
        let current_exe = fixture.join("cfmpeg");
        let helper = fixture.join("ffmpeg");
        fs::write(&helper, b"binary").expect("write helper");

        let resolved = resolve_binary_path("ffmpeg", None, Some(&current_exe), None);

        assert_eq!(resolved, Some(helper.clone()));

        cleanup(&helper);
        cleanup(&current_exe);
    }

    #[test]
    fn resolves_homebrew_style_libexec_binary() {
        let fixture = temp_dir("cfmpeg-prefix");
        let bin_dir = fixture.join("bin");
        let libexec_dir = fixture.join("libexec");
        fs::create_dir_all(&bin_dir).expect("create bin");
        fs::create_dir_all(&libexec_dir).expect("create libexec");

        let current_exe = bin_dir.join("cfmpeg");
        let helper = libexec_dir.join("ffprobe");
        fs::write(&helper, b"binary").expect("write helper");

        let resolved = resolve_binary_path("ffprobe", None, Some(&current_exe), None);

        assert_eq!(resolved, Some(helper.clone()));

        cleanup(&helper);
        cleanup(&current_exe);
    }

    #[test]
    fn resolves_from_path_when_no_bundle_exists() {
        let fixture = temp_dir("cfmpeg-path");
        let helper = fixture.join("ffmpeg");
        fs::write(&helper, b"binary").expect("write helper");

        let resolved = resolve_binary_path(
            "ffmpeg",
            None,
            None,
            Some(OsString::from(fixture.as_os_str())),
        );

        assert_eq!(resolved, Some(helper.clone()));

        cleanup(&helper);
    }

    fn temp_binary(name: &str) -> PathBuf {
        let directory = temp_dir(name);
        let binary = directory.join("tool");
        fs::write(&binary, b"binary").expect("write binary");
        binary
    }

    fn temp_dir(prefix: &str) -> PathBuf {
        let directory = env::temp_dir().join(format!("{prefix}-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).expect("create temp dir");
        directory
    }

    fn cleanup(path: &PathBuf) {
        if path.is_file() {
            let _ = fs::remove_file(path);
        }

        if let Some(parent) = path.parent() {
            let _ = fs::remove_dir_all(parent);
        }
    }
}
