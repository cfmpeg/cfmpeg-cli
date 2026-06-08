use flate2::read::GzDecoder;
use serde_yaml::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tar::Archive;
use tempfile::tempdir;

fn repo_path(path: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path)
}

fn repo_file(path: &str) -> String {
    let full_path = repo_path(path);

    fs::read_to_string(&full_path)
        .unwrap_or_else(|error| panic!("failed to read {}: {}", full_path.display(), error))
}

fn workflow(path: &str) -> Value {
    serde_yaml::from_str(&repo_file(path))
        .unwrap_or_else(|error| panic!("failed to parse workflow {path}: {error}"))
}

fn workflow_job<'a>(workflow: &'a Value, name: &str) -> &'a Value {
    let job = &workflow["jobs"][name];
    assert!(!job.is_null(), "missing workflow job `{name}`");
    job
}

fn workflow_step<'a>(job: &'a Value, name: &str) -> &'a Value {
    job["steps"]
        .as_sequence()
        .unwrap_or_else(|| panic!("job is missing steps"))
        .iter()
        .find(|step| step["name"].as_str() == Some(name))
        .unwrap_or_else(|| panic!("missing workflow step `{name}`"))
}

fn write_executable(path: &Path, contents: &str) {
    fs::write(path, contents)
        .unwrap_or_else(|error| panic!("failed to write executable {}: {}", path.display(), error));

    let mut permissions = fs::metadata(path)
        .unwrap_or_else(|error| panic!("failed to stat {}: {}", path.display(), error))
        .permissions();
    permissions.set_mode(0o755);

    fs::set_permissions(path, permissions).unwrap_or_else(|error| {
        panic!(
            "failed to set executable permissions on {}: {}",
            path.display(),
            error
        )
    });
}

fn render(script: &str, replacements: &[(&str, &str)]) -> String {
    replacements
        .iter()
        .fold(script.to_owned(), |rendered, (needle, value)| {
            rendered.replace(needle, value)
        })
}

