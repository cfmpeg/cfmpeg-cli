//! Local output files: checks before a job starts and atomic delivery after it.

use crate::error::{CfmpegError, Result};
use crate::parser::{Input, Output, Overwrite};
use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Refuses outputs that ffmpeg itself would refuse to open, so that nothing is
/// uploaded or encoded for a result that cannot be written.
pub fn preflight(inputs: &[Input], outputs: &[Output], overwrite: Overwrite) -> Result<()> {
    // Resolved, so that another spelling of an input's path is recognised.
    let local_inputs: Vec<(usize, PathBuf)> = inputs
        .iter()
        .enumerate()
        .filter_map(|(index, input)| match input {
            Input::LocalFile { path, .. } => Some((index, path.canonicalize().ok()?)),
            Input::Special(_) | Input::Url(_) => None,
        })
        .collect();

    for output in outputs {
        let path = &output.path;
        let not_writable = |reason: String| CfmpegError::OutputNotWritable {
            path: path.display().to_string(),
            reason,
        };

        // The download would replace the input that the job was made from.
        if let Ok(resolved) = path.canonicalize() {
            if let Some((input_index, _)) =
                local_inputs.iter().find(|(_, input)| *input == resolved)
            {
                return Err(CfmpegError::OutputIsInput {
                    output: path.display().to_string(),
                    input_index: *input_index,
                });
            }
        }

        match std::fs::metadata(path) {
            Ok(metadata) if metadata.is_dir() => {
                return Err(not_writable("it is a directory".to_string()));
            }
            Ok(_) if overwrite == Overwrite::Never => {
                return Err(CfmpegError::OutputExists(path.display().to_string()));
            }
            Ok(metadata) if metadata.permissions().readonly() => {
                return Err(not_writable("the existing file is read-only".to_string()));
            }
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(not_writable(error.to_string())),
        }

        let directory = directory_of(path);
        if !directory.is_dir() {
            return Err(not_writable(format!(
                "directory `{}` does not exist",
                directory.display()
            )));
        }

        // Creating a file is the portable way to learn whether a directory is
        // writable; the file is removed again when the handle is dropped.
        tempfile::Builder::new()
            .prefix(".cfmpeg-")
            .tempfile_in(directory)
            .map_err(|error| {
                not_writable(format!(
                    "directory `{}` is not writable ({error})",
                    directory.display()
                ))
            })?;
    }

    Ok(())
}

/// An output that is still being written, kept next to its destination.
///
/// The temporary file is removed when the value is dropped, so a failed or
/// interrupted download leaves no partial file behind and never touches a file
/// that already exists at the destination.
pub struct PartialOutput {
    temp_path: PathBuf,
    target: PathBuf,
}

impl PartialOutput {
    /// Creates an empty temporary file in the directory of `target`. The
    /// temporary name ends with the target's file name, extension included.
    pub fn create(target: &Path) -> Result<Self> {
        let not_writable = |reason: String| CfmpegError::OutputNotWritable {
            path: target.display().to_string(),
            reason,
        };
        let file_name = target
            .file_name()
            .ok_or_else(|| not_writable("it does not name a file".to_string()))?;

        let unique = Uuid::new_v4().simple().to_string();
        let mut temp_name = OsString::from(format!(".cfmpeg-{}-", &unique[..8]));
        temp_name.push(file_name);
        let temp_path = directory_of(target).join(temp_name);

        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(|error| not_writable(error.to_string()))?;

        Ok(Self {
            temp_path,
            target: target.to_path_buf(),
        })
    }

    /// Where the output is written until it is committed.
    pub fn path(&self) -> &Path {
        &self.temp_path
    }

    /// Moves the finished file to its destination in one atomic rename.
    pub fn commit(self, overwrite: Overwrite) -> Result<()> {
        match std::fs::metadata(&self.target) {
            Ok(_) if overwrite == Overwrite::Never => {
                return Err(CfmpegError::OutputExists(self.target.display().to_string()));
            }
            // A replaced file keeps its permissions, as it would when ffmpeg
            // overwrites it in place.
            Ok(existing) => std::fs::set_permissions(&self.temp_path, existing.permissions())?,
            Err(_) => {}
        }

        std::fs::rename(&self.temp_path, &self.target)?;

        Ok(())
    }
}

impl Drop for PartialOutput {
    fn drop(&mut self) {
        // After a commit the temporary file no longer exists and this fails,
        // which is fine; in every other case it removes the partial file.
        let _ = std::fs::remove_file(&self.temp_path);
    }
}

fn directory_of(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        Some(_) | None => Path::new("."),
    }
}

#[cfg(test)]
mod tests {
    use super::{preflight, PartialOutput};
    use crate::error::CfmpegError;
    use crate::parser::{Input, Output, Overwrite};
    use std::fs;
    use std::path::{Path, PathBuf};

    fn output(path: PathBuf) -> Output {
        Output {
            path,
            remote_name: "output_0.mp4".to_string(),
        }
    }

