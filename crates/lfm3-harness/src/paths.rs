//! Confined workspace path resolution shared by tools and checks.
//!
//! Every model-supplied path is relative to one canonical workspace root.
//! Parent components, absolute paths, reserved device names, link and
//! reparse-point traversal, and canonical escapes are rejected before any
//! caller can touch the filesystem.

use std::path::{Component, Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Mode {
    Read,
    List,
    Write,
}

#[derive(Debug)]
pub(crate) enum PathError {
    Outside,
    Invalid(String),
    Reparse(String),
    NotFound,
    NotAFile,
    NotADirectory,
    Io(String),
}

/// Returns the workspace-relative suffix when `raw` names a path inside the
/// canonical root, accepting either separator and a `\\?\` verbatim prefix.
fn strip_workspace_root(root: &Path, raw: &str) -> Option<String> {
    let without_verbatim = |value: &str| value.strip_prefix("//?/").unwrap_or(value).to_owned();
    let normalized_raw = without_verbatim(&raw.replace('\\', "/"));
    let normalized_root = without_verbatim(&root.to_string_lossy().replace('\\', "/"));
    let normalized_root = normalized_root.trim_end_matches('/');
    if normalized_root.is_empty() {
        return None;
    }
    let rest = normalized_raw
        .to_ascii_lowercase()
        .strip_prefix(&normalized_root.to_ascii_lowercase())?
        .strip_prefix('/')?
        .to_owned();
    if rest.is_empty() {
        return None;
    }
    Some(normalized_raw[normalized_raw.len() - rest.len()..].to_owned())
}

/// Resolves `raw` inside an already-canonical `root`.
pub(crate) fn resolve(root: &Path, raw: &str, mode: Mode) -> Result<PathBuf, PathError> {
    if matches!(mode, Mode::Write) && raw.trim().is_empty() {
        return Err(PathError::Invalid("a write needs a file path".to_owned()));
    }
    // Models sometimes echo the trusted workspace root in front of a
    // relative path. Treating that exact prefix as relative is safe: any
    // other absolute path is still rejected below.
    let inside;
    let raw = match strip_workspace_root(root, raw) {
        Some(relative) => {
            inside = relative;
            inside.as_str()
        }
        None => raw,
    };
    let mut parts: Vec<String> = Vec::new();
    for component in Path::new(raw).components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => {
                let value = part
                    .to_str()
                    .ok_or_else(|| PathError::Invalid("path is not valid UTF-8".to_owned()))?;
                validate_component(value)?;
                parts.push(value.to_owned());
            }
            _ => return Err(PathError::Outside),
        }
    }

    let mut current = root.to_path_buf();
    let mut missing = false;
    for part in &parts {
        current.push(part);
        if missing {
            // Deeper components cannot exist yet; keep building the full
            // path so a write lands on the requested file, not its parent.
            continue;
        }
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if is_link(&metadata) {
                    return Err(PathError::Reparse(relative_display(root, &current)));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => missing = true,
            Err(error) => return Err(PathError::Io(error.to_string())),
        }
    }

    if current.exists() {
        let canonical =
            std::fs::canonicalize(&current).map_err(|error| PathError::Io(error.to_string()))?;
        if !canonical.starts_with(root) {
            return Err(PathError::Outside);
        }
    }

    match mode {
        Mode::Read => {
            if !current.exists() {
                return Err(PathError::NotFound);
            }
            if !current.is_file() {
                return Err(PathError::NotAFile);
            }
        }
        Mode::List => {
            if !current.exists() {
                return Err(PathError::NotFound);
            }
            if !current.is_dir() {
                return Err(PathError::NotADirectory);
            }
        }
        Mode::Write => {
            if current.exists() && current.is_dir() {
                return Err(PathError::NotAFile);
            }
        }
    }
    Ok(current)
}

/// A display path relative to the workspace, for messages only.
pub(crate) fn relative_display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn validate_component(part: &str) -> Result<(), PathError> {
    if part.is_empty() {
        return Err(PathError::Invalid("empty path component".to_owned()));
    }
    if part.ends_with('.') || part.ends_with(' ') {
        return Err(PathError::Invalid(
            "Windows strips trailing dots and spaces from components".to_owned(),
        ));
    }
    if is_reserved_name(part) {
        return Err(PathError::Invalid(format!(
            "{part:?} is a reserved device name"
        )));
    }
    Ok(())
}

fn is_reserved_name(part: &str) -> bool {
    let stem = part.split('.').next().unwrap_or(part).trim_end();
    let upper = stem.to_ascii_uppercase();
    if matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }
    upper.len() == 4
        && (upper.starts_with("COM") || upper.starts_with("LPT"))
        && upper.as_bytes()[3].is_ascii_digit()
}

#[cfg(windows)]
pub(crate) fn is_link(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
pub(crate) fn is_link(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(test)]
mod tests {
    use super::{resolve, Mode};

    #[test]
    fn writes_into_missing_directories_keep_the_full_path() {
        let directory = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        let path = resolve(&root, "out/summary.md", Mode::Write).unwrap();
        assert_eq!(path, root.join("out").join("summary.md"));
        assert!(!path.parent().unwrap().exists());
    }

    #[test]
    fn deep_writes_below_a_missing_directory_keep_every_component() {
        let directory = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        let path = resolve(&root, "a/b/c.txt", Mode::Write).unwrap();
        assert_eq!(path, root.join("a").join("b").join("c.txt"));
    }

    #[test]
    fn reads_of_missing_nested_files_report_not_found() {
        let directory = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        assert!(matches!(
            resolve(&root, "out/summary.md", Mode::Read),
            Err(super::PathError::NotFound)
        ));
    }

    #[test]
    fn the_workspace_root_prefix_is_accepted_as_relative() {
        let directory = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        let echoed = format!("{}\\out\\summary.md", root.display());
        let expected = root.join("out").join("summary.md");
        assert_eq!(resolve(&root, &echoed, Mode::Write).unwrap(), expected);
        let plain = root.display().to_string();
        let plain = plain.strip_prefix("\\\\?\\").unwrap_or(&plain);
        let verbatim = format!("\\\\?\\{plain}\\out\\summary.md");
        assert_eq!(resolve(&root, &verbatim, Mode::Write).unwrap(), expected);
    }

    #[test]
    fn absolute_paths_outside_the_workspace_stay_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        let outside = if cfg!(windows) {
            "C:\\Windows\\system.ini"
        } else {
            "/etc/passwd"
        };
        assert!(matches!(
            resolve(&root, outside, Mode::Read),
            Err(super::PathError::Outside)
        ));
    }
}