fn run_script(
    script: &str,
    working_directory: &Path,
    extra_path: Option<&Path>,
    envs: &[(&str, &str)],
) -> Output {
    let script_path = working_directory.join("__workflow_test.sh");
    fs::write(&script_path, format!("#!/bin/bash\n{}\n", script)).unwrap_or_else(|error| {
        panic!(
            "failed to write workflow script {}: {}",
            script_path.display(),
            error
        )
    });

    let mut script_permissions = fs::metadata(&script_path)
        .unwrap_or_else(|error| panic!("failed to stat {}: {}", script_path.display(), error))
        .permissions();
    script_permissions.set_mode(0o755);
    fs::set_permissions(&script_path, script_permissions).unwrap_or_else(|error| {
        panic!(
            "failed to set executable permissions on {}: {}",
            script_path.display(),
            error
        )
    });

    let mut command = Command::new("bash");
    command.arg(&script_path).current_dir(working_directory);

    if let Some(extra_path) = extra_path {
        let current_path = std::env::var("PATH").unwrap_or_default();
        command.env("PATH", format!("{}:{}", extra_path.display(), current_path));
    }

    for (key, value) in envs {
        command.env(key, value);
    }

    command.output().unwrap_or_else(|error| {
        panic!(
            "failed to execute workflow script {}: {}",
            script_path.display(),
            error
        )
    })
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "script failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn release_binaries_workflow_normalizes_tags_and_packages_release_assets() {
    let workflow = workflow(".github/workflows/release-binaries.yml");
    let build_job = workflow_job(&workflow, "build-binaries");

    let matrix = build_job["strategy"]["matrix"]["include"]
        .as_sequence()
        .expect("release-binaries matrix should be a sequence");
    let asset_names: Vec<_> = matrix
        .iter()
        .map(|entry| {
            entry["asset_name"]
                .as_str()
                .expect("asset_name should be a string")
        })
        .collect();

    assert_eq!(
        asset_names,
        vec![
            "cfmpeg-darwin-arm64",
            "cfmpeg-darwin-x64",
            "cfmpeg-linux-arm64",
            "cfmpeg-linux-x64",
        ]
    );

    let normalize_step = workflow_step(build_job, "Normalize and validate tag");
    let normalize_script = render(
        normalize_step["run"]
            .as_str()
            .expect("normalize tag step should have a run script"),
        &[("${{ inputs.tag }}", "refs/tags/v1.2.3")],
    );

    let normalization_workspace = tempdir().expect("failed to create normalization tempdir");
    let github_output = normalization_workspace.path().join("github-output.txt");
    let output = run_script(
        &normalize_script,
        normalization_workspace.path(),
        None,
        &[("GITHUB_OUTPUT", github_output.to_str().unwrap())],
    );
    assert_success(&output);

    assert_eq!(
        fs::read_to_string(&github_output).expect("failed to read GitHub output"),
        "value=v1.2.3\n"
    );

    let prepare_step = workflow_step(build_job, "Prepare release assets");
    let prepare_script = render(
        prepare_step["run"]
            .as_str()
            .expect("prepare assets step should have a run script"),
        &[("${{ matrix.asset_name }}", "cfmpeg-linux-x64")],
    );

    let packaging_workspace = tempdir().expect("failed to create packaging tempdir");
    fs::create_dir_all(packaging_workspace.path().join("target/release"))
        .expect("failed to create target/release");
    fs::create_dir_all(
        packaging_workspace
            .path()
            .join("runner-temp/ffmpeg-helper/bin"),
    )
    .expect("failed to create helper bin directory");
    fs::write(
        packaging_workspace.path().join("target/release/cfmpeg"),
        "cfmpeg-binary",
    )
    .expect("failed to write cfmpeg binary");
    fs::write(
        packaging_workspace
            .path()
            .join("runner-temp/ffmpeg-helper/bin/ffmpeg"),
        "ffmpeg-binary",
    )
    .expect("failed to write ffmpeg helper");
    fs::write(
        packaging_workspace
            .path()
            .join("runner-temp/ffmpeg-helper/bin/ffprobe"),
        "ffprobe-binary",
    )
    .expect("failed to write ffprobe helper");

    let output = run_script(
        &prepare_script,
        packaging_workspace.path(),
        None,
        &[(
            "RUNNER_TEMP",
            packaging_workspace
                .path()
                .join("runner-temp")
                .to_str()
                .unwrap(),
        )],
    );
    assert_success(&output);

    let archive_path = packaging_workspace
        .path()
        .join("release/cfmpeg-linux-x64.tar.gz");
    let checksum_path = packaging_workspace
        .path()
        .join("release/cfmpeg-linux-x64.tar.gz.sha256");

    assert!(archive_path.exists(), "expected packaged archive to exist");
    assert!(checksum_path.exists(), "expected checksum file to exist");

    let archive = fs::File::open(&archive_path).expect("failed to open packaged archive");
    let archive = GzDecoder::new(archive);
    let mut archive = Archive::new(archive);

    let mut entries = archive
        .entries()
        .expect("failed to read packaged archive entries")
        .map(|entry| {
            entry
                .expect("failed to read archive entry")
                .path()
                .expect("failed to read archive path")
                .to_string_lossy()
                .into_owned()
        })
        .filter_map(|entry| {
            let normalized = entry
                .trim_start_matches("./")
                .trim_end_matches('/')
                .to_string();

            if normalized.is_empty()
                || normalized == "."
                || normalized.starts_with("._")
                || normalized.contains("/._")
            {
                None
            } else {
                Some(normalized)
            }
        })
        .collect::<Vec<_>>();
    entries.sort();

    assert_eq!(
        entries,
        vec![
            "bin".to_string(),
            "bin/cfmpeg".to_string(),
            "libexec".to_string(),
            "libexec/ffmpeg".to_string(),
            "libexec/ffprobe".to_string(),
        ]
    );

    let checksum = fs::read_to_string(&checksum_path).expect("failed to read checksum file");
    let (sha, file_name) = checksum
        .split_once("  ")
        .expect("checksum file should contain two-column shasum output");
    assert_eq!(sha.len(), 64, "checksum should be a SHA-256 digest");
    assert!(sha.chars().all(|character| character.is_ascii_hexdigit()));
    assert_eq!(file_name.trim(), "release/cfmpeg-linux-x64.tar.gz");

    let upload_step = workflow_step(build_job, "Upload to GitHub Release");
    assert_eq!(
        upload_step["uses"].as_str(),
        Some("softprops/action-gh-release@v2")
    );
    assert_eq!(
        upload_step["with"]["tag_name"].as_str(),
        Some("${{ steps.tag.outputs.value }}")
    );
}

