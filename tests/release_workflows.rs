// These tests run workflow steps through bash with stubbed tools, so they are
// Unix-only; the Windows CI job compiles the crate without them.
#![cfg(unix)]

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

/// Workflow steps call the repository's scripts by relative path, but the
/// tests run them from a scratch directory.
fn with_repo_scripts(script: &str) -> String {
    script.replace(
        " scripts/",
        &format!(" {}/", repo_path("scripts").display()),
    )
}

/// What `scripts/build-helper-ffmpeg.sh` leaves in `<prefix>/licenses/ffmpeg`.
fn write_ffmpeg_licenses(directory: &Path) {
    fs::create_dir_all(directory).expect("failed to create ffmpeg licence directory");
    fs::write(directory.join("COPYING.LGPLv2.1"), "LGPL text").expect("failed to write LGPL");
    fs::write(directory.join("LICENSE.md"), "FFmpeg licence overview")
        .expect("failed to write LICENSE.md");
    fs::write(
        directory.join("NOTICE.txt"),
        "Source:   https://ffmpeg.org/releases/ffmpeg-8.0.tar.xz\nSHA-256:  0000\n\n  ./configure\n    --disable-autodetect\n",
    )
    .expect("failed to write NOTICE.txt");
}

fn tar_gz_files(archive_path: &Path) -> Vec<String> {
    let archive = fs::File::open(archive_path).expect("failed to open packaged archive");
    let mut archive = Archive::new(GzDecoder::new(archive));

    let mut files = archive
        .entries()
        .expect("failed to read packaged archive entries")
        .map(|entry| entry.expect("failed to read archive entry"))
        .filter(|entry| entry.header().entry_type().is_file())
        .map(|entry| {
            entry
                .path()
                .expect("failed to read archive path")
                .to_string_lossy()
                .trim_start_matches("./")
                .to_string()
        })
        .filter(|path| !path.starts_with("._") && !path.contains("/._"))
        .collect::<Vec<_>>();
    files.sort();
    files
}

fn top_level_directories(files: &[String]) -> Vec<&str> {
    let mut directories = files
        .iter()
        .filter_map(|file| file.split_once('/').map(|(directory, _)| directory))
        .collect::<Vec<_>>();
    directories.sort();
    directories.dedup();
    directories
}

fn assert_contains_all(files: &[String], expected: &[&str]) {
    for path in expected {
        assert!(
            files.iter().any(|file| file == path),
            "expected the archive to contain {path}; it holds {files:?}"
        );
    }
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

    let prepare_job = workflow_job(&workflow, "prepare");
    let normalize_step = workflow_step(prepare_job, "Normalize and validate tag");
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

    let packaging_steps: Vec<String> = [
        "Collect licence notices",
        "Prepare release assets",
        "Verify release archive",
    ]
    .iter()
    .map(|step_name| {
        let script = workflow_step(build_job, step_name)["run"]
            .as_str()
            .unwrap_or_else(|| panic!("{step_name} step should have a run script"));
        with_repo_scripts(&render(
            script,
            &[("${{ matrix.asset_name }}", "cfmpeg-linux-x64")],
        ))
    })
    .collect();

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
    write_ffmpeg_licenses(
        &packaging_workspace
            .path()
            .join("runner-temp/ffmpeg-helper/licenses/ffmpeg"),
    );

    for script in &packaging_steps {
        let output = run_script(
            script,
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
    }

    let archive_path = packaging_workspace
        .path()
        .join("release/cfmpeg-linux-x64.tar.gz");
    let checksum_path = packaging_workspace
        .path()
        .join("release/cfmpeg-linux-x64.tar.gz.sha256");

    assert!(archive_path.exists(), "expected packaged archive to exist");
    assert!(checksum_path.exists(), "expected checksum file to exist");

    let files = tar_gz_files(&archive_path);
    assert_eq!(
        top_level_directories(&files),
        vec!["bin", "libexec", "licenses"]
    );
    assert_contains_all(
        &files,
        &[
            "bin/cfmpeg",
            "libexec/ffmpeg",
            "licenses/README.txt",
            "licenses/cfmpeg/LICENSE",
            "licenses/ffmpeg/COPYING.LGPLv2.1",
            "licenses/ffmpeg/LICENSE.md",
            "licenses/ffmpeg/NOTICE.txt",
            "licenses/rust/CRATES.txt",
        ],
    );
    // These binaries link their C library dynamically, so the notices for
    // statically linked runtimes do not belong in this archive.
    for static_only in ["musl", "mingw-w64", "gcc-runtime"] {
        assert!(
            !files
                .iter()
                .any(|file| file.starts_with(&format!("licenses/{static_only}/"))),
            "a dynamically linked archive should not carry licenses/{static_only}"
        );
    }

    assert_eq!(
        fs::read_to_string(
            packaging_workspace
                .path()
                .join("release/bundle/licenses/cfmpeg/LICENSE")
        )
        .expect("failed to read bundled licence"),
        repo_file("LICENSE")
    );

    // Crates compiled into the binary are listed with their licence texts;
    // dev-dependencies and compile-time-only crates are not.
    let crates = fs::read_to_string(
        packaging_workspace
            .path()
            .join("release/bundle/licenses/rust/CRATES.txt"),
    )
    .expect("failed to read crate notices");
    for linked in ["tokio", "reqwest", "ring", "rustls"] {
        assert!(
            crates.contains(&format!("\n{linked} ")),
            "crate notices should list {linked}"
        );
    }
    for not_linked in ["serde_yaml", "flate2", "syn", "serde_derive"] {
        assert!(
            !crates.contains(&format!("\n{not_linked} ")),
            "crate notices should not list {not_linked}"
        );
    }
    assert!(crates.contains("Permission is hereby granted, free of charge"));
    assert!(crates.contains("Apache License"));

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
        Some("${{ needs.prepare.outputs.tag }}")
    );
}

