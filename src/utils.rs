use std::fs;
use std::path::{Path, PathBuf};

/// Utility function to expand paths with ~ notation
pub fn expand_path(path: &str) -> PathBuf {
    if path.starts_with('~') {
        if let Some(home) = dirs::home_dir() {
            home.join(&path[2..])
        } else {
            PathBuf::from(path)
        }
    } else {
        PathBuf::from(path)
    }
}

/// Walk up the directory tree, returning both the git working directory (root) and
/// the path to the HEAD file for the nearest git repository.
///
/// For regular repos: `(root, root/.git/HEAD)`.
/// For worktrees: `(worktree_root, <resolved-gitdir>/HEAD)`.
pub fn find_git_root_and_head(path: &Path) -> Option<(PathBuf, PathBuf)> {
    let mut current = path;
    loop {
        let git_entry = current.join(".git");
        if git_entry.is_dir() {
            return Some((current.to_path_buf(), git_entry.join("HEAD")));
        } else if git_entry.is_file() {
            // Worktree: .git is a file containing "gitdir: <path>"
            let git_root = current.to_path_buf();
            let contents = fs::read_to_string(&git_entry).ok()?;
            let gitdir = contents
                .lines()
                .find_map(|l| l.trim().strip_prefix("gitdir:"))?
                .trim()
                .to_string();
            let gitdir_path = PathBuf::from(&gitdir);
            let gitdir_abs = if gitdir_path.is_absolute() {
                gitdir_path
            } else {
                current.join(&gitdir_path)
            };
            return Some((git_root, gitdir_abs.join("HEAD")));
        }
        current = current.parent()?;
    }
}

/// Walk up the directory tree to find the HEAD file for the nearest git repository.
/// For regular repos returns `<root>/.git/HEAD`.
/// For worktrees resolves the `gitdir:` pointer and returns that worktree's HEAD.
pub fn find_git_head(path: &Path) -> Option<PathBuf> {
    find_git_root_and_head(path).map(|(_, head)| head)
}

/// Utility function to get the current branch of a git repository
pub fn get_repository_branch(repo_path: &str) -> Option<String> {
    let git_head = find_git_head(Path::new(repo_path))?;
    read_branch_from_head(&git_head)
}

/// Like [`get_repository_branch`] but uses `cache` to skip the directory-tree
/// walk that locates the HEAD file.  On a cache miss the HEAD path is stored so
/// subsequent calls for the same directory are fast.
///
/// `cache` is a `ShortpathCache` — passed in so callers can share a single
/// instance across many repos without re-opening the database each time.
pub fn get_repository_branch_cached(
    repo_path: &str,
    cache: &crate::shortpath_cache::ShortpathCache,
) -> Option<String> {
    let repo = Path::new(repo_path);
    let git_head = match cache.get_head_file(repo) {
        Some(p) => p,
        None => {
            let found = find_git_head(repo)?;
            cache.set_head_file(repo, &found);
            found
        }
    };
    read_branch_from_head(&git_head)
}

fn read_branch_from_head(git_head: &Path) -> Option<String> {
    match fs::read_to_string(git_head) {
        Ok(contents) => {
            let trimmed = contents.trim();
            if let Some(branch) = trimmed.strip_prefix("ref: refs/heads/") {
                let branch = branch.trim();
                // `.invalid` is the sentinel git writes into the text HEAD file
                // for reftable repos so that naïve text readers get an obviously
                // wrong value.  The real branch lives in the reftable.
                if branch == ".invalid" {
                    let gitdir = git_head.parent()?;
                    return crate::git_reftable::read_head_from_reftable(gitdir);
                }
                if branch.is_empty() || branch == "master" || branch == "main" {
                    return None;
                }
                return Some(branch.to_string());
            }
            // File exists but is not a symbolic ref (detached HEAD, raw OID, or
            // other unrecognised content).  Don't fall through to reftable.
            if !trimmed.is_empty() {
                return None;
            }
            // File is empty — fall through to the reftable reader.
            let gitdir = git_head.parent()?;
            crate::git_reftable::read_head_from_reftable(gitdir)
        }
        Err(_) => {
            // HEAD text file does not exist — try reftable format.
            let gitdir = git_head.parent()?;
            crate::git_reftable::read_head_from_reftable(gitdir)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn test_expand_path_with_home() {
        let Some(home_dir) = dirs::home_dir() else {
            eprintln!("WARNING: Home directory not found, skipping test");
            return;
        };

        // Test expanding ~
        let expanded = expand_path("~/test");
        assert_eq!(expanded, home_dir.join("test"));

        // Test expanding ~/sub/path
        let expanded = expand_path("~/sub/path");
        assert_eq!(expanded, home_dir.join("sub/path"));
    }

    #[test]
    fn test_expand_path_without_home() {
        // Test regular path
        let expanded = expand_path("/absolute/path");
        assert_eq!(expanded, PathBuf::from("/absolute/path"));

        // Test relative path
        let expanded = expand_path("relative/path");
        assert_eq!(expanded, PathBuf::from("relative/path"));
    }

    #[test]
    fn test_reftable_sentinel_falls_back_to_reftable() {
        use crate::git_reftable::tests::make_reftable_gitdir;
        use std::fs;

        // Create a gitdir with a `.invalid` sentinel HEAD file and a reftable
        // that contains the real branch.
        let tmp = tempfile::TempDir::new().unwrap();
        let gitdir = tmp.path();

        // Write the sentinel text HEAD
        fs::write(gitdir.join("HEAD"), "ref: refs/heads/.invalid\n").unwrap();

        // Create a real reftable pointing at feature-xyz
        make_reftable_gitdir(gitdir, "refs/heads/feature-xyz");

        let head_path = gitdir.join("HEAD");
        let branch = read_branch_from_head(&head_path);
        assert_eq!(branch, Some("feature-xyz".to_string()));
    }

    #[test]
    fn test_repository_branch_detection() {
        // This test would require creating actual git repos, so we'll test the None cases

        // Test with non-existent path
        let result = get_repository_branch("/non/existent/path");
        assert_eq!(result, None);

        // Test with current directory (may or may not be a git repo)
        if let Ok(current_dir) = env::current_dir() {
            let result = get_repository_branch(&current_dir.to_string_lossy());
            // If it returns Some, it should not be master or main
            if let Some(branch) = result {
                assert_ne!(branch, "master");
                assert_ne!(branch, "main");
            }
        }
    }
}
