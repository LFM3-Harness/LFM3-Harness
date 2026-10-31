//! Bounded workspace search for file discovery and related-file reports.
//!
//! Both tools walk only the resolved workspace directory, skip links and
//! dependency folders, and read at most a small text window per file. They are
//! deterministic: the same workspace produces the same ordered result.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Instant,
};

use serde::Deserialize;

use super::{
    check_cancelled, clip, finish, parse_args, Access, ToolError, ToolRegistry, ToolRequest,
    ToolStatus, MAX_OUTPUT_CHARS,
};
use crate::CancellationChild;

const SKIPPED_DIRECTORIES: [&str; 6] = ["target", ".git", "node_modules", ".svn", "dist", ".lfm3"];
const MAX_WALK_ENTRIES: usize = 4_000;
const MAX_DEPTH: usize = 10;
const MAX_SCAN_BYTES: u64 = 512 * 1024;
const SNIPPET_CHARS: usize = 160;

const STOP_WORDS: [&str; 24] = [
    "about", "after", "again", "also", "because", "been", "before", "being", "between", "both",
    "could", "does", "each", "from", "have", "into", "more", "most", "other", "should", "some",
    "such", "than", "that",
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    query: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    max_results: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RelatedArgs {
    path: String,
    #[serde(default)]
    max_results: Option<u32>,
}

struct Hit {
    path: String,
    score: usize,
    name_match: bool,
    snippets: Vec<String>,
    terms: usize,
}

pub(crate) async fn search_files(
    registry: &ToolRegistry,
    request: &ToolRequest,
    cancel: &CancellationChild,
    started: Instant,
) -> Result<super::ToolResult, ToolError> {
    check_cancelled(cancel)?;
    let args: SearchArgs = parse_args(request)?;
    let mode = args
        .mode
        .as_deref()
        .unwrap_or("both")
        .trim()
        .to_ascii_lowercase();
    if !matches!(mode.as_str(), "name" | "content" | "both") {
        return Err(ToolError::InvalidArguments {
            tool: request.tool,
            message: "mode must be name, content, or both".to_owned(),
        });
    }
    let terms = query_terms(&args.query);
    if terms.is_empty() {
        return Err(ToolError::InvalidArguments {
            tool: request.tool,
            message: "query must contain at least one word of two or more characters".to_owned(),
        });
    }
    let limit = args.max_results.unwrap_or(20).clamp(1, 40) as usize;
    let requested = args.path.unwrap_or_else(|| ".".to_owned());
    let directory = registry.resolve(&requested, Access::List)?;

    let mut hits = Vec::new();
    let mut truncated = false;
    let root = registry.workspace().to_path_buf();
    let mut visited = 0_usize;
    walk(&directory, 0, &mut visited, &mut truncated, &mut |path| {
        let relative = relative_path(&root, path);
        let name_lower = relative.to_ascii_lowercase();
        let full_query = args.query.trim().to_ascii_lowercase();
        let name_hit = mode != "content" && name_lower.contains(&full_query);
        let name_terms = terms
            .iter()
            .filter(|term| name_lower.contains(*term))
            .count();
        let name_score = if name_lower
            .rsplit('/')
            .next()
            .is_some_and(|file| file == full_query)
        {
            20
        } else if name_hit {
            10 + name_terms
        } else {
            name_terms * 2
        };

        let mut snippets = Vec::new();
        let mut content_terms = 0_usize;
        if mode != "name" {
            if let Some(text) = read_text(path) {
                for term in &terms {
                    let mut found = false;
                    for (index, line) in text.lines().enumerate() {
                        if line.to_ascii_lowercase().contains(term) {
                            found = true;
                            if snippets.len() < 3 {
                                snippets.push(format!(
                                    "L{}: {}",
                                    index + 1,
                                    clip_line(line.trim(), SNIPPET_CHARS)
                                ));
                            }
                        }
                    }
                    if found {
                        content_terms += 1;
                    }
                }
            }
        }
        if name_score == 0 && content_terms == 0 {
            return;
        }
        hits.push(Hit {
            path: relative,
            score: name_score + content_terms * 4,
            name_match: name_hit,
            snippets,
            terms: content_terms,
        });
    });

    if hits.is_empty() {
        return Ok(finish(
            request,
            started,
            ToolStatus::Succeeded,
            None,
            format!(
                "No file in {} matched {:?} by {}.",
                requested,
                args.query.trim(),
                match mode.as_str() {
                    "name" => "name",
                    "content" => "content",
                    _ => "name or content",
                }
            ),
            Vec::new(),
        ));
    }

    hits.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left.path.cmp(&right.path))
    });
    let total = hits.len();
    let mut output = format!(
        "Found {total} file(s) for {:?} in {}.\n",
        args.query.trim(),
        requested
    );
    for hit in hits.iter().take(limit) {
        let kind = match (hit.name_match, hit.terms) {
            (true, 0) => "name".to_owned(),
            (false, terms) => format!("content, {terms} term(s)"),
            (true, terms) => format!("name and content, {terms} term(s)"),
        };
        output.push_str(&format!("\n{}  [{kind}]\n", hit.path));
        for snippet in &hit.snippets {
            output.push_str(&format!("  {snippet}\n"));
        }
    }
    if total > limit {
        output.push_str(&format!("[showing {limit} of {total} matches]\n"));
    }
    if truncated {
        output.push_str(&format!(
            "[scan stopped after {MAX_WALK_ENTRIES} entries; narrow the path or query]\n"
        ));
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

pub(crate) async fn related_files(
    registry: &ToolRegistry,
    request: &ToolRequest,
    cancel: &CancellationChild,
    started: Instant,
) -> Result<super::ToolResult, ToolError> {
    check_cancelled(cancel)?;
    let args: RelatedArgs = parse_args(request)?;
    let limit = args.max_results.unwrap_or(10).clamp(1, 30) as usize;
    let seed = registry.resolve(&args.path, Access::Read)?;
    let seed_text = read_text(&seed).ok_or_else(|| ToolError::InvalidArguments {
        tool: request.tool,
        message: format!(
            "{} is not a readable UTF-8 text file; related_files needs a text source",
            args.path
        ),
    })?;
    let terms = salient_terms(&seed_text);
    if terms.is_empty() {
        return Err(ToolError::InvalidArguments {
            tool: request.tool,
            message: "the source file has no distinctive words to relate".to_owned(),
        });
    }
    let term_set: Vec<&str> = terms.iter().map(String::as_str).collect();

    let root = registry.workspace().to_path_buf();
    let mut hits = Vec::new();
    let mut truncated = false;
    let mut visited = 0_usize;
    walk(&root, 0, &mut visited, &mut truncated, &mut |path| {
        if path == seed.as_path() {
            return;
        }
        let Some(text) = read_text(path) else {
            return;
        };
        let lower = text.to_ascii_lowercase();
        let matched: Vec<&str> = term_set
            .iter()
            .copied()
            .filter(|term| lower.contains(term))
            .collect();
        if matched.len() < 2 {
            return;
        }
        let relative = relative_path(&root, path);
        let name_lower = relative.to_ascii_lowercase();
        let filename_bonus = matched
            .iter()
            .filter(|term| name_lower.contains(**term))
            .count()
            * 2;
        let snippet = matched.iter().find_map(|term| {
            text.lines()
                .find(|line| line.to_ascii_lowercase().contains(term))
                .map(|line| clip_line(line.trim(), SNIPPET_CHARS))
        });
        hits.push(Hit {
            path: relative,
            score: matched.len() * 4 + filename_bonus,
            name_match: filename_bonus > 0,
            snippets: snippet.into_iter().collect(),
            terms: matched.len(),
        });
    });

    if hits.is_empty() {
        return Ok(finish(
            request,
            started,
            ToolStatus::Succeeded,
            None,
            format!(
                "No other file shared two or more distinctive terms with {}.",
                args.path
            ),
            Vec::new(),
        ));
    }
    hits.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left.path.cmp(&right.path))
    });
    let total = hits.len();
    let mut output = format!(
        "Files related to {} (seed terms: {}):\n",
        args.path,
        term_set.join(", ")
    );
    for hit in hits.iter().take(limit) {
        output.push_str(&format!(
            "\n{}  [{} shared term(s){}]\n",
            hit.path,
            hit.terms,
            if hit.name_match { ", name" } else { "" }
        ));
        for snippet in &hit.snippets {
            output.push_str(&format!("  {snippet}\n"));
        }
    }
    if total > limit {
        output.push_str(&format!("[showing {limit} of {total} related files]\n"));
    }
    if truncated {
        output.push_str("[scan stopped at the entry cap]\n");
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

fn query_terms(query: &str) -> Vec<String> {
    let mut terms: Vec<String> = query
        .split(|character: char| {
            !character.is_alphanumeric() && character != '_' && character != '-'
        })
        .map(|word| word.trim().to_ascii_lowercase())
        .filter(|word| word.chars().count() >= 2)
        .collect();
    terms.sort();
    terms.dedup();
    terms.truncate(12);
    terms
}

fn salient_terms(text: &str) -> Vec<String> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for word in text.split(|character: char| !character.is_alphanumeric() && character != '_') {
        let word = word.trim().to_ascii_lowercase();
        let len = word.chars().count();
        if !(4..=32).contains(&len) {
            continue;
        }
        if STOP_WORDS.contains(&word.as_str()) {
            continue;
        }
        *counts.entry(word).or_default() += 1;
    }
    let mut ranked: Vec<(String, usize)> = counts.into_iter().collect();
    ranked.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    ranked.into_iter().take(12).map(|(word, _)| word).collect()
}