#[test]
fn release_binaries_workflow_builds_the_tag_unless_it_is_a_dry_run() {
    let workflow = workflow(".github/workflows/release-binaries.yml");
    let prepare_job = workflow_job(&workflow, "prepare");
    assert_eq!(
        prepare_job["outputs"]["tag"].as_str(),
        Some("${{ steps.tag.outputs.value }}")
    );
    assert_eq!(
        prepare_job["outputs"]["ref"].as_str(),
        Some("${{ steps.ref.outputs.value }}")
    );

    let resolve_step = workflow_step(prepare_job, "Resolve build ref");
    assert_eq!(
        resolve_step["env"]["DRY_RUN"].as_str(),
        Some("${{ inputs.dry_run }}")
    );
    assert_eq!(
        resolve_step["env"]["TAG"].as_str(),
        Some("${{ steps.tag.outputs.value }}")
    );
    let resolve_script = resolve_step["run"]
        .as_str()
        .expect("resolve build ref step should have a run script");

    for (dry_run, expected) in [
        ("false", "value=refs/tags/v1.2.3\n"),
        ("true", "value=0123456789abcdef0123456789abcdef01234567\n"),
    ] {
        let workspace = tempdir().expect("failed to create resolve tempdir");
        let github_output = workspace.path().join("github-output.txt");
        let output = run_script(
            resolve_script,
            workspace.path(),
            None,
            &[
                ("DRY_RUN", dry_run),
                ("TAG", "v1.2.3"),
                ("GITHUB_SHA", "0123456789abcdef0123456789abcdef01234567"),
                ("GITHUB_OUTPUT", github_output.to_str().unwrap()),
            ],
        );
        assert_success(&output);

        assert_eq!(
            fs::read_to_string(&github_output).expect("failed to read GitHub output"),
            expected,
            "unexpected build ref when dry_run is {dry_run}"
        );
    }

    for job_name in [
        "build-binaries",
        "build-musl-binaries",
        "build-windows-helper",
        "build-windows-binary",
    ] {
        let job = workflow_job(&workflow, job_name);
        let checkout_step = workflow_step(job, "Checkout");
        assert_eq!(
            checkout_step["with"]["ref"].as_str(),
            Some("${{ needs.prepare.outputs.ref }}"),
            "{job_name} should check out the ref resolved by the prepare job"
        );
    }

    for job_name in [
        "build-binaries",
        "build-musl-binaries",
        "build-windows-binary",
    ] {
        let job = workflow_job(&workflow, job_name);

        let upload_step = workflow_step(job, "Upload to GitHub Release");
        assert_eq!(
            upload_step["if"].as_str(),
            Some("${{ !inputs.dry_run }}"),
            "{job_name} must not upload to the release during a dry run"
        );
        assert_eq!(
            upload_step["with"]["tag_name"].as_str(),
            Some("${{ needs.prepare.outputs.tag }}")
        );

        let artifact_step = workflow_step(job, "Keep archive as a workflow artifact");
        assert_eq!(
            artifact_step["if"].as_str(),
            Some("${{ inputs.dry_run }}"),
            "{job_name} should only keep workflow artifacts during a dry run"
        );
    }
}