#[test]
fn release_binaries_workflow_uses_current_macos_intel_runner_for_x64() {
    let workflow = workflow(".github/workflows/release-binaries.yml");
    let build_job = workflow_job(&workflow, "build-binaries");

    let matrix = build_job["strategy"]["matrix"]["include"]
        .as_sequence()
        .expect("release-binaries matrix should be a sequence");
    let darwin_x64 = matrix
        .iter()
        .find(|entry| entry["asset_name"].as_str() == Some("cfmpeg-darwin-x64"))
        .expect("missing darwin x64 matrix entry");

    assert_eq!(darwin_x64["os"].as_str(), Some("macos-15-intel"));
}

#[test]
fn release_binaries_workflow_installs_macos_x64_ffmpeg_build_dependencies() {
    let workflow = workflow(".github/workflows/release-binaries.yml");
    let build_job = workflow_job(&workflow, "build-binaries");
    let steps = build_job["steps"]
        .as_sequence()
        .expect("build-binaries job should have steps");

    let install_index = steps
        .iter()
        .position(|step| step["name"].as_str() == Some("Install macOS ffmpeg build dependencies"))
        .expect("missing macOS ffmpeg build dependency install step");
    let helper_index = steps
        .iter()
        .position(|step| step["name"].as_str() == Some("Build bundled ffmpeg helpers"))
        .expect("missing ffmpeg helper build step");

    assert!(
        install_index < helper_index,
        "ffmpeg build dependencies must be installed before building helpers"
    );

    let install_step = &steps[install_index];
    assert_eq!(
        install_step["if"].as_str(),
        Some("matrix.asset_name == 'cfmpeg-darwin-x64'")
    );

    let install_script = install_step["run"]
        .as_str()
        .expect("macOS dependency install step should have a run script");
    assert!(install_script.contains("brew install nasm"));
}

#[test]
fn release_binaries_workflow_installs_linux_x64_ffmpeg_build_dependencies() {
    let workflow = workflow(".github/workflows/release-binaries.yml");
    let build_job = workflow_job(&workflow, "build-binaries");
    let steps = build_job["steps"]
        .as_sequence()
        .expect("build-binaries job should have steps");

    let install_index = steps
        .iter()
        .position(|step| step["name"].as_str() == Some("Install ffmpeg build dependencies"))
        .expect("missing ffmpeg build dependency install step");
    let helper_index = steps
        .iter()
        .position(|step| step["name"].as_str() == Some("Build bundled ffmpeg helpers"))
        .expect("missing ffmpeg helper build step");

    assert!(
        install_index < helper_index,
        "ffmpeg build dependencies must be installed before building helpers"
    );

    let install_step = &steps[install_index];
    assert_eq!(
        install_step["if"].as_str(),
        Some("matrix.asset_name == 'cfmpeg-linux-x64'")
    );

    let install_script = install_step["run"]
        .as_str()
        .expect("dependency install step should have a run script");
    assert!(install_script.contains("sudo apt-get update"));
    assert!(install_script.contains("sudo apt-get install -y nasm"));
}

#[test]
fn build_helper_ffmpeg_disables_x86_assembly_when_nasm_is_unavailable() {
    let script = repo_file("scripts/build-helper-ffmpeg.sh");

    assert!(script.contains("command -v nasm"));
    assert!(script.contains("--disable-x86asm"));
}

