//! Path sandbox for tool file access.
//!
//! Tool arguments only ever name paths relative to the run root. Everything else
//! (absolute paths, drive prefixes, UNC paths, `..` components, Windows alternate
//! data streams and symlinks that leave the root) is rejected before the path is
//! used.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use silyn_core::error::SilynError;

// ---------------------------------------------------------------------------
// Argument validation
// ---------------------------------------------------------------------------

/// Validates a tool-supplied path and returns it as a normalized relative path.
///
/// The returned path contains only `Normal` components and is safe to join onto
/// the run root. `.` components are dropped; a trailing separator is ignored.
pub fn normalize_relative(raw: &str) -> Result<PathBuf, SilynError> {
    if raw.trim().is_empty() {
        return Err(invalid(raw, "path must not be empty"));
    }
    if raw.contains('\0') {
        return Err(invalid(raw, "path must not contain NUL bytes"));
    }
    if raw.starts_with('/') || raw.starts_with('\\') {
        return Err(invalid(raw, "absolute and UNC paths are not allowed"));
    }

    let segments: Vec<&str> = raw.split(['/', '\\']).collect();
    let last = segments.len().saturating_sub(1);
    for (index, segment) in segments.iter().enumerate() {
        if segment.is_empty() {
            // A trailing separator is harmless; an empty segment elsewhere is not.
            if index == last {
                continue;
            }
            return Err(invalid(raw, "path contains an empty component"));
        }
        if *segment == ".." {
            return Err(invalid(raw, "parent directory components are not allowed"));
        }
        if segment.contains(':') {
            return Err(invalid(
                raw,
                "drive prefixes and alternate data streams are not allowed",
            ));
        }
    }

    let mut normalized = PathBuf::new();
    for component in Path::new(raw).components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            _ => {
                return Err(invalid(
                    raw,
                    "path must be relative and free of parent components",
                ));
            }
        }
    }

    Ok(normalized)
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// Resolves a path that must already exist inside `root`.
///
/// The root and the candidate are canonicalized with `dunce`; a candidate that
/// resolves outside the root (including through a symlink) is rejected.
pub fn resolve_existing(root: &Path, raw: &str) -> Result<PathBuf, SilynError> {
    let root = canonical_root(root)?;
    let relative = normalize_relative(raw)?;
    let candidate = if relative.as_os_str().is_empty() {
        root.clone()
    } else {
        root.join(&relative)
    };

    let resolved = dunce::canonicalize(&candidate).map_err(|_| {
        SilynError::Permanent(format!(
            "tool path `{raw}` does not exist or is not accessible"
        ))
    })?;

    if !resolved.starts_with(&root) {
        return Err(escape(raw));
    }

    Ok(resolved)
}

/// Resolves a path for a write, creating no directories.
///
/// The parent directory must exist and must canonicalize inside `root`. When the
/// target already exists (including as a symlink) it is canonicalized and checked
/// as well, so a symlink cannot be used to leave the sandbox.
pub fn resolve_for_write(root: &Path, raw: &str) -> Result<PathBuf, SilynError> {
    let root = canonical_root(root)?;
    let relative = normalize_relative(raw)?;
    let file_name = relative
        .file_name()
        .ok_or_else(|| invalid(raw, "path must name a file"))?;

    let parent_candidate = match relative
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        Some(parent) => root.join(parent),
        None => root.clone(),
    };
    let parent = dunce::canonicalize(&parent_candidate).map_err(|_| {
        SilynError::Permanent(format!("parent directory of `{raw}` does not exist"))
    })?;

    if !parent.starts_with(&root) {
        return Err(escape(raw));
    }
    if !parent.is_dir() {
        return Err(invalid(raw, "parent is not a directory"));
    }

    let target = parent.join(file_name);
    match fs::symlink_metadata(&target) {
        Ok(_) => {
            let resolved = dunce::canonicalize(&target).map_err(|_| {
                SilynError::Permanent(format!("tool path `{raw}` is not accessible"))
            })?;
            if !resolved.starts_with(&root) {
                return Err(escape(raw));
            }
            Ok(resolved)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(target),
        Err(_) => Err(SilynError::Permanent(format!(
            "tool path `{raw}` is not accessible"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn canonical_root(root: &Path) -> Result<PathBuf, SilynError> {
    dunce::canonicalize(root)
        .map_err(|_| SilynError::Permanent("run root is not accessible".to_owned()))
}

fn invalid(raw: &str, reason: &str) -> SilynError {
    SilynError::Permanent(format!("invalid tool path `{raw}`: {reason}"))
}

fn escape(raw: &str) -> SilynError {
    SilynError::Permanent(format!("tool path `{raw}` escapes the run root"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_relative_paths() {
        assert_eq!(
            normalize_relative("notes/hello.txt").expect("valid"),
            PathBuf::from("notes").join("hello.txt")
        );
        assert_eq!(
            normalize_relative("./notes/").expect("valid"),
            PathBuf::from("notes")
        );
        assert!(
            normalize_relative(".")
                .expect("valid")
                .as_os_str()
                .is_empty()
        );
    }

    #[test]
    fn rejects_traversal_and_absolute_paths() {
        for raw in [
            "..",
            "../secret.txt",
            "notes/../../secret.txt",
            "..\\secret.txt",
            "/etc/passwd",
            "\\\\server\\share\\file",
            "C:\\Windows\\win.ini",
            "notes/file.txt:stream",
            "notes//file.txt",
        ] {
            assert!(normalize_relative(raw).is_err(), "must reject `{raw}`");
        }
    }
}