#[test]
fn release_binaries_workflow_builds_static_musl_archives_in_alpine() {
    let workflow = workflow(".github/workflows/release-binaries.yml");
    let musl_job = workflow_job(&workflow, "build-musl-binaries");

    let matrix = musl_job["strategy"]["matrix"]["include"]
        .as_sequence()
        .expect("musl matrix should be a sequence");
    let targets: Vec<_> = matrix
        .iter()
        .map(|entry| {
            (
                entry["os"].as_str().expect("os should be a string"),
                entry["asset_name"]
                    .as_str()
                    .expect("asset_name should be a string"),
                entry["rust_target"]
                    .as_str()
                    .expect("rust_target should be a string"),
            )
        })
        .collect();

    assert_eq!(
        targets,
        vec![
            (
                "ubuntu-24.04-arm",
                "cfmpeg-linux-arm64-musl",
                "aarch64-unknown-linux-musl"
            ),
            (
                "ubuntu-latest",
                "cfmpeg-linux-x64-musl",
                "x86_64-unknown-linux-musl"
            ),
        ]
    );

    let build_script = workflow_step(musl_job, "Build static binaries in Alpine")["run"]
        .as_str()
        .expect("musl build step should have a run script");
    assert!(build_script.contains("\"$MUSL_BUILD_IMAGE\""));
    assert!(
        build_script.contains("sh scripts/build-musl-bundle.sh \"$RUST_TARGET\" release/bundle")
    );
    assert!(repo_path("scripts/build-musl-bundle.sh").is_file());

    let bundle_script = repo_file("scripts/build-musl-bundle.sh");
    assert!(bundle_script.contains("cargo build --release --locked --target \"$RUST_TARGET\""));
    assert!(bundle_script
        .contains("FFMPEG_HELPER_STATIC=1 bash scripts/build-helper-ffmpeg.sh \"$HELPER_PREFIX\""));

    let prepare_script = workflow_step(musl_job, "Prepare release assets")["run"]
        .as_str()
        .expect("musl prepare assets step should have a run script");
    assert!(prepare_script.contains(
        "-czf \"release/${{ matrix.asset_name }}.tar.gz\" -C release/bundle bin libexec licenses\n"
    ));
    assert!(prepare_script.contains(
        "shasum -a 256 \"release/${{ matrix.asset_name }}.tar.gz\" > \"release/${{ matrix.asset_name }}.tar.gz.sha256\""
    ));

    assert!(bundle_script.contains("python3 scripts/assemble-licenses.py"));
    assert!(bundle_script.contains("--kind musl"));
    assert!(bundle_script.contains("--ffmpeg-licenses \"$HELPER_PREFIX/licenses/ffmpeg\""));
    assert_eq!(
        workflow_step(musl_job, "Verify release archive")["run"].as_str(),
        Some(
            "python3 scripts/verify-release-archive.py \"release/${{ matrix.asset_name }}.tar.gz\" --kind musl"
        )
    );
}

