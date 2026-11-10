use crate::error::{CfmpegError, Result};
use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub fn ffmpeg_binary() -> Result<PathBuf> {
    let current_exe = env::current_exe().ok();
    let env_override = env::var_os("CFMPEG_FFMPEG_BINARY").map(PathBuf::from);
    let path_env = env::var_os("PATH");

    resolve_binary_path("ffmpeg", env_override.as_deref(), current_exe.as_deref(), path_env)
        .ok_or_else(|| {
            CfmpegError::Config(
                "ffmpeg was not found. Install ffmpeg on PATH, set CFMPEG_FFMPEG_BINARY, or use a cfmpeg release that bundles helper binaries.".to_string(),
            )
        })
}

/// Locates the ffmpeg that runs the user's own command (`--local` and fallback).
///
/// Unlike [`ffmpeg_binary`], a system ffmpeg wins over the bundled helper: the
/// helper is built without external libraries, so it cannot run commands that
/// need encoders such as libx264. It is still used when nothing else exists.
pub fn user_ffmpeg_binary() -> Result<PathBuf> {
    let binary_name = format!("ffmpeg{}", env::consts::EXE_SUFFIX);
    let env_override = env::var_os("CFMPEG_FFMPEG_BINARY").map(PathBuf::from);

    resolve_user_binary_path(
        &binary_name,
        env_override.as_deref(),
        env::current_exe().ok().as_deref(),
        env::var_os("PATH"),
    )
    .ok_or_else(|| {
        CfmpegError::Config(
            "ffmpeg was not found; install ffmpeg on PATH or set CFMPEG_FFMPEG_BINARY to run commands locally".to_string(),
        )
    })
}

fn resolve_user_binary_path(
    binary_name: &str,
    env_override: Option<&Path>,
    current_exe: Option<&Path>,
    path_env: Option<OsString>,
) -> Option<PathBuf> {
    if let Some(path) = env_override.filter(|path| path.is_file()) {
        return Some(path.to_path_buf());
    }

    find_on_path(binary_name, path_env)
        .or_else(|| resolve_binary_path(binary_name, None, current_exe, None))
}

fn resolve_binary_path(
    binary_name: &str,
    env_override: Option<&Path>,
    current_exe: Option<&Path>,
    path_env: Option<OsString>,
) -> Option<PathBuf> {
    if let Some(path) = env_override.filter(|path| path.is_file()) {
        return Some(path.to_path_buf());
    }

    if let Some(current_exe) = current_exe {
        // cfmpeg may have been started through a symlink (Homebrew links
        // bin/cfmpeg into its prefix, and macOS reports the path that was
        // invoked); the bundled helper sits next to the real executable.
        let real_exe = current_exe
            .canonicalize()
            .unwrap_or_else(|_| current_exe.to_path_buf());

        for candidate in adjacent_binary_candidates(binary_name, &real_exe) {
            if candidate.is_file() {
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
        .find(|candidate| candidate.is_file())
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
        let helper = libexec_dir.join("ffmpeg");
        fs::write(&helper, b"binary").expect("write helper");

        let resolved = resolve_binary_path("ffmpeg", None, Some(&current_exe), None);

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

    #[test]
    fn user_commands_prefer_system_ffmpeg_over_the_bundled_helper() {
        let prefix = tempfile::tempdir().expect("temp dir");
        let current_exe = prefix.path().join("bin").join("cfmpeg");
        let helper = prefix.path().join("libexec").join("ffmpeg");
        let system = prefix.path().join("system").join("ffmpeg");
        let override_binary = prefix.path().join("override").join("ffmpeg");
        for binary in [&helper, &system, &override_binary] {
            fs::create_dir_all(binary.parent().expect("parent")).expect("create dir");
            fs::write(binary, b"binary").expect("write binary");
        }
        let path_env = || Some(OsString::from(prefix.path().join("system").as_os_str()));

        assert_eq!(
            super::resolve_user_binary_path("ffmpeg", None, Some(&current_exe), path_env()),
            Some(system)
        );
        assert_eq!(
            super::resolve_user_binary_path(
                "ffmpeg",
                Some(&override_binary),
                Some(&current_exe),
                path_env()
            ),
            Some(override_binary.clone())
        );
        // cfmpeg's own remuxing and segmenting keep using the helper.
        assert_eq!(
            resolve_binary_path("ffmpeg", None, Some(&current_exe), path_env()),
            Some(helper)
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolves_helper_next_to_the_real_binary_behind_a_symlink() {
        // The layout of a Homebrew install: the real files live in a versioned
        // directory, and only bin/cfmpeg is linked into the prefix, where a
        // system ffmpeg may sit right next to the link.
        let prefix = tempfile::tempdir().expect("temp dir");
        let keg = prefix.path().join("Cellar").join("cfmpeg").join("0.1.6");
        let real_exe = keg.join("bin").join("cfmpeg");
        let helper = keg.join("libexec").join("ffmpeg");
        let linked_exe = prefix.path().join("bin").join("cfmpeg");
        let system = prefix.path().join("bin").join("ffmpeg");
        for file in [&real_exe, &helper, &system] {
            fs::create_dir_all(file.parent().expect("parent")).expect("create dir");
            fs::write(file, b"binary").expect("write binary");
        }
        std::os::unix::fs::symlink(&real_exe, &linked_exe).expect("symlink");
        let path_env = || Some(OsString::from(prefix.path().join("bin").as_os_str()));

        // cfmpeg's own jobs use the helper, the user's command the system ffmpeg.
        assert_eq!(
            resolve_binary_path("ffmpeg", None, Some(&linked_exe), path_env()),
            Some(helper.canonicalize().expect("canonical helper"))
        );
        assert_eq!(
            super::resolve_user_binary_path("ffmpeg", None, Some(&linked_exe), path_env()),
            Some(system)
        );
        // Without any other ffmpeg, the user's command falls back to the helper.
        assert_eq!(
            super::resolve_user_binary_path("ffmpeg", None, Some(&linked_exe), None),
            Some(helper.canonicalize().expect("canonical helper"))
        );
    }

    #[test]
    fn user_commands_use_the_bundled_helper_without_a_system_ffmpeg() {
        let prefix = tempfile::tempdir().expect("temp dir");
        let current_exe = prefix.path().join("bin").join("cfmpeg");
        let helper = prefix.path().join("libexec").join("ffmpeg");
        fs::create_dir_all(helper.parent().expect("parent")).expect("create dir");
        fs::write(&helper, b"binary").expect("write binary");

        let resolved = super::resolve_user_binary_path(
            "ffmpeg",
            None,
            Some(&current_exe),
            Some(OsString::from(prefix.path().join("empty").as_os_str())),
        );

        assert_eq!(resolved, Some(helper));
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