#[test]
fn release_workflow_normalizes_versions_before_running_release_steps() {
    let workflow = workflow(".github/workflows/release.yml");
    let release_job = workflow_job(&workflow, "release");
    let normalize_step = workflow_step(release_job, "Normalize and validate version");
    let normalize_script = render(
        normalize_step["run"]
            .as_str()
            .expect("normalize version step should have a run script"),
        &[("${{ inputs.version }}", "v1.2.3")],
    );

    let workspace = tempdir().expect("failed to create normalization tempdir");
    let github_output = workspace.path().join("github-output.txt");
    let output = run_script(
        &normalize_script,
        workspace.path(),
        None,
        &[("GITHUB_OUTPUT", github_output.to_str().unwrap())],
    );
    assert_success(&output);

    assert_eq!(
        fs::read_to_string(&github_output).expect("failed to read GitHub output"),
        "number=1.2.3\n"
    );
}

#[test]
fn release_workflow_skips_the_version_bump_commit_when_manifest_is_current() {
    let workflow = workflow(".github/workflows/release.yml");
    let release_job = workflow_job(&workflow, "release");
    let commit_step = workflow_step(release_job, "Commit version bump");
    let commit_script = render(
        commit_step["run"]
            .as_str()
            .expect("commit version bump step should have a run script"),
        &[("${{ steps.version.outputs.number }}", "0.1.0")],
    );

    let workspace = tempdir().expect("failed to create workflow tempdir");
    let stub_bin = workspace.path().join("bin");
    fs::create_dir_all(&stub_bin).expect("failed to create stub bin directory");

    write_executable(
        &stub_bin.join("git"),
        r#"#!/bin/bash
if [ "$1" = "add" ]; then
  exit 0
fi

if [ "$1" = "diff" ] && [ "$2" = "--cached" ] && [ "$3" = "--quiet" ]; then
  exit 0
fi

if [ "$1" = "commit" ]; then
  echo "commit should not run when there are no staged manifest changes" >&2
  exit 1
fi

echo "unexpected git command: $*" >&2
exit 1
"#,
    );

    let output = run_script(&commit_script, workspace.path(), Some(&stub_bin), &[]);
    assert_success(&output);
}

#[test]
fn release_workflow_skips_mutating_steps_when_release_tag_already_exists() {
    let workflow = workflow(".github/workflows/release.yml");
    let release_job = workflow_job(&workflow, "release");
    let steps = release_job["steps"]
        .as_sequence()
        .expect("release job should have steps");

    let existing_tag_index = steps
        .iter()
        .position(|step| step["name"].as_str() == Some("Check existing tag"))
        .expect("missing existing tag check step");
    let bump_index = steps
        .iter()
        .position(|step| step["name"].as_str() == Some("Bump version"))
        .expect("missing version bump step");

    assert!(
        existing_tag_index < bump_index,
        "existing tag must be checked before mutating release steps"
    );

    let existing_tag_script = steps[existing_tag_index]["run"]
        .as_str()
        .expect("existing tag check step should have a run script");
    assert!(existing_tag_script.contains("git ls-remote --exit-code --tags origin"));
    assert!(existing_tag_script.contains("refs/tags/v${{ steps.version.outputs.number }}"));
    assert!(existing_tag_script.contains("exists=true"));
    assert!(existing_tag_script.contains("exists=false"));

    for step_name in [
        "Bump version",
        "Verify manifest after bump",
        "Commit version bump",
        "Create and push tag",
        "Push to main",
    ] {
        let step = workflow_step(release_job, step_name);
        assert_eq!(
            step["if"].as_str(),
            Some("steps.existing_tag.outputs.exists != 'true'"),
            "{step_name} should be skipped when the release tag already exists"
        );
    }
}

#[test]
fn release_workflow_publishes_the_crate_to_crates_io() {
    let workflow = workflow(".github/workflows/release.yml");
    let publish_job = workflow_job(&workflow, "publish-crate");

    assert_eq!(publish_job["needs"].as_str(), Some("release"));
    assert_eq!(
        publish_job["env"]["CARGO_REGISTRY_TOKEN"].as_str(),
        Some("${{ secrets.CARGO_REGISTRY_TOKEN }}")
    );

    let publish_step = workflow_step(publish_job, "Publish crate to crates.io");
    let publish_script = publish_step["run"]
        .as_str()
        .expect("publish step should have a run script");

    assert!(publish_script.contains("cargo publish --locked --token \"$CARGO_REGISTRY_TOKEN\""));
}