#[test]
fn helper_build_script_records_the_ffmpeg_source_and_how_it_was_built() {
    let workspace = tempdir().expect("failed to create helper build tempdir");

    // A stand-in FFmpeg source release: configure and make are stubbed, and
    // the licence files are the ones the script has to carry over.
    let source = workspace.path().join("source/ffmpeg-9.9");
    fs::create_dir_all(&source).expect("failed to create fake source tree");
    write_executable(&source.join("configure"), "#!/bin/sh\nexit 0\n");
    fs::write(source.join("COPYING.LGPLv2.1"), "LGPL text").expect("failed to write LGPL");
    fs::write(source.join("LICENSE.md"), "FFmpeg licence overview")
        .expect("failed to write LICENSE.md");

    let tarball = workspace.path().join("ffmpeg-9.9.tar.xz");
    let status = Command::new("tar")
        .arg("-cJf")
        .arg(&tarball)
        .arg("-C")
        .arg(workspace.path().join("source"))
        .arg("ffmpeg-9.9")
        .status()
        .expect("failed to run tar");
    assert!(status.success(), "failed to pack the fake source release");

    let checksum = Command::new("shasum")
        .args(["-a", "256"])
        .arg(&tarball)
        .output()
        .expect("failed to run shasum");
    assert_success(&checksum);
    let tarball_sha256 = String::from_utf8_lossy(&checksum.stdout)
        .split_whitespace()
        .next()
        .expect("shasum should print a digest")
        .to_string();

    let stub_bin = workspace.path().join("stub-bin");
    fs::create_dir_all(&stub_bin).expect("failed to create stub bin directory");
    write_executable(
        &stub_bin.join("curl"),
        r#"#!/bin/bash
if [ "$2" != "https://ffmpeg.org/releases/ffmpeg-9.9.tar.xz" ] || [ "$3" != "-o" ]; then
  echo "unexpected curl invocation: $*" >&2
  exit 1
fi
cp "$TEST_TARBALL" "$4"
"#,
    );
    // The linker map lists the archive members a static link pulled in:
    // toolchain libraries by absolute path, FFmpeg's own by relative path.
    write_executable(
        &stub_bin.join("make"),
        r#"#!/bin/bash
for argument in "$@"; do
  case "$argument" in
    LDFLAGS-ffmpeg=-Wl,-Map,*)
      cat > "${argument#LDFLAGS-ffmpeg=-Wl,-Map,}" <<'MAP'
Archive member included to satisfy reference by file (symbol)

/usr/lib/libc.a(printf.o)
                              fftools/ffmpeg.o (printf)
/usr/lib/gcc/libgcc.a(_udivti3.o)
                              libavutil/libavutil.a(mathematics.o) (__udivti3)
libavcodec/libavcodec.a(utils.o)
                              fftools/ffmpeg.o (avcodec_open2)
/usr/lib/libc.a(malloc.o)
                              fftools/ffmpeg.o (malloc)
MAP
      ;;
  esac
done
echo "ffmpeg-binary" > ffmpeg
"#,
    );

    let build = |prefix: &Path, link_statically: bool| {
        let mut command = Command::new("bash");
        command
            .arg(repo_path("scripts/build-helper-ffmpeg.sh"))
            .arg(prefix)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    stub_bin.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .env("FFMPEG_HELPER_VERSION", "9.9")
            .env("TEST_TARBALL", &tarball)
            .env_remove("FFMPEG_HELPER_TARGET")
            .env_remove("FFMPEG_HELPER_STATIC");
        if link_statically {
            command.env("FFMPEG_HELPER_STATIC", "1");
        }
        let output = command
            .output()
            .expect("failed to run build-helper-ffmpeg.sh");
        assert_success(&output);

        assert!(prefix.join("bin/ffmpeg").is_file());
        let licenses = prefix.join("licenses/ffmpeg");
        assert_eq!(
            fs::read_to_string(licenses.join("COPYING.LGPLv2.1")).expect("failed to read LGPL"),
            "LGPL text"
        );
        assert_eq!(
            fs::read_to_string(licenses.join("LICENSE.md")).expect("failed to read LICENSE.md"),
            "FFmpeg licence overview"
        );
        fs::read_to_string(licenses.join("NOTICE.txt")).expect("failed to read NOTICE.txt")
    };

    let dynamic_prefix = workspace.path().join("dynamic");
    let notice = build(&dynamic_prefix, false);
    assert!(notice.contains("libexec/ffmpeg in this archive is FFmpeg 9.9."));
    assert!(notice.contains("Source:   https://ffmpeg.org/releases/ffmpeg-9.9.tar.xz\n"));
    assert!(notice.contains(&format!("SHA-256:  {tarball_sha256}\n")));
    assert!(notice.contains("GNU Lesser General Public License, version 2.1 or later"));
    assert!(notice.contains(&format!(
        "  ./configure\n    --prefix={}\n    --disable-autodetect\n",
        dynamic_prefix.display()
    )));
    assert!(!notice.contains("--extra-ldflags=-static"));
    assert!(notice.contains("scripts/build-helper-ffmpeg.sh"));
    assert!(notice.contains("links the platform's C library dynamically"));

    let notice = build(&workspace.path().join("static"), true);
    assert!(notice.contains("    --extra-ldflags=-static\n"));
    assert!(notice.contains("\n  /usr/lib/gcc/libgcc.a\n  /usr/lib/libc.a\n"));
    assert!(!notice.contains("libavcodec/libavcodec.a"));
    assert!(!notice.contains("dynamically"));
}

