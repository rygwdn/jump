#![allow(clippy::expect_used)]

use clap::{Arg, ArgAction, ArgGroup, Command};
use jumpr::path_shortener::{PathType, ShortPath};
use jumpr::shortpath_cache::ShortpathCache;
use jumpr::utils::find_git_root_and_head;
use jumpr::FrecencyDb;
use jumpr::ShortPathPart::*;
use jumpr::{shorten_path, ShortPathPart};
use std::env;
use std::path::{Path, PathBuf};

pub fn command() -> Command {
    Command::new("shortpath")
        .about("Shortens paths for shell prompts")
        .arg(
            Arg::new("path")
                .value_name("PATH")
                .help("Path to shorten (default: current directory)")
                .default_value(".")
                .action(ArgAction::Set)
                .required(false),
        )
        .arg(
            Arg::new("project")
                .long("project")
                .help("Output only the project/repo prefix segment (symbol + name)")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("dir")
                .long("path")
                .help("Output only the directory path segment (infix + suffix, no prefix)")
                .action(ArgAction::SetTrue),
        )
        .group(
            ArgGroup::new("output_mode")
                .args(["project", "dir"])
                .required(false),
        )
}

pub fn handle(matches: &clap::ArgMatches) {
    let path = matches.get_one::<String>("path").expect("path is required");
    let cwd = expand_path(path);

    let db = FrecencyDb::new();
    let cache = ShortpathCache::new(db.db_path().to_path_buf());

    let short_path = resolve_shortpath(&cwd, &cache);

    let output = if matches.get_flag("project") {
        short_path.build(1, &[Prefix])
    } else if matches.get_flag("dir") {
        short_path.build(1, &[Infix, Suffix])
    } else {
        short_path.build(1, &[ShortPathPart::Prefix, Infix, Suffix])
    };

    println!("{output}");
}

/// Resolve the short path for `cwd`, using the per-project cache where possible.
///
/// Hot path (cache hit): SQL prefix match → zero filesystem I/O → compute
/// segments as `cwd.strip_prefix(git_root)` (pure string op).
///
/// Cold path (cache miss): full `shorten_path()` computation; if the result is
/// a git-type path the `PathType` and `git_root` are stored for future calls.
fn resolve_shortpath(cwd: &Path, cache: &ShortpathCache) -> ShortPath {
    // Fast path: prefix-match against cached git roots — zero FS I/O
    if let Some((path_type, git_root)) = cache.get_path_type(cwd) {
        return ShortPath {
            segments: path_segments(cwd, &git_root),
            path_type,
        };
    }

    // Slow path: full computation
    let short_path = shorten_path(cwd);

    // Cache git-type results keyed by git_root (world-tree / home / regular
    // paths are fast enough without caching)
    if is_git_path_type(&short_path.path_type) {
        if let Some((git_root, head_path)) = find_git_root_and_head(cwd) {
            cache.set_path_type(&git_root, &short_path.path_type, Some(&head_path));
        }
    }

    short_path
}

fn is_git_path_type(pt: &PathType) -> bool {
    matches!(
        pt,
        PathType::GitHub { .. } | PathType::GitHubRemote { .. } | PathType::Git { .. }
    )
}

/// Compute path segments relative to the git root (pure string op, no I/O).
fn path_segments(cwd: &Path, git_root: &Path) -> Vec<String> {
    let rel = cwd.strip_prefix(git_root).unwrap_or(cwd);
    let rel_str = rel.to_string_lossy();
    if rel_str.is_empty() || rel_str == "." {
        Vec::new()
    } else {
        rel_str
            .split('/')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
    }
}

fn expand_path(path: &str) -> PathBuf {
    let path_buf = if path == "." {
        env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    } else {
        PathBuf::from(path)
    };

    if let Ok(canonical) = path_buf.canonicalize() {
        canonical
    } else {
        path_buf
    }
}
