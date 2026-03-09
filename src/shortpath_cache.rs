//! SQLite-backed cache for shortpath results and git HEAD file paths.
//!
//! The cache lives in the same database as the frecency data.  Every entry is
//! keyed by the canonical directory path and validated against the mtime of the
//! `.git/HEAD` file so that a branch switch (which changes HEAD) automatically
//! invalidates the cached shortpath.  A 24-hour TTL provides a secondary
//! safety net for cases where the HEAD file path itself may have changed (e.g.
//! after re-cloning).

use rusqlite::{params, Connection, OpenFlags, Result as SqlResult};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::path_shortener::ShortPath;

/// How long a cache entry is considered valid even without a HEAD mtime match.
const CACHE_TTL_SECS: i64 = 86_400; // 24 hours

/// Cache for shortpath results and HEAD file paths, backed by the frecency SQLite DB.
pub struct ShortpathCache {
    db_path: PathBuf,
}

impl ShortpathCache {
    /// Create a cache that shares the given database file.
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
        // Ensure the table exists — handles the case where the cache is written
        // before any frecency operation has initialised the full schema.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS shortpath_cache (
                directory TEXT PRIMARY KEY,
                head_file_path TEXT,
                head_mtime INTEGER,
                shortpath_json TEXT,
                cached_at INTEGER NOT NULL
            );",
        )?;
        Ok(conn)
    }

    /// Look up a cached `ShortPath` for `directory`.
    ///
    /// Validation:
    /// - If a `head_file_path` is stored, its current mtime is compared to the
    ///   cached `head_mtime`.  Any difference (e.g. from a branch switch) causes
    ///   a cache miss.
    /// - If the entry is older than `CACHE_TTL_SECS` it is also treated as a miss.
    ///
    /// Returns `Some((shortpath, head_file_path))` on a valid hit.
    pub fn get(&self, directory: &Path) -> Option<(ShortPath, Option<PathBuf>)> {
        if !self.db_path.exists() {
            return None;
        }
        let conn = self.open_readonly().ok()?;
        let dir_str = directory.to_string_lossy();

        let (shortpath_json, head_file_str, cached_at, head_mtime): (
            Option<String>,
            Option<String>,
            i64,
            Option<i64>,
        ) = conn
            .query_row(
                "SELECT shortpath_json, head_file_path, cached_at, head_mtime
                 FROM shortpath_cache
                 WHERE directory = ?1",
                params![dir_str.as_ref()],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                    ))
                },
            )
            .ok()?;

        // Must have a shortpath to be a shortpath cache hit
        let shortpath_json = shortpath_json?;

        let now = now_secs();

        // TTL check
        if now - cached_at > CACHE_TTL_SECS {
            return None;
        }

        let head_file_path = head_file_str.map(PathBuf::from);

        // HEAD mtime validation: if HEAD changed (branch switch), invalidate
        if let (Some(ref head_path), Some(cached_head_mtime)) = (&head_file_path, head_mtime) {
            let actual_mtime = file_mtime_secs(head_path);
            if actual_mtime != Some(cached_head_mtime) {
                return None;
            }
        }

        let shortpath: ShortPath = serde_json::from_str(&shortpath_json).ok()?;
        Some((shortpath, head_file_path))
    }

    /// Store a `ShortPath` for `directory` in the cache.
    ///
    /// `head_file_path` should be the path to the `.git/HEAD` file (or worktree
    /// HEAD) for the repository containing `directory`.  Its current mtime is
    /// stored so that future lookups can detect branch switches.
    pub fn set(&self, directory: &Path, shortpath: &ShortPath, head_file_path: Option<&Path>) {
        let Ok(shortpath_json) = serde_json::to_string(shortpath) else {
            return;
        };
        let now = now_secs();
        let head_mtime = head_file_path.and_then(file_mtime_secs);
        let dir_str = directory.to_string_lossy();
        let head_path_str = head_file_path.map(|p| p.to_string_lossy().into_owned());

        let Ok(conn) = self.open_readwrite() else {
            return;
        };
        conn.execute(
            "INSERT OR REPLACE INTO shortpath_cache
             (directory, head_file_path, head_mtime, shortpath_json, cached_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                dir_str.as_ref(),
                head_path_str,
                head_mtime,
                shortpath_json,
                now
            ],
        )
        .ok();
    }

    /// Return the cached HEAD file path for `directory`, or `None` on miss.
    ///
    /// This is used by branch-detection code to skip the directory-tree walk
    /// that locates the `.git/HEAD` file.  The returned path is only returned
    /// if the file still exists on disk.
    pub fn get_head_file(&self, directory: &Path) -> Option<PathBuf> {
        if !self.db_path.exists() {
            return None;
        }
        let conn = self.open_readonly().ok()?;
        let dir_str = directory.to_string_lossy();
        let now = now_secs();

        let (head_file_str, cached_at): (Option<String>, i64) = conn
            .query_row(
                "SELECT head_file_path, cached_at FROM shortpath_cache WHERE directory = ?1",
                params![dir_str.as_ref()],
                |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, i64>(1)?)),
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

    /// Cache a HEAD file path without an associated shortpath.
    ///
    /// Uses `INSERT OR IGNORE` so it will not overwrite an existing entry that
    /// already has a `shortpath_json` stored.
    pub fn set_head_file(&self, directory: &Path, head_file_path: &Path) {
        let now = now_secs();
        let head_mtime = file_mtime_secs(head_file_path);
        let dir_str = directory.to_string_lossy();
        let head_path_str = head_file_path.to_string_lossy();

        let Ok(conn) = self.open_readwrite() else {
            return;
        };
        // Insert a head-file-only row; do not overwrite a richer row that has shortpath_json.
        conn.execute(
            "INSERT INTO shortpath_cache (directory, head_file_path, head_mtime, shortpath_json, cached_at)
             VALUES (?1, ?2, ?3, NULL, ?4)
             ON CONFLICT(directory) DO UPDATE SET
                 head_file_path = excluded.head_file_path,
                 head_mtime     = excluded.head_mtime,
                 cached_at      = excluded.cached_at
             WHERE shortpath_json IS NULL",
            params![dir_str.as_ref(), head_path_str.as_ref(), head_mtime, now],
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

fn file_mtime_secs(path: &Path) -> Option<i64> {
    std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::path_shortener::{PathType, ShortPath};
    use tempfile::TempDir;

    fn make_shortpath() -> ShortPath {
        ShortPath {
            path_type: PathType::GitHub {
                owner: "acme".to_string(),
                repo: "widget".to_string(),
            },
            segments: vec![],
        }
    }

    #[test]
    fn test_cache_miss_on_empty_db() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("test.db");
        let cache = ShortpathCache::new(db_path);
        assert!(cache.get(Path::new("/some/dir")).is_none());
    }

    #[test]
    fn test_set_and_get_roundtrip() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("test.db");
        // Need frecency schema first (or rely on cache's own CREATE TABLE IF NOT EXISTS)
        let cache = ShortpathCache::new(db_path);

        let dir = Path::new("/some/project");
        let sp = make_shortpath();
        cache.set(dir, &sp, None);

        let result = cache.get(dir);
        assert!(result.is_some());
        let (got_sp, got_head) = result.unwrap();
        assert_eq!(got_sp, sp);
        assert!(got_head.is_none());
    }

    #[test]
    fn test_head_mtime_invalidates_cache() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("test.db");
        let cache = ShortpathCache::new(db_path);

        // Create a real file to use as fake HEAD
        let head_file = temp.path().join("HEAD");
        std::fs::write(&head_file, "ref: refs/heads/main").unwrap();

        let dir = Path::new("/some/project");
        let sp = make_shortpath();
        cache.set(dir, &sp, Some(&head_file));

        // Should hit
        assert!(cache.get(dir).is_some());

        // Modify the HEAD file (simulates branch switch changing its mtime)
        // We need to actually change the mtime — write different content
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(&head_file, "ref: refs/heads/feature").unwrap();

        // Should now miss due to mtime mismatch
        assert!(cache.get(dir).is_none());
    }

    #[test]
    fn test_head_file_cache() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("test.db");
        let cache = ShortpathCache::new(db_path);

        let head_file = temp.path().join("HEAD");
        std::fs::write(&head_file, "ref: refs/heads/main").unwrap();
        let dir = temp.path().join("project");
        std::fs::create_dir_all(&dir).unwrap();

        cache.set_head_file(&dir, &head_file);

        let result = cache.get_head_file(&dir);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), head_file);
    }

    #[test]
    fn test_set_with_head_file_then_get_head_file() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("test.db");
        let cache = ShortpathCache::new(db_path);

        let head_file = temp.path().join("HEAD");
        std::fs::write(&head_file, "ref: refs/heads/main").unwrap();
        let dir = Path::new("/my/project");
        let sp = make_shortpath();

        // set() stores both shortpath_json AND head_file_path
        cache.set(dir, &sp, Some(&head_file));

        // get_head_file() should return it
        let result = cache.get_head_file(dir);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), head_file);
    }
}