#[test]
fn static_musl_archives_must_carry_the_notices_for_what_is_linked_in() {
    let workspace = tempdir().expect("failed to create licence tempdir");
    let bundle = workspace.path().join("bundle");
    fs::create_dir_all(bundle.join("bin")).expect("failed to create bin");
    fs::create_dir_all(bundle.join("libexec")).expect("failed to create libexec");
    fs::write(bundle.join("bin/cfmpeg"), "cfmpeg-binary").expect("failed to write cfmpeg");
    fs::write(bundle.join("libexec/ffmpeg"), "ffmpeg-binary").expect("failed to write ffmpeg");
    let ffmpeg_licenses = workspace.path().join("helper/licenses/ffmpeg");
    write_ffmpeg_licenses(&ffmpeg_licenses);

    let assemble = Command::new("python3")
        .arg(repo_path("scripts/assemble-licenses.py"))
        .arg("--bundle")
        .arg(&bundle)
        .args([
            "--kind",
            "musl",
            "--rust-target",
            "x86_64-unknown-linux-musl",
        ])
        .arg("--ffmpeg-licenses")
        .arg(&ffmpeg_licenses)
        .output()
        .expect("failed to run assemble-licenses.py");
    assert_success(&assemble);

    for vendored in [
        "musl/COPYRIGHT",
        "gcc-runtime/COPYING.RUNTIME",
        "gcc-runtime/COPYING3",
    ] {
        assert_eq!(
            fs::read(bundle.join("licenses").join(vendored)).expect("failed to read notice"),
            fs::read(repo_path("packaging/licenses").join(vendored))
                .expect("failed to read vendored notice"),
            "{vendored} should be the vendored upstream text"
        );
    }
    assert!(!bundle.join("licenses/mingw-w64").exists());

    let package = |archive: &str, members: &[&str]| {
        let status = Command::new("tar")
            .arg("-czf")
            .arg(workspace.path().join(archive))
            .arg("-C")
            .arg(&bundle)
            .args(members)
            .status()
            .expect("failed to run tar");
        assert!(status.success(), "tar failed for {archive}");
    };
    let verify = |archive: &str| {
        Command::new("python3")
            .arg(repo_path("scripts/verify-release-archive.py"))
            .arg(workspace.path().join(archive))
            .args(["--kind", "musl"])
            .output()
            .expect("failed to run verify-release-archive.py")
    };

    package("complete.tar.gz", &["bin", "libexec", "licenses"]);
    assert_success(&verify("complete.tar.gz"));

    package("no-licenses.tar.gz", &["bin", "libexec"]);
    let output = verify("no-licenses.tar.gz");
    assert!(
        !output.status.success(),
        "an archive without notices must fail verification"
    );
    let report = String::from_utf8_lossy(&output.stdout);
    assert!(report.contains("missing licenses/ffmpeg/COPYING.LGPLv2.1"));
    assert!(report.contains("missing licenses/musl/COPYRIGHT"));

    // A notice that no longer says where the FFmpeg source is must also fail.
    fs::write(bundle.join("licenses/ffmpeg/NOTICE.txt"), "FFmpeg helper\n")
        .expect("failed to overwrite NOTICE.txt");
    package("vague-notice.tar.gz", &["bin", "libexec", "licenses"]);
    let output = verify("vague-notice.tar.gz");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("does not mention"));
}

