//! Workspace-confined file tools with atomic writes and hashed results.

use std::{
    fs as std_fs,
    io::Write,
    path::{Path, PathBuf},
    time::Instant,
};

use serde::Deserialize;

use super::{
    check_cancelled, clip, finish, io_error, parse_args, Access, ToolError, ToolRegistry,
    ToolRequest, ToolStatus, MAX_LIST_ENTRIES, MAX_OUTPUT_CHARS, MAX_READ_BYTES, MAX_WRITE_BYTES,
};
use crate::verify::{media_type_for, ContentDigest};
use crate::CancellationChild;

const SKIPPED_DIRECTORIES: [&str; 4] = ["target", ".git", "node_modules", ".svn"];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    #[serde(default)]
    path: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    path: String,
    content: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditArgs {
    path: String,
    old_text: String,
    new_text: String,
}

pub(crate) async fn list_files(
    registry: &ToolRegistry,
    request: &ToolRequest,
    cancel: &CancellationChild,
    started: Instant,
) -> Result<super::ToolResult, ToolError> {
    check_cancelled(cancel)?;
    let args: ListArgs = parse_args(request)?;
    let requested = args.path.unwrap_or_else(|| ".".to_owned());
    let directory = registry.resolve(&requested, Access::List)?;

    let mut entries = Vec::new();
    let mut truncated = false;
    walk(
        registry.workspace(),
        &directory,
        0,
        &mut entries,
        &mut truncated,
    );
    let mut output = if entries.is_empty() {
        "(empty)".to_owned()
    } else {
        entries.join("\n")
    };
    if truncated {
        output.push_str(&format!("\n[truncated at {MAX_LIST_ENTRIES} entries]"));
    }
    Ok(finish(
        request,
        started,
        ToolStatus::Succeeded,
        None,
        clip(output, MAX_OUTPUT_CHARS),
        Vec::new(),
    ))
}

pub(crate) async fn read_file(
    registry: &ToolRegistry,
    request: &ToolRequest,
    cancel: &CancellationChild,
    started: Instant,
) -> Result<super::ToolResult, ToolError> {
    check_cancelled(cancel)?;
    let args: ReadArgs = parse_args(request)?;
    let path = registry.resolve(&args.path, Access::Read)?;
    let metadata = std_fs::metadata(&path).map_err(|error| io_error(&args.path, error))?;
    if metadata.len() > MAX_READ_BYTES {
        return Err(ToolError::TooLarge {
            path: args.path,
            limit: MAX_READ_BYTES,
        });
    }
    let bytes = std_fs::read(&path).map_err(|error| io_error(&args.path, error))?;
    let digest = ContentDigest::of(&bytes, media_type_for(&path));
    let text = String::from_utf8(bytes).map_err(|_| ToolError::Io {
        path: args.path,
        message: "file is not valid UTF-8; only text files can be read this way".to_owned(),
    })?;
    Ok(finish(
        request,
        started,
        ToolStatus::Succeeded,
        None,
        clip(text, MAX_OUTPUT_CHARS),
        vec![digest],
    ))
}

pub(crate) async fn write_file(
    registry: &ToolRegistry,
    request: &ToolRequest,
    cancel: &CancellationChild,
    started: Instant,
) -> Result<super::ToolResult, ToolError> {
    check_cancelled(cancel)?;
    let args: WriteArgs = parse_args(request)?;
    if args.content.len() > MAX_WRITE_BYTES {
        return Err(ToolError::TooLarge {
            path: args.path,
            limit: MAX_WRITE_BYTES as u64,
        });
    }
    let path = registry.resolve(&args.path, Access::Write)?;
    check_cancelled(cancel)?;
    if let Some(parent) = path.parent() {
        std_fs::create_dir_all(parent).map_err(|error| io_error(&args.path, error))?;
    }
    let bytes = args.content.into_bytes();
    atomic_write(&path, &bytes).map_err(|error| io_error(&args.path, error))?;
    let digest = ContentDigest::of(&bytes, media_type_for(&path));
    let output = format!("wrote {} bytes to {}", digest.size, args.path);
    Ok(finish(
        request,
        started,
        ToolStatus::Succeeded,
        None,
        output,
        vec![digest],
    ))
}

pub(crate) async fn edit_file(
    registry: &ToolRegistry,
    request: &ToolRequest,
    cancel: &CancellationChild,
    started: Instant,
) -> Result<super::ToolResult, ToolError> {
    check_cancelled(cancel)?;
    let args: EditArgs = parse_args(request)?;
    if args.old_text.is_empty() {
        return Err(ToolError::InvalidArguments {
            tool: request.tool,
            message: "old_text must not be empty".to_owned(),
        });
    }
    if args.new_text.len() > MAX_WRITE_BYTES {
        return Err(ToolError::TooLarge {
            path: args.path,
            limit: MAX_WRITE_BYTES as u64,
        });
    }
    let path = registry.resolve(&args.path, Access::Write)?;
    let metadata = std_fs::metadata(&path).map_err(|error| io_error(&args.path, error))?;
    if metadata.len() > MAX_READ_BYTES {
        return Err(ToolError::TooLarge {
            path: args.path,
            limit: MAX_READ_BYTES,
        });
    }
    let text = std_fs::read_to_string(&path).map_err(|error| io_error(&args.path, error))?;
    let count = text.matches(&args.old_text).count();
    if count != 1 {
        return Err(ToolError::AmbiguousEdit {
            path: args.path,
            count,
        });
    }
    let updated = text.replacen(&args.old_text, &args.new_text, 1);
    if updated.len() > MAX_WRITE_BYTES {
        return Err(ToolError::TooLarge {
            path: args.path,
            limit: MAX_WRITE_BYTES as u64,
        });
    }
    check_cancelled(cancel)?;
    atomic_write(&path, updated.as_bytes()).map_err(|error| io_error(&args.path, error))?;
    let digest = ContentDigest::of(updated.as_bytes(), media_type_for(&path));
    let output = format!("replaced 1 occurrence in {}", args.path);
    Ok(finish(
        request,
        started,
        ToolStatus::Succeeded,
        None,
        output,
        vec![digest],
    ))
}

fn walk(
    root: &Path,
    directory: &Path,
    depth: usize,
    entries: &mut Vec<String>,
    truncated: &mut bool,
) {
    if depth > 4 || *truncated {
        return;
    }
    let Ok(read) = std_fs::read_dir(directory) else {
        return;
    };
    let mut items: Vec<_> = read.flatten().collect();
    items.sort_by_key(std::fs::DirEntry::file_name);
    for entry in items {
        if entries.len() >= MAX_LIST_ENTRIES {
            *truncated = true;
            return;
        }
        let path = entry.path();
        let Ok(metadata) = std_fs::symlink_metadata(&path) else {
            continue;
        };
        let relative = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        if metadata.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if SKIPPED_DIRECTORIES.iter().any(|skip| *skip == name) {
                continue;
            }
            entries.push(format!("{relative}/"));
            walk(root, &path, depth + 1, entries, truncated);
        } else {
            entries.push(relative);
        }
    }
}

/// Writes through a unique sibling temporary file, then renames it into place.
/// A failed write never leaves a partial target or a stray temporary.
fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no parent")
    })?;
    let temporary: PathBuf = parent.join(format!(".lfm3-{}.tmp", uuid::Uuid::new_v4().simple()));
    let result = (|| {
        let mut file = std_fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std_fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std_fs::remove_file(&temporary);
    }
    result
}
