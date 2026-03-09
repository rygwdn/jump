//! SQLite-backed per-project cache for shortpath `PathType` results.
//!
//! ## Design
//!
//! Rather than caching the full shortpath keyed by the exact current working
//! directory, this module caches the **`PathType`** (the expensive-to-compute
//! part) keyed by the **git working directory** (`git_root`).
//!
//! The `segments` component of a `ShortPath` (the relative path inside the
//! repo) is trivially derived from `cwd.strip_prefix(git_root)` — a pure
//! string operation with no filesystem I/O.
//!
//! ### Cache lookup
//!
//! To avoid even the `find_git_root` stat walk on a hit, lookup uses a SQL
//! prefix match against all stored `git_root` values:
//!
//! ```sql
//! WHERE cwd = git_root
//!    OR substr(cwd, 1, length(git_root) + 1) = git_root || '/'
//! ORDER BY length(git_root) DESC   -- longest match first (handles submodules)
//! LIMIT 1
//! ```
//!
//! On a warm cache hit this is **zero filesystem I/O** — just a SQLite read.
//! On a miss the full `find_git_root` + `read_origin_url` path runs and the
//! result is stored.
//!
//! ### Validation
//!
//! `PathType` is stable for the lifetime of a checkout — it only changes if
//! the remote URL is modified (`git remote set-url`) which is extremely rare.
//! A 24-hour TTL is therefore sufficient; no HEAD mtime check is needed.
//!
//! ### HEAD file path (branch detection)
//!
//! The `head_file_path` column stores the path to the HEAD file for the repo.
//! This is used by `get_repository_branch_cached` to skip the directory-tree
//! walk when branch detection is needed (e.g. during `nav` candidate building).

use crate::path_shortener::PathType;
use rusqlite::{params, Connection, OpenFlags, Result as SqlResult};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// TTL for cache entries.  PathType almost never changes so 24 h is conservative.
const CACHE_TTL_SECS: i64 = 86_400;

/// Cache for `PathType` values keyed by git working directory.
pub struct ShortpathCache {
    db_path: PathBuf,
}

impl ShortpathCache {
    /// Create a cache that shares the given database file (the frecency DB).
    pub fn new(db_path: PathBuf) -> Self {
        Self { db_path }
    }