#[test]
fn release_binaries_workflow_packages_the_windows_zip() {
    let workflow = workflow(".github/workflows/release-binaries.yml");

    let helper_job = workflow_job(&workflow, "build-windows-helper");
    let cross_compile_step = workflow_step(helper_job, "Cross-compile the helper ffmpeg");
    assert_eq!(
        cross_compile_step["env"]["FFMPEG_HELPER_TARGET"].as_str(),
        Some("windows-x64")
    );
    let hand_off_step = workflow_step(helper_job, "Hand the helper to the Windows build");
    let helper_artifact = hand_off_step["with"]["name"]
        .as_str()
        .expect("helper artifact should be named");

    let windows_job = workflow_job(&workflow, "build-windows-binary");
    assert_eq!(windows_job["runs-on"].as_str(), Some("windows-latest"));
    assert_eq!(
        windows_job["env"]["ASSET_NAME"].as_str(),
        Some("cfmpeg-windows-x64")
    );
    let rust_target = windows_job["env"]["RUST_TARGET"]
        .as_str()
        .expect("RUST_TARGET should be a string");
    assert_eq!(rust_target, "x86_64-pc-windows-msvc");

    let download_step = workflow_step(windows_job, "Download the helper ffmpeg");
    assert_eq!(
        download_step["with"]["name"].as_str(),
        Some(helper_artifact)
    );
    assert_eq!(
        download_step["with"]["path"].as_str(),
        Some("windows-helper")
    );

    let workspace = tempdir().expect("failed to create packaging tempdir");
    let release_dir = workspace
        .path()
        .join("target")
        .join(rust_target)
        .join("release");
    fs::create_dir_all(&release_dir).expect("failed to create target release directory");
    fs::create_dir_all(workspace.path().join("windows-helper"))
        .expect("failed to create helper directory");
    fs::write(release_dir.join("cfmpeg.exe"), "cfmpeg-binary").expect("failed to write cfmpeg.exe");
    fs::write(
        workspace.path().join("windows-helper/ffmpeg.exe"),
        "ffmpeg-binary",
    )
    .expect("failed to write ffmpeg.exe");
    write_ffmpeg_licenses(&workspace.path().join("windows-helper/licenses/ffmpeg"));

    let hand_off_script = cross_compile_step["run"]
        .as_str()
        .expect("cross-compile step should have a run script");
    assert!(hand_off_script
        .contains("cp -R \"$RUNNER_TEMP/ffmpeg-helper/licenses\" windows-helper/licenses"));
    assert_eq!(
        hand_off_step["with"]["path"].as_str(),
        Some("windows-helper/")
    );

    // The runner's 7z, sha256sum and python are stood in for by tools every
    // Unix development machine has.
    let stub_bin = workspace.path().join("stub-bin");
    fs::create_dir_all(&stub_bin).expect("failed to create stub bin directory");
    write_executable(
        &stub_bin.join("7z"),
        r#"#!/bin/bash
if [ "$1" != "a" ] || [ "$2" != "-tzip" ]; then
  echo "unexpected 7z invocation: $*" >&2
  exit 1
fi
shift 2
archive="$1"
shift
exec zip -qr "$archive" "$@"
"#,
    );
    write_executable(
        &stub_bin.join("sha256sum"),
        r#"#!/bin/bash
exec shasum -a 256 "$@"
"#,
    );
    write_executable(
        &stub_bin.join("python"),
        r#"#!/bin/bash
exec python3 "$@"
"#,
    );

    let envs = [
        ("ASSET_NAME", "cfmpeg-windows-x64"),
        ("RUST_TARGET", rust_target),
    ];
    for step_name in [
        "Assemble bundle",
        "Prepare release assets",
        "Verify release archive",
    ] {
        let script = workflow_step(windows_job, step_name)["run"]
            .as_str()
            .unwrap_or_else(|| panic!("{step_name} step should have a run script"));
        let output = run_script(
            &with_repo_scripts(script),
            workspace.path(),
            Some(&stub_bin),
            &envs,
        );
        assert_success(&output);
    }

    let archive_path = workspace.path().join("release/cfmpeg-windows-x64.zip");
    let listing = Command::new("unzip")
        .arg("-Z1")
        .arg(&archive_path)
        .output()
        .expect("failed to list the packaged zip");
    assert_success(&listing);

    let mut files = String::from_utf8_lossy(&listing.stdout)
        .lines()
        .filter(|entry| !entry.ends_with('/'))
        .map(str::to_string)
        .collect::<Vec<_>>();
    files.sort();

    assert_eq!(
        top_level_directories(&files),
        vec!["bin", "libexec", "licenses"]
    );
    assert_contains_all(
        &files,
        &[
            "bin/cfmpeg.exe",
            "libexec/ffmpeg.exe",
            "licenses/README.txt",
            "licenses/cfmpeg/LICENSE",
            "licenses/ffmpeg/COPYING.LGPLv2.1",
            "licenses/ffmpeg/NOTICE.txt",
            "licenses/rust/CRATES.txt",
            "licenses/mingw-w64/COPYING.MinGW-w64-runtime.txt",
            "licenses/mingw-w64/COPYING.winpthreads",
            "licenses/mingw-w64/DISCLAIMER",
            "licenses/gcc-runtime/COPYING.RUNTIME",
            "licenses/gcc-runtime/COPYING3",
        ],
    );
    assert!(
        !files.iter().any(|file| file.starts_with("licenses/musl/")),
        "the Windows archive links no musl"
    );

    let checksum = fs::read_to_string(
        workspace
            .path()
            .join("release/cfmpeg-windows-x64.zip.sha256"),
    )
    .expect("failed to read checksum file");
    let (sha, file_name) = checksum
        .split_once("  ")
        .expect("checksum file should contain two-column shasum output");
    assert_eq!(sha.len(), 64, "checksum should be a SHA-256 digest");
    assert!(sha.chars().all(|character| character.is_ascii_hexdigit()));
    assert_eq!(file_name, "release/cfmpeg-windows-x64.zip\n");

    let upload_step = workflow_step(windows_job, "Upload to GitHub Release");
    let uploaded_files = upload_step["with"]["files"]
        .as_str()
        .expect("upload step should list files");
    assert!(uploaded_files.contains("release/${{ env.ASSET_NAME }}.zip\n"));
    assert!(uploaded_files.contains("release/${{ env.ASSET_NAME }}.zip.sha256"));
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

    let workspace = tempdir().expect("failed to create crate check tempdir");
    let stub_bin = workspace.path().join("bin");
    fs::create_dir_all(&stub_bin).expect("failed to create stub bin directory");
    write_executable(
        &stub_bin.join("cargo"),
        r#"#!/bin/sh
if [ "$1" = "info" ] && [ "$2" = "cfmpeg@0.1.0" ] && [ "$3" = "--color" ] && [ "$4" = "never" ]; then
  echo 'version: 0.1.0'
  exit 0
fi

if [ "$1" = "info" ] && [ "$2" = "cfmpeg@0.2.0" ] && [ "$3" = "--color" ] && [ "$4" = "never" ]; then
  echo 'error: could not find `cfmpeg@0.2.0` in registry `https://github.com/rust-lang/crates.io-index`' >&2
  exit 101
fi

echo "unexpected cargo invocation: $*" >&2
exit 1
"#,
    );
    let github_output = workspace.path().join("github-output.txt");
    let existing_crate_script_for_existing_version = render(
        existing_crate_script,
        &[("${{ needs.release.outputs.version }}", "0.1.0")],
    );
    let output = run_script(
        &existing_crate_script_for_existing_version,
        workspace.path(),
        Some(&stub_bin),
        &[("GITHUB_OUTPUT", github_output.to_str().unwrap())],
    );
    assert_success(&output);

    assert_eq!(
        fs::read_to_string(&github_output).expect("failed to read GitHub output"),
        "exists=true\n"
    );

    let missing_github_output = workspace.path().join("missing-github-output.txt");
    let missing_crate_script = render(
        existing_crate_script,
        &[("${{ needs.release.outputs.version }}", "0.2.0")],
    );
    let output = run_script(
        &missing_crate_script,
        workspace.path(),
        Some(&stub_bin),
        &[("GITHUB_OUTPUT", missing_github_output.to_str().unwrap())],
    );
    assert_success(&output);

    assert_eq!(
        fs::read_to_string(&missing_github_output).expect("failed to read GitHub output"),
        "exists=false\n"
    );

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

    // `brew install cfmpeg/tap/cfmpeg` resolves to the repository
    // cfmpeg/homebrew-tap, so that is where the formula has to be pushed.
    let tap_owner = update_formula_step["env"]["HOMEBREW_TAP_OWNER"]
        .as_str()
        .expect("HOMEBREW_TAP_OWNER should be a string");
    let tap_repo = update_formula_step["env"]["HOMEBREW_TAP_REPO"]
        .as_str()
        .expect("HOMEBREW_TAP_REPO should be a string");
    assert_eq!((tap_owner, tap_repo), ("cfmpeg", "homebrew-tap"));

    let smoke_test_job = workflow_job(&workflow, "smoke-test-homebrew");
    assert_eq!(
        smoke_test_job["env"]["HOMEBREW_TAP"].as_str(),
        Some("cfmpeg/tap")
    );

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

if [ "$1" = "diff" ] && [ "$2" = "--cached" ] && [ "$3" = "--quiet" ]; then
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
            ("REPOSITORY", "cfmpeg/cfmpeg-cli"),
            ("HOMEBREW_TAP_OWNER", tap_owner),
            ("HOMEBREW_TAP_REPO", tap_repo),
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
        "url \"https://github.com/cfmpeg/cfmpeg-cli/releases/download/v1.2.3/cfmpeg-darwin-arm64.tar.gz\""
    ));
    assert!(formula
        .contains("sha256 \"1111111111111111111111111111111111111111111111111111111111111111\""));
    assert!(formula.contains("libexec.install \"libexec/ffmpeg\"\n"));
    // The licence notices in the archive are installed with the keg.
    assert!(formula.contains("doc.install \"licenses\"\n"));
    assert!(
        formula.contains("assert_predicate doc/\"licenses/ffmpeg/COPYING.LGPLv2.1\", :exist?\n")
    );
    assert!(
        formula.contains("assert_match version.to_s, shell_output(\"#{bin}/cfmpeg --version\")")
    );

    let git_log = fs::read_to_string(&log_path).expect("failed to read git log");
    assert!(git_log
        .contains("clone https://x-access-token:test-token@github.com/cfmpeg/homebrew-tap.git"));
    assert!(git_log.contains("add Formula/cfmpeg.rb"));
    assert!(git_log.contains("diff --cached --quiet -- Formula/cfmpeg.rb"));
    assert!(git_log.contains("commit -m cfmpeg 1.2.3"));
    assert!(git_log.contains("push origin main"));

    let add_index = git_log
        .find("add Formula/cfmpeg.rb")
        .expect("formula should be staged before diffing");
    let diff_index = git_log
        .find("diff --cached --quiet -- Formula/cfmpeg.rb")
        .expect("staged formula should be checked for changes");
    assert!(
        add_index < diff_index,
        "formula must be staged before the no-op diff so new formula files are committed"
    );
}

