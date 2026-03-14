use std::fs;
use std::path::PathBuf;

fn repo_file(path: &str) -> String {
    let full_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path);

    fs::read_to_string(&full_path)
        .unwrap_or_else(|error| panic!("failed to read {}: {}", full_path.display(), error))
}

#[test]
fn release_binaries_workflow_builds_all_homebrew_assets() {
    let workflow = repo_file(".github/workflows/release-binaries.yml");

    assert!(workflow.contains("cfmpeg-darwin-arm64"));
    assert!(workflow.contains("cfmpeg-darwin-x64"));
    assert!(workflow.contains("cfmpeg-linux-arm64"));
    assert!(workflow.contains("cfmpeg-linux-x64"));
    assert!(workflow.contains("softprops/action-gh-release@v2"));
}

#[test]
fn release_workflow_updates_the_homebrew_tap() {
    let workflow = repo_file(".github/workflows/release.yml");

    assert!(workflow.contains("HOMEBREW_TAP_TOKEN"));
    assert!(workflow.contains("Formula/cfmpeg.rb"));
    assert!(workflow.contains("homebrew-tap"));
    assert!(workflow.contains("brew install"));
    assert!(workflow.contains("cfmpeg --version"));
}
