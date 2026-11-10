//! The checked-in option table has to match what the generator produces from
//! the checked-in dumps. Needs `python3`; no ffmpeg is run.
#![cfg(unix)]

use std::fs;
use std::path::Path;
use std::process::Command;

#[test]
fn table_is_what_the_generator_writes_from_the_checked_in_dumps() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    let scratch = tempfile::tempdir().expect("temp dir");
    let regenerated = scratch.path().join("table.rs");

    let output = Command::new("python3")
        .arg(repo.join("scripts/generate-ffmpeg-options.py"))
        .arg("--output")
        .arg(&regenerated)
        .output()
        .expect("python3 is needed to check the ffmpeg option table");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        fs::read_to_string(&regenerated).expect("regenerated table")
            == fs::read_to_string(repo.join("src/ffmpeg_options/table.rs")).expect("table"),
        "src/ffmpeg_options/table.rs is stale; run scripts/generate-ffmpeg-options.py"
    );
}