/// Walks regular files only: links and dependency folders are skipped so a
/// search cannot drift outside the workspace or stall on generated trees.
fn walk(
    directory: &Path,
    depth: usize,
    visited: &mut usize,
    truncated: &mut bool,
    visit: &mut dyn FnMut(&Path),
) {
    if depth > MAX_DEPTH || *truncated {
        return;
    }
    let Ok(read) = std::fs::read_dir(directory) else {
        return;
    };
    let mut items: Vec<_> = read.flatten().collect();
    items.sort_by_key(std::fs::DirEntry::file_name);
    for entry in items {
        if *visited >= MAX_WALK_ENTRIES {
            *truncated = true;
            return;
        }
        *visited += 1;
        let path: PathBuf = entry.path();
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if name.starts_with('.') || SKIPPED_DIRECTORIES.contains(&name.as_str()) {
                continue;
            }
            walk(&path, depth + 1, visited, truncated, visit);
        } else if metadata.is_file() {
            visit(&path);
        }
    }
}

fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn read_text(path: &Path) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_SCAN_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if bytes.iter().take(4_096).any(|byte| *byte == 0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

fn clip_line(line: &str, limit: usize) -> String {
    if line.chars().count() <= limit {
        return line.to_owned();
    }
    let mut clipped: String = line.chars().take(limit).collect();
    clipped.push('…');
    clipped
}