    fn open_readonly(&self) -> SqlResult<Connection> {
        Connection::open_with_flags(
            &self.db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_URI
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
    }

    fn open_readwrite(&self) -> SqlResult<Connection> {
        let conn = Connection::open(&self.db_path)?;
        // Ensure the table exists — handles the case where the cache write
        // happens before any frecency operation has initialised the full schema.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS project_path_type_cache (
                git_root        TEXT PRIMARY KEY,
                path_type_json  TEXT NOT NULL,
                head_file_path  TEXT,
                cached_at       INTEGER NOT NULL
            );",
        )?;
        Ok(conn)
    }

    /// Look up the cached `PathType` for `cwd` using a SQL prefix match against
    /// stored `git_root` values.
    ///
    /// Returns `(path_type, git_root)` on a valid hit, `None` on a miss.
    /// On a hit there is **zero filesystem I/O**.
    pub fn get_path_type(&self, cwd: &Path) -> Option<(PathType, PathBuf)> {
        if !self.db_path.exists() {
            return None;
        }
        let conn = self.open_readonly().ok()?;
        let cwd_str = cwd.to_string_lossy();
        let now = now_secs();

        let (git_root_str, path_type_json, cached_at): (String, String, i64) = conn
            .query_row(
                "SELECT git_root, path_type_json, cached_at
                 FROM project_path_type_cache
                 WHERE ?1 = git_root
                    OR substr(?1, 1, length(git_root) + 1) = git_root || '/'
                 ORDER BY length(git_root) DESC
                 LIMIT 1",
                params![cwd_str.as_ref()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .ok()?;

        if now - cached_at > CACHE_TTL_SECS {
            return None;
        }

        let path_type: PathType = serde_json::from_str(&path_type_json).ok()?;
        Some((path_type, PathBuf::from(git_root_str)))
    }

    /// Store the `PathType` for a git working directory.
    ///
    /// `head_file_path` is optional; when provided it is stored for use by
    /// branch-detection code via [`Self::get_head_file`].
    pub fn set_path_type(
        &self,
        git_root: &Path,
        path_type: &PathType,
        head_file_path: Option<&Path>,
    ) {
        let Ok(json) = serde_json::to_string(path_type) else {
            return;
        };
        let now = now_secs();
        let git_root_str = git_root.to_string_lossy();
        let head_path_str = head_file_path.map(|p| p.to_string_lossy().into_owned());

        let Ok(conn) = self.open_readwrite() else {
            return;
        };
        conn.execute(
            "INSERT OR REPLACE INTO project_path_type_cache
             (git_root, path_type_json, head_file_path, cached_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![git_root_str.as_ref(), json, head_path_str, now],
        )
        .ok();
    }

    /// Return the cached HEAD file path for `git_root` (exact match).
    ///
    /// Used by branch-detection code to skip the directory-tree walk.
    /// Only returns a path if the file still exists on disk.
    pub fn get_head_file(&self, git_root: &Path) -> Option<PathBuf> {
        if !self.db_path.exists() {
            return None;
        }
        let conn = self.open_readonly().ok()?;
        let git_root_str = git_root.to_string_lossy();
        let now = now_secs();

        let (head_file_str, cached_at): (Option<String>, i64) = conn
            .query_row(
                "SELECT head_file_path, cached_at
                 FROM project_path_type_cache
                 WHERE git_root = ?1",
                params![git_root_str.as_ref()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .ok()?;

        if now - cached_at > CACHE_TTL_SECS {
            return None;
        }

        let head_path = PathBuf::from(head_file_str?);
        if head_path.exists() {
            Some(head_path)
        } else {
            None
        }
    }

    /// Store a HEAD file path for `git_root` without overwriting an existing
    /// `path_type_json` entry.
    pub fn set_head_file(&self, git_root: &Path, head_file_path: &Path) {
        let now = now_secs();
        let git_root_str = git_root.to_string_lossy();
        let head_path_str = head_file_path.to_string_lossy();

        let Ok(conn) = self.open_readwrite() else {
            return;
        };
        // Insert head-file-only row; don't overwrite a richer row that already
        // has path_type_json.
        conn.execute(
            "INSERT INTO project_path_type_cache
             (git_root, path_type_json, head_file_path, cached_at)
             VALUES (?1, '', ?2, ?3)
             ON CONFLICT(git_root) DO UPDATE SET
                 head_file_path = excluded.head_file_path,
                 cached_at      = excluded.cached_at
             WHERE path_type_json = ''",
            params![git_root_str.as_ref(), head_path_str.as_ref(), now],
        )
        .ok();
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::path_shortener::PathType;
    use tempfile::TempDir;

    fn github_path_type() -> PathType {
        PathType::GitHub {
            owner: "acme".into(),
            repo: "widget".into(),
        }
    }

    fn make_cache(temp: &TempDir) -> ShortpathCache {
        ShortpathCache::new(temp.path().join("test.db"))
    }

    #[test]
    fn test_miss_on_empty_db() {
        let temp = TempDir::new().unwrap();
        let cache = make_cache(&temp);
        assert!(cache.get_path_type(Path::new("/some/project")).is_none());
    }

    #[test]
    fn test_exact_root_hit() {
        let temp = TempDir::new().unwrap();
        let cache = make_cache(&temp);
        let root = Path::new("/home/user/src/widget");
        let pt = github_path_type();

        cache.set_path_type(root, &pt, None);

        let result = cache.get_path_type(root);
        assert!(result.is_some());
        let (got_pt, got_root) = result.unwrap();
        assert_eq!(got_pt, pt);
        assert_eq!(got_root, root);
    }

    #[test]
    fn test_prefix_match_subdirectory() {
        let temp = TempDir::new().unwrap();
        let cache = make_cache(&temp);
        let root = Path::new("/home/user/src/widget");
        let pt = github_path_type();

        cache.set_path_type(root, &pt, None);

        // Look up from a subdirectory — should hit via prefix match
        let subdir = Path::new("/home/user/src/widget/src/components/button");
        let result = cache.get_path_type(subdir);
        assert!(result.is_some());
        let (got_pt, got_root) = result.unwrap();
        assert_eq!(got_pt, pt);
        assert_eq!(got_root, root);
    }

    #[test]
    fn test_prefix_match_picks_longest_root() {
        let temp = TempDir::new().unwrap();
        let cache = make_cache(&temp);

        let outer_root = Path::new("/home/user/src");
        let inner_root = Path::new("/home/user/src/widget");
        let outer_pt = PathType::Git {
            repo_name: "src".into(),
        };
        let inner_pt = github_path_type();

        cache.set_path_type(outer_root, &outer_pt, None);
        cache.set_path_type(inner_root, &inner_pt, None);

        // Should return the longer (inner) match, not the outer one
        let subdir = Path::new("/home/user/src/widget/lib");
        let result = cache.get_path_type(subdir).unwrap();
        assert_eq!(result.0, inner_pt);
        assert_eq!(result.1, inner_root);
    }

    #[test]
    fn test_no_false_prefix_match() {
        let temp = TempDir::new().unwrap();
        let cache = make_cache(&temp);

        // Cache /home/user/src/widget — must NOT match /home/user/src/widget-extra
        let root = Path::new("/home/user/src/widget");
        cache.set_path_type(root, &github_path_type(), None);

        let other = Path::new("/home/user/src/widget-extra/lib");
        assert!(cache.get_path_type(other).is_none());
    }

    #[test]
    fn test_ttl_expiry() {
        let temp = TempDir::new().unwrap();
        let cache = make_cache(&temp);
        let root = Path::new("/home/user/src/widget");
        cache.set_path_type(root, &github_path_type(), None);

        // Manually backdate the cached_at to simulate expiry
        {
            let conn = Connection::open(cache.db_path.clone()).unwrap();
            let expired = now_secs() - CACHE_TTL_SECS - 1;
            conn.execute(
                "UPDATE project_path_type_cache SET cached_at = ?1 WHERE git_root = ?2",
                params![expired, root.to_string_lossy().as_ref()],
            )
            .unwrap();
        }

        assert!(cache.get_path_type(root).is_none());
    }

    #[test]
    fn test_head_file_roundtrip() {
        let temp = TempDir::new().unwrap();
        let cache = make_cache(&temp);

        let head_file = temp.path().join("HEAD");
        std::fs::write(&head_file, "ref: refs/heads/main").unwrap();
        let root = Path::new("/home/user/src/widget");

        cache.set_path_type(root, &github_path_type(), Some(&head_file));

        let result = cache.get_head_file(root);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), head_file);
    }

    #[test]
    fn test_set_head_file_only() {
        let temp = TempDir::new().unwrap();
        let cache = make_cache(&temp);

        let head_file = temp.path().join("HEAD");
        std::fs::write(&head_file, "ref: refs/heads/main").unwrap();
        let root = temp.path().join("project");
        std::fs::create_dir_all(&root).unwrap();

        cache.set_head_file(&root, &head_file);

        let result = cache.get_head_file(&root);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), head_file);
    }

    #[test]
    fn test_set_head_file_does_not_overwrite_path_type() {
        let temp = TempDir::new().unwrap();
        let cache = make_cache(&temp);

        let head_file = temp.path().join("HEAD");
        std::fs::write(&head_file, "ref: refs/heads/main").unwrap();
        let root = Path::new("/home/user/src/widget");
        let pt = github_path_type();

        // First set the full entry (with path_type)
        cache.set_path_type(root, &pt, Some(&head_file));

        // Now try to overwrite with a head-file-only set
        let other_head = temp.path().join("OTHER_HEAD");
        std::fs::write(&other_head, "ref: refs/heads/main").unwrap();
        cache.set_head_file(root, &other_head);

        // path_type should be preserved and head_file_path should NOT be overwritten
        // (because path_type_json != '' so the condition in set_head_file is false)
        let got = cache.get_path_type(root).unwrap();
        assert_eq!(got.0, pt);
        // head_file is still the original one
        let got_head = cache.get_head_file(root).unwrap();
        assert_eq!(got_head, head_file);
    }
}