#[test]
fn release_workflow_skips_crates_io_publish_when_version_already_exists() {
    let workflow = workflow(".github/workflows/release.yml");
    let publish_job = workflow_job(&workflow, "publish-crate");
    let steps = publish_job["steps"]
        .as_sequence()
        .expect("publish-crate job should have steps");

    let existing_crate_index = steps
        .iter()
        .position(|step| step["name"].as_str() == Some("Check existing crate version"))
        .expect("missing existing crate version check step");
    let require_token_index = steps
        .iter()
        .position(|step| step["name"].as_str() == Some("Require crates.io token"))
        .expect("missing crates.io token requirement step");
    let publish_index = steps
        .iter()
        .position(|step| step["name"].as_str() == Some("Publish crate to crates.io"))
        .expect("missing crates.io publish step");

    assert!(
        existing_crate_index < require_token_index,
        "existing crate version must be checked before requiring a publish token"
    );
    assert!(
        existing_crate_index < publish_index,
        "existing crate version must be checked before publishing"
    );

    let existing_crate_script = steps[existing_crate_index]["run"]
        .as_str()
        .expect("existing crate version check step should have a run script");
    assert!(existing_crate_script.contains("https://crates.io/api/v1/crates/cfmpeg/"));
    assert!(existing_crate_script.contains("exists=true"));
    assert!(existing_crate_script.contains("exists=false"));

    let require_token_step = workflow_step(publish_job, "Require crates.io token");
    assert_eq!(
        require_token_step["if"].as_str(),
        Some("env.CARGO_REGISTRY_TOKEN == '' && steps.existing_crate.outputs.exists != 'true'")
    );

    let publish_step = workflow_step(publish_job, "Publish crate to crates.io");
    assert_eq!(
        publish_step["if"].as_str(),
        Some("env.CARGO_REGISTRY_TOKEN != '' && steps.existing_crate.outputs.exists != 'true'")
    );
}

#[test]
fn release_workflow_generates_and_commits_the_homebrew_formula_from_release_assets() {
    let workflow = workflow(".github/workflows/release.yml");
    let update_homebrew_job = workflow_job(&workflow, "update-homebrew");
    let update_formula_step = workflow_step(update_homebrew_job, "Update Homebrew formula");
    let update_formula_script = update_formula_step["run"]
        .as_str()
        .expect("update formula step should have a run script");

    let workspace = tempdir().expect("failed to create workflow tempdir");
    let stub_bin = workspace.path().join("bin");
    fs::create_dir_all(&stub_bin).expect("failed to create stub bin directory");

    let log_path = workspace.path().join("git.log");
    let clone_dest_path = workspace.path().join("clone-destination.txt");

    write_executable(
        &stub_bin.join("curl"),
        r#"#!/bin/bash
url="${@: -1}"
case "$url" in
  */cfmpeg-darwin-arm64.tar.gz.sha256)
    echo "1111111111111111111111111111111111111111111111111111111111111111  cfmpeg-darwin-arm64.tar.gz"
    ;;
  */cfmpeg-darwin-x64.tar.gz.sha256)
    echo "2222222222222222222222222222222222222222222222222222222222222222  cfmpeg-darwin-x64.tar.gz"
    ;;
  */cfmpeg-linux-arm64.tar.gz.sha256)
    echo "3333333333333333333333333333333333333333333333333333333333333333  cfmpeg-linux-arm64.tar.gz"
    ;;
  */cfmpeg-linux-x64.tar.gz.sha256)
    echo "4444444444444444444444444444444444444444444444444444444444444444  cfmpeg-linux-x64.tar.gz"
    ;;
  *)
    echo "unexpected curl URL: $url" >&2
    exit 1
    ;;
