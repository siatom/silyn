//! Sealed-artifact hashing.
//!
//! `cargo run -p xtask -- verify-eval-integrity` is the authoritative gate. This
//! module mirrors its rules so the harness can be tested in isolation: every
//! line of `suite.lock` is `<sha256>  <path-relative-to-suite-dir>`, and any
//! change to a listed file is a hard failure.

use std::path::Path;

use sha2::{Digest, Sha256};
use silyn_core::SilynError;

/// Lowercase hex sha256 of a byte slice.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Lowercase hex sha256 of a file.
pub fn sha256_file(path: &Path) -> Result<String, SilynError> {
    let bytes = std::fs::read(path).map_err(|error| {
        SilynError::Permanent(format!("read artifact {}: {error}", path.display()))
    })?;
    Ok(sha256_hex(&bytes))
}

/// Verifies every entry in `<suite_dir>/suite.lock` and returns the entry count.
///
/// A missing lock file is an error here (unlike xtask, which skips unsealed
/// suites). Tests use this to prove tampering is detected.
pub fn verify_suite_lock(suite_dir: &Path) -> Result<usize, SilynError> {
    let lock_path = suite_dir.join("suite.lock");
    let lock = std::fs::read_to_string(&lock_path)
        .map_err(|error| SilynError::Permanent(format!("read {}: {error}", lock_path.display())))?;

    let mut verified = 0;
    for line in lock.lines().filter(|line| !line.trim().is_empty()) {
        let (expected, relative) = line
            .split_once("  ")
            .ok_or_else(|| SilynError::Schema(format!("malformed suite.lock line: {line}")))?;
        let path = suite_dir.join(relative);
        let actual = sha256_file(&path).map_err(|error| {
            SilynError::Permanent(format!(
                "suite artifact missing: {} ({error})",
                path.display()
            ))
        })?;
        if actual != expected {
            return Err(SilynError::Permanent(format!(
                "suite artifact `{}` changed after sealing; create a new suite version",
                path.display()
            )));
        }
        verified += 1;
    }

    if verified == 0 {
        return Err(SilynError::Schema(format!(
            "{} lists no sealed artifacts",
            lock_path.display()
        )));
    }
    Ok(verified)
}