#[test]
fn homebrew_smoke_test_requires_the_bundled_helper_through_the_brew_symlink() {
    let workflow = workflow(".github/workflows/release.yml");
    let smoke_test_job = workflow_job(&workflow, "smoke-test-homebrew");
    let smoke_test_script = workflow_step(smoke_test_job, "Smoke test Homebrew install")["run"]
        .as_str()
        .expect("smoke test step should have a run script");

    // A miniature Homebrew prefix: the keg holds the real files, and
    // <prefix>/bin/cfmpeg is only a symlink into it.
    let workspace = tempdir().expect("failed to create workflow tempdir");
    let prefix = workspace.path().join("prefix");
    let keg = prefix.join("Cellar/cfmpeg/1.2.3");
    fs::create_dir_all(keg.join("bin")).expect("failed to create keg bin");
    fs::create_dir_all(keg.join("libexec")).expect("failed to create keg libexec");
    fs::create_dir_all(prefix.join("bin")).expect("failed to create prefix bin");
    fs::create_dir_all(prefix.join("opt")).expect("failed to create prefix opt");
    let installed_licenses = keg.join("share/doc/cfmpeg/licenses");
    fs::create_dir_all(installed_licenses.join("cfmpeg")).expect("failed to create licences");
    fs::write(installed_licenses.join("cfmpeg/LICENSE"), "MIT").expect("failed to write licence");
    write_ffmpeg_licenses(&installed_licenses.join("ffmpeg"));

    write_executable(
        &keg.join("libexec/ffmpeg"),
        r#"#!/bin/bash
echo "ffmpeg version 8.0 bundled helper"
"#,
    );
    write_executable(
        &keg.join("bin/cfmpeg"),
        r#"#!/bin/bash
if [ "$1" = "--version" ]; then
  echo "cfmpeg 1.2.3"
  exit 0
fi

if [ "$1" = "--local" ] && [ "$2" = "-version" ]; then
  if [ "$TEST_RESOLVES_SYMLINK" = "true" ]; then
    exec "$TEST_KEG/libexec/ffmpeg" -version
  fi
  echo "error: ffmpeg was not found" >&2
  exit 1
fi

echo "unexpected cfmpeg invocation: $*" >&2
exit 1
"#,
    );
    std::os::unix::fs::symlink(&keg, prefix.join("opt/cfmpeg")).expect("failed to link opt");
    std::os::unix::fs::symlink(keg.join("bin/cfmpeg"), prefix.join("bin/cfmpeg"))
        .expect("failed to link cfmpeg");

    write_executable(
        &prefix.join("bin/brew"),
        r#"#!/bin/bash
if [ "$1" = "--prefix" ] && [ "$#" -eq 1 ]; then
  echo "$TEST_BREW_PREFIX"
  exit 0
fi

if [ "$1" = "list" ] && [ "$2" = "--versions" ]; then
  echo "cfmpeg 1.2.3"
  exit 0
fi

case "$1" in
  tap | install | upgrade | uninstall) exit 0 ;;
esac

echo "unexpected brew invocation: $*" >&2
exit 1
"#,
    );

    let run = |resolves_symlink: &str| {
        run_script(
            smoke_test_script,
            workspace.path(),
            Some(&prefix.join("bin")),
            &[
                ("VERSION", "1.2.3"),
                ("HOMEBREW_TAP", "cfmpeg/tap"),
                ("TEST_BREW_PREFIX", prefix.to_str().unwrap()),
                ("TEST_KEG", keg.to_str().unwrap()),
                ("TEST_RESOLVES_SYMLINK", resolves_symlink),
            ],
        )
    };

    assert_success(&run("true"));

    let output = run("false");
    assert!(
        !output.status.success(),
        "the smoke test must fail when cfmpeg cannot reach its bundled helper"
    );
    assert!(String::from_utf8_lossy(&output.stdout)
        .contains("cfmpeg did not run its bundled helper ffmpeg"));

    // A keg without the licence notices fails as well.
    fs::remove_file(installed_licenses.join("ffmpeg/NOTICE.txt"))
        .expect("failed to remove NOTICE.txt");
    assert!(
        !run("true").status.success(),
        "the smoke test must fail when the keg has no FFmpeg notice"
    );
}