    #[test]
    fn preflight_refuses_an_output_that_is_also_an_input() {
        let directory = tempfile::tempdir().expect("temp dir");
        let media = directory.path().join("clip.mov");
        fs::write(&media, b"source").expect("input file");
        let inputs = [
            Input::Url("https://example.com/intro.mov".to_string()),
            Input::LocalFile {
                path: media.clone(),
                size: 6,
            },
        ];
        // Another spelling of the same file.
        let same_file = directory.path().join(".").join("clip.mov");

        let error = preflight(&inputs, &[output(same_file.clone())], Overwrite::Allowed)
            .expect_err("output is an input");

        assert_eq!(
            error.to_string(),
            format!(
                "Output {} same as Input #1 - exiting; ffmpeg cannot edit existing files in-place",
                same_file.display()
            )
        );
        assert!(preflight(
            &inputs,
            &[output(directory.path().join("clip-out.mov"))],
            Overwrite::Allowed
        )
        .is_ok());
        assert_eq!(fs::read(&media).expect("read input"), b"source");
    }

    fn entries(directory: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(directory)
            .expect("read dir")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    #[test]
    fn preflight_accepts_new_and_existing_files_when_overwriting() {
        let directory = tempfile::tempdir().expect("temp dir");
        let existing = directory.path().join("existing.mp4");
        fs::write(&existing, b"old").expect("existing file");

        preflight(
            &[],
            &[output(existing), output(directory.path().join("new.mp4"))],
            Overwrite::Allowed,
        )
        .expect("writable outputs");

        assert_eq!(entries(directory.path()), vec!["existing.mp4"]);
    }

    #[test]
    fn preflight_refuses_existing_file_like_ffmpeg_when_never_overwriting() {
        let directory = tempfile::tempdir().expect("temp dir");
        let existing = directory.path().join("out.mp4");
        fs::write(&existing, b"old").expect("existing file");

        let error =
            preflight(&[], &[output(existing.clone())], Overwrite::Never).expect_err("exists");

        assert_eq!(
            error.to_string(),
            format!("File '{}' already exists. Exiting.", existing.display())
        );
        assert!(preflight(
            &[],
            &[output(directory.path().join("new.mp4"))],
            Overwrite::Never
        )
        .is_ok());
    }

    #[test]
    fn preflight_refuses_missing_directory() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("missing").join("out.mp4");

        let error =
            preflight(&[], &[output(path)], Overwrite::Allowed).expect_err("missing directory");

        assert!(matches!(error, CfmpegError::OutputNotWritable { .. }));
        assert!(error.to_string().contains("does not exist"), "{error}");
    }

    #[test]
    fn preflight_refuses_directory_as_output() {
        let directory = tempfile::tempdir().expect("temp dir");

        let error = preflight(
            &[],
            &[output(directory.path().to_path_buf())],
            Overwrite::Allowed,
        )
        .expect_err("directory output");

        assert!(error.to_string().contains("it is a directory"), "{error}");
    }

    #[test]
    fn preflight_refuses_read_only_existing_file() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("locked.mp4");
        fs::write(&path, b"old").expect("existing file");
        let mut permissions = fs::metadata(&path).expect("metadata").permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&path, permissions).expect("read-only");

        let error =
            preflight(&[], &[output(path)], Overwrite::Allowed).expect_err("read-only output");

        assert!(error.to_string().contains("read-only"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn preflight_refuses_unwritable_directory() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("temp dir");
        let locked = directory.path().join("locked");
        fs::create_dir(&locked).expect("locked dir");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).expect("lock dir");

        let result = preflight(&[], &[output(locked.join("out.mp4"))], Overwrite::Allowed);

        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).expect("unlock dir");
        let error = result.expect_err("unwritable directory");
        assert!(error.to_string().contains("is not writable"), "{error}");
    }

    #[test]
    fn partial_output_replaces_destination_only_on_commit() {
        let directory = tempfile::tempdir().expect("temp dir");
        let target = directory.path().join("out.mp4");
        fs::write(&target, b"old").expect("existing file");

        let partial = PartialOutput::create(&target).expect("partial output");
        fs::write(partial.path(), b"new").expect("write partial");

        assert_eq!(partial.path().parent(), target.parent());
        assert_eq!(partial.path().extension(), target.extension());
        assert_eq!(fs::read(&target).expect("read target"), b"old");

        partial.commit(Overwrite::Allowed).expect("commit");

        assert_eq!(fs::read(&target).expect("read target"), b"new");
        assert_eq!(entries(directory.path()), vec!["out.mp4"]);
    }

    #[test]
    fn partial_output_is_removed_when_dropped() {
        let directory = tempfile::tempdir().expect("temp dir");
        let target = directory.path().join("out.mp4");
        fs::write(&target, b"old").expect("existing file");

        let partial = PartialOutput::create(&target).expect("partial output");
        fs::write(partial.path(), b"half").expect("write partial");
        drop(partial);

        assert_eq!(fs::read(&target).expect("read target"), b"old");
        assert_eq!(entries(directory.path()), vec!["out.mp4"]);
    }

    #[test]
    fn partial_output_never_replaces_a_file_that_appeared_meanwhile() {
        let directory = tempfile::tempdir().expect("temp dir");
        let target = directory.path().join("out.mp4");

        let partial = PartialOutput::create(&target).expect("partial output");
        fs::write(partial.path(), b"new").expect("write partial");
        fs::write(&target, b"appeared").expect("concurrent file");

        let error = partial.commit(Overwrite::Never).expect_err("exists");

        assert!(matches!(error, CfmpegError::OutputExists(_)));
        assert_eq!(fs::read(&target).expect("read target"), b"appeared");
        assert_eq!(entries(directory.path()), vec!["out.mp4"]);
    }
}
