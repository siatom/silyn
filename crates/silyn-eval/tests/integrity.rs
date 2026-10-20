//! Suite integrity helper tests.
//!
//! `cargo run -p xtask -- verify-eval-integrity` is the CI gate; this mirrors
//! its rules against a temporary suite so tampering is proven to be detected.

use std::fs;
use std::path::Path;

use silyn_eval::integrity::{sha256_file, sha256_hex, verify_suite_lock};

#[test]
fn changed_artifact_fails_integrity_check() {
    let temp = tempfile::tempdir().expect("temp dir");
    let suite = temp.path().join("csv-stats-v1");
    fs::create_dir(&suite).expect("create suite dir");
    fs::write(suite.join("suite.toml"), "suite_id = \"csv-stats-v1\"\n").expect("write manifest");
    fs::write(suite.join("README.md"), "# suite\n").expect("write readme");

    let manifest_hash = sha256_file(&suite.join("suite.toml")).expect("hash manifest");
    let readme_hash = sha256_file(&suite.join("README.md")).expect("hash readme");
    fs::write(
        suite.join("suite.lock"),
        format!("{manifest_hash}  suite.toml\n{readme_hash}  README.md\n"),
    )
    .expect("write lock");

    assert_eq!(verify_suite_lock(&suite).expect("intact lock"), 2);

    // Editing a sealed file must fail with the sealing message.
    fs::write(suite.join("README.md"), "# suite edited after sealing\n").expect("edit readme");
    let error = verify_suite_lock(&suite).expect_err("tampering must fail");
    assert!(
        error.to_string().contains("changed after sealing"),
        "unexpected error: {error}"
    );

    // Removing a sealed file is a failure too, not a skipped line.
    fs::remove_file(suite.join("suite.toml")).expect("remove manifest");
    let error = verify_suite_lock(&suite).expect_err("missing artifact must fail");
    assert!(
        error.to_string().contains("missing"),
        "unexpected error: {error}"
    );
}

#[test]
fn repository_suite_lock_matches_sealed_files() {
    let suite = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../eval/suites/csv-stats-v1")
        .canonicalize()
        .expect("suite dir");
    assert_eq!(verify_suite_lock(&suite).expect("lock is intact"), 2);
}

#[test]
fn sha256_hex_matches_known_vector() {
    assert_eq!(
        sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}
