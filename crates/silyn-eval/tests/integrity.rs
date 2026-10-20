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

#[test]
fn hashes_match_manifest_digests() {
    use silyn_eval::generator::{SUITE_ID, split_sha256};
    use silyn_eval::manifest::SuiteManifest;
    use silyn_eval::task::Split;

    let suite = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../eval/suites/csv-stats-v1")
        .canonicalize()
        .expect("suite dir");
    let manifest = SuiteManifest::load(&suite.join("suite.toml")).expect("manifest");
    manifest.validate().expect("manifest validates");
    assert_eq!(manifest.suite_id, SUITE_ID);
    assert!(manifest.sealed, "csv-stats-v1.1 must be sealed");

    // `silyn-eval hashes` prints exactly these digests and fails when they drift.
    for split in Split::ALL {
        let digest = split_sha256(split).expect("split digest");
        assert_eq!(
            digest,
            manifest.split_entry(split).sha256,
            "{split} digest does not match suite.toml"
        );
        assert_eq!(manifest.split_entry(split).count, split.count());
    }

    // The lock covers every committed artifact of the suite directory. Only the
    // manifest and the README are committed there; fixtures are generated.
    let lock = std::fs::read_to_string(suite.join("suite.lock")).expect("lock");
    let listed: Vec<&str> = lock
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.split_once("  ").expect("lock line").1)
        .collect();
    assert_eq!(listed, vec!["suite.toml", "README.md"]);
    assert_eq!(
        verify_suite_lock(&suite).expect("intact lock"),
        listed.len()
    );

    // A generated fixture path must not be sealed: the generator is the source
    // of truth, and a fixture would duplicate the digest without adding proof.
    assert!(!listed.iter().any(|path| path.contains("fixture")));
}