esac
"#,
    );

    write_executable(
        &stub_bin.join("git"),
        r#"#!/bin/bash
printf '%s\n' "$*" >> "$TEST_GIT_LOG"

if [ "$1" = "clone" ]; then
  dest="${@: -1}"
  mkdir -p "$dest/Formula"
  echo "old formula" > "$dest/Formula/cfmpeg.rb"
  printf '%s' "$dest" > "$TEST_CLONE_DEST"
  exit 0
fi

if [ "$1" = "diff" ] && [ "$2" = "--quiet" ]; then
  exit 1
fi

exit 0
"#,
    );

    write_executable(
        &stub_bin.join("sleep"),
        r#"#!/bin/bash
exit 0
"#,
    );

    let output = run_script(
        update_formula_script,
        workspace.path(),
        Some(&stub_bin),
        &[
            ("COMMITTER_TOKEN", "test-token"),
            ("VERSION", "1.2.3"),
            ("REPOSITORY", "aarondfrancis/cfmpeg"),
            ("REPOSITORY_OWNER", "aarondfrancis"),
            ("HOMEBREW_TAP_OWNER", "cfmpeg"),
            ("HOMEBREW_TAP_REPO", "cfmpeg-homebrew-tap"),
            ("TEST_GIT_LOG", log_path.to_str().unwrap()),
            ("TEST_CLONE_DEST", clone_dest_path.to_str().unwrap()),
        ],
    );
    assert_success(&output);

    let clone_destination =
        fs::read_to_string(&clone_dest_path).expect("failed to read clone destination");
    let formula_path = PathBuf::from(clone_destination).join("Formula/cfmpeg.rb");
    let formula = fs::read_to_string(&formula_path).expect("failed to read generated formula");

    assert!(formula.contains("version \"1.2.3\""));
    assert!(formula.contains(
        "url \"https://github.com/aarondfrancis/cfmpeg/releases/download/v1.2.3/cfmpeg-darwin-arm64.tar.gz\""
    ));
    assert!(formula
        .contains("sha256 \"1111111111111111111111111111111111111111111111111111111111111111\""));
    assert!(formula.contains("libexec.install \"libexec/ffmpeg\", \"libexec/ffprobe\""));
    assert!(
        formula.contains("assert_match version.to_s, shell_output(\"#{bin}/cfmpeg --version\")")
    );

    let git_log = fs::read_to_string(&log_path).expect("failed to read git log");
    assert!(git_log.contains(
        "clone https://x-access-token:test-token@github.com/cfmpeg/cfmpeg-homebrew-tap.git"
    ));
    assert!(git_log.contains("commit -m cfmpeg 1.2.3"));
    assert!(git_log.contains("push origin main"));
}

#[test]
fn release_workflow_smoke_tests_the_cfmpeg_homebrew_namespace() {
    let workflow = workflow(".github/workflows/release.yml");
    let smoke_job = workflow_job(&workflow, "smoke-test-homebrew");

    assert_eq!(
        smoke_job["env"]["HOMEBREW_TAP"].as_str(),
        Some("cfmpeg/cfmpeg-homebrew-tap")
    );
    assert_eq!(
        smoke_job["env"]["HOMEBREW_TAP_URL"].as_str(),
        Some("https://github.com/cfmpeg/cfmpeg-homebrew-tap.git")
    );

    let smoke_step = workflow_step(smoke_job, "Smoke test Homebrew install");
    let smoke_script = smoke_step["run"]
        .as_str()
        .expect("smoke test step should have a run script");

    assert!(smoke_script.contains("brew tap \"${HOMEBREW_TAP}\" \"${HOMEBREW_TAP_URL}\""));
    assert!(smoke_script.contains("brew install \"${HOMEBREW_TAP}/cfmpeg\""));
    assert!(smoke_script.contains("brew upgrade \"${HOMEBREW_TAP}/cfmpeg\""));
}

#[test]
fn readme_uses_the_cfmpeg_homebrew_namespace() {
    let readme = repo_file("README.md");

    assert!(readme.contains(
        "brew tap cfmpeg/cfmpeg-homebrew-tap https://github.com/cfmpeg/cfmpeg-homebrew-tap.git"
    ));
    assert!(readme.contains("brew install cfmpeg/cfmpeg-homebrew-tap/cfmpeg"));
    assert!(!readme.contains("brew install aarondfrancis/homebrew-tap/cfmpeg"));
}