#[test]
fn readme_uses_the_cfmpeg_homebrew_namespace() {
    let readme = repo_file("README.md");

    assert!(readme.contains("\nbrew install cfmpeg/tap/cfmpeg\n"));
    assert!(!readme.contains("brew tap "));
    assert!(readme.contains("\ncurl -fsSL https://cfmpeg.com/install | sh\n"));
}

#[test]
fn readme_lists_every_release_archive() {
    let readme = repo_file("README.md");
    let workflow = workflow(".github/workflows/release-binaries.yml");

    let mut archives = Vec::new();
    for job_name in ["build-binaries", "build-musl-binaries"] {
        let matrix = workflow_job(&workflow, job_name)["strategy"]["matrix"]["include"]
            .as_sequence()
            .unwrap_or_else(|| panic!("{job_name} matrix should be a sequence"));
        for entry in matrix {
            let asset_name = entry["asset_name"]
                .as_str()
                .expect("asset_name should be a string");
            archives.push(format!("{asset_name}.tar.gz"));
        }
    }
    let windows_asset = workflow_job(&workflow, "build-windows-binary")["env"]["ASSET_NAME"]
        .as_str()
        .expect("ASSET_NAME should be a string");
    archives.push(format!("{windows_asset}.zip"));

    assert_eq!(archives.len(), 7);
    for archive in archives {
        assert!(
            readme.contains(&format!("`{archive}`")),
            "README install section should list {archive}"
        );
    }
}

#[test]
fn public_metadata_uses_the_cfmpeg_cli_repository() {
    let cargo_toml = repo_file("Cargo.toml");
    let release_workflow = repo_file(".github/workflows/release.yml");

    assert!(cargo_toml.contains(r#"repository = "https://github.com/cfmpeg/cfmpeg-cli""#));
    assert!(release_workflow.contains("github.repository == 'cfmpeg/cfmpeg-cli'"));
}
