use crate::config::ConfigManager;
use crate::utils::expand_path;
use rusqlite::{params, Connection, OpenFlags, Result as SqlResult};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

// Store frecency data as (rank, last_accessed_epoch)
const HOUR: u64 = 3600;
const DAY: u64 = 24 * HOUR;
const WEEK: u64 = 7 * DAY;
const DAYS_TO_KEEP: i64 = 30;

/// Minimum seconds between access-only (visit_count=0) DB writes per path.
/// Visits (visit_count>0) are always recorded immediately.
const THROTTLE_SECS: i64 = 60;

/// How often (in seconds) to run the old-record cleanup DELETE.
const PRUNE_INTERVAL_SECS: i64 = 6 * HOUR as i64;

pub struct FrecencyDb {
    db_path: PathBuf,
}

impl FrecencyDb {
    pub fn new() -> Self {
        Self::from_config()
    }

    pub fn from_config() -> Self {
        // In tests, don't create config files automatically
        let config = if cfg!(test) {
            ConfigManager::load_config_with_options(false)
        } else {
            ConfigManager::load_config()
        };
        let db_path = expand_path(&config.frecency_db_path);

        // Ensure parent directory exists
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }

        Self { db_path }
    }

    pub fn with_path(db_path: PathBuf) -> Self {
        // Ensure parent directory exists
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }

        Self { db_path }
    }

    /// Return the path to the SQLite database file.
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// Open a read-write connection and ensure the full schema exists.
    /// Used for reads (get_raw_scores) and writes when throttle is bypassed.
    fn open_db(&self) -> SqlResult<Connection> {
        let conn = Connection::open(&self.db_path)?;

        conn.execute_batch(
            "PRAGMA journal_mode=WAL;

            CREATE TABLE IF NOT EXISTS frecency (
                path TEXT NOT NULL,
                date INTEGER NOT NULL,
                visits INTEGER NOT NULL DEFAULT 0,
                last_accessed INTEGER NOT NULL,
                PRIMARY KEY (path, date)
            );

            CREATE INDEX IF NOT EXISTS idx_frecency_date ON frecency(date);

            CREATE TABLE IF NOT EXISTS metadata (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS shortpath_cache (
                directory TEXT PRIMARY KEY,
                head_file_path TEXT,
                head_mtime INTEGER,
                shortpath_json TEXT,
                cached_at INTEGER NOT NULL
            );",
        )?;

        Ok(conn)
    }

    /// Open a read-only connection. Fails if the DB file does not exist.
    /// Used for the throttle check before deciding whether to write.
    fn open_db_readonly(&self) -> SqlResult<Connection> {
        Connection::open_with_flags(
            &self.db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_URI
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
    }

    /// Check whether a write is needed (throttle) and whether pruning is due.
    /// Opens a read-only connection; returns (needs_write, needs_prune).
    /// Falls back to (true, true) if the read-only open fails.
    fn check_needs_update(
        &self,
        canonical_path: &str,
        visit_count: i64,
        now: i64,
        today: i64,
    ) -> (bool, bool) {
        let conn = match self.open_db_readonly() {
            Ok(c) => c,
            // Can't open read-only (e.g., WAL files not initialised yet) — assume write needed
            Err(_) => return (true, true),
        };

        // Access-only updates are throttled; visits always go through
        let is_throttled = if visit_count == 0 {
            let last_accessed: Option<i64> = conn
                .query_row(
                    "SELECT MAX(last_accessed) FROM frecency WHERE path = ?1 AND date = ?2",
                    params![canonical_path, today],
                    |row| row.get(0),
                )
                .ok()
                .flatten();

            last_accessed
                .map(|la| now - la < THROTTLE_SECS)
                .unwrap_or(false)
        } else {
            false
        };

        if is_throttled {
            return (false, false);
        }

        // Only bother checking prune if we're going to write anyway
        let needs_prune = {
            let last_pruned: Option<i64> = conn
                .query_row(
                    "SELECT CAST(value AS INTEGER) FROM metadata WHERE key = 'last_pruned'",
                    [],
                    |row| row.get(0),
                )
                .ok()
                .flatten();

            // Prune if never done, or interval has elapsed; also handle missing table gracefully
            last_pruned
                .map(|lp| now - lp > PRUNE_INTERVAL_SECS)
                .unwrap_or(true)
        };

        (true, needs_prune)
    }

    pub fn visit(&self, path: &str, visit_count: i64) -> Result<(), Box<dyn std::error::Error>> {
        let canonical_path = Path::new(path)
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(path))
            .to_string_lossy()
            .into_owned();

        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
        let today = now / DAY as i64;

        // Phase 1: read-only throttle + prune check (skips write lock entirely when not needed)
        let (needs_write, needs_prune) = if self.db_path.exists() {
            self.check_needs_update(&canonical_path, visit_count, now, today)
        } else {
            // DB doesn't exist yet — always write, always initialise schema
            (true, false)
        };

        if !needs_write {
            return Ok(());
        }

        // Phase 2: open read-write (schema init / migration handled here via IF NOT EXISTS)
        let conn = self.open_db()?;

        conn.execute(
            "INSERT INTO frecency (path, date, visits, last_accessed)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(path, date) DO UPDATE SET
                visits = visits + ?3,
                last_accessed = ?4",
            params![canonical_path, today, visit_count, now],
        )?;

        if needs_prune {
            conn.execute(
                "DELETE FROM frecency WHERE date < ?1",
                params![today - DAYS_TO_KEEP],
            )?;
            conn.execute(
                "INSERT OR REPLACE INTO metadata (key, value) VALUES ('last_pruned', ?1)",
                params![now.to_string()],
            )?;
        }

        Ok(())
    }

    fn get_raw_scores(&self) -> Result<HashMap<String, f64>, Box<dyn std::error::Error>> {
        let conn = self.open_db()?;

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let mut stmt = conn.prepare(
            "SELECT path, SUM(visits), MAX(last_accessed)
             FROM frecency
             GROUP BY path",
        )?;

        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;

        Ok(rows
            .filter_map(Result::ok)
            .map(|(path, visits, last_accessed)| {
                (
                    path,
                    calculate_score(visits as f64, last_accessed as u64, now),
                )
            })
            .collect())
    }

    /// Get all scores from the database
    pub fn get_scores(&self) -> HashMap<String, f64> {
        let mut scores = self.get_raw_scores().unwrap_or_default();

        // Normalize scores to max of 100
        if let Some(&max_score) = scores
            .values()
            .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        {
            if max_score > 0.0 {
                let scale = 100.0 / max_score;
                scores
                    .values_mut()
                    .for_each(|score| *score = (*score * scale).round());
            }
        }

        scores
    }

    /// Get score for a specific path
    pub fn get_score(&self, path: &str) -> f64 {
        let canonical_path = Path::new(path)
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(path))
            .to_string_lossy()
            .into_owned();

        self.get_scores()
            .get(&canonical_path)
            .copied()
            .unwrap_or(0.0)
    }
}

impl Default for FrecencyDb {
    fn default() -> Self {
        Self::new()
    }
}

/// Calculate frecency score with time decay
fn calculate_score(visits: f64, last_accessed: u64, now: u64) -> f64 {
    let age = now.saturating_sub(last_accessed);

    let multiplier = match age {
        0..HOUR => 4.0,
        HOUR..DAY => 2.0,
        DAY..WEEK => 0.5,
        _ => 0.25,
    };

    visits * multiplier
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_frecency_basic() {
        let temp_dir = TempDir::new().unwrap();
        let db = FrecencyDb::with_path(temp_dir.path().join("test.db"));

        // Visit paths (visits always bypass throttle)
        db.visit("/test/path1", 1).unwrap();
        db.visit("/test/path1", 1).unwrap();
        db.visit("/test/path2", 1).unwrap();

        let scores = db.get_scores();

        // More visits = higher score
        assert!(
            scores.get("/test/path1").unwrap_or(&0.0) > scores.get("/test/path2").unwrap_or(&0.0)
        );

        // Check specific path score
        let score = db.get_score("/test/path1");
        assert!(score > 0.0);
    }

    #[test]
    fn test_visit_with_different_counts() {
        let temp_dir = TempDir::new().unwrap();
        let db = FrecencyDb::with_path(temp_dir.path().join("test.db"));

        // Visit with different counts
        db.visit("/test/path1", 5).unwrap();
        db.visit("/test/path2", 1).unwrap();
        db.visit("/test/path3", 0).unwrap(); // Access only

        let scores = db.get_scores();

        // Path1 should have highest score
        let score1 = scores.get("/test/path1").unwrap_or(&0.0);
        let score2 = scores.get("/test/path2").unwrap_or(&0.0);
        let score3 = scores.get("/test/path3").unwrap_or(&0.0);

        assert!(score1 > score2);
        assert!(score2 > score3);
        assert_eq!(*score3, 0.0); // Access only should have zero score
    }

    #[test]
    fn test_nonexistent_path_score() {
        let temp_dir = TempDir::new().unwrap();
        let db = FrecencyDb::with_path(temp_dir.path().join("test.db"));

        // Score for non-existent path should be 0
        assert_eq!(db.get_score("/nonexistent/path"), 0.0);
    }

    #[test]
    fn test_score_normalization() {
        let temp_dir = TempDir::new().unwrap();
        let db = FrecencyDb::with_path(temp_dir.path().join("test.db"));

        // Create entries with different visit counts
        db.visit("/test/most_visited", 100).unwrap();
        db.visit("/test/medium_visited", 50).unwrap();
        db.visit("/test/least_visited", 10).unwrap();

        let scores = db.get_scores();

        // Most visited should have score of 100 (normalized)
        let max_score = scores.get("/test/most_visited").unwrap_or(&0.0);
        assert_eq!(*max_score, 100.0);

        // Other scores should be proportionally normalized
        let medium_score = scores.get("/test/medium_visited").unwrap_or(&0.0);
        assert_eq!(*medium_score, 50.0);

        let min_score = scores.get("/test/least_visited").unwrap_or(&0.0);
        assert_eq!(*min_score, 10.0);
    }

    #[test]
    fn test_calculate_score_time_decay() {
        let now = 1000000;

        // Same visit count, different ages
        let score_recent = calculate_score(10.0, now - 1800, now); // 30 minutes ago
        let score_today = calculate_score(10.0, now - 7200, now); // 2 hours ago
        let score_this_week = calculate_score(10.0, now - 86400, now); // 1 day ago
        let score_old = calculate_score(10.0, now - 604800, now); // 1 week ago

        // Recent visits should have higher scores
        assert!(score_recent > score_today);
        assert!(score_today > score_this_week);
        assert!(score_this_week > score_old);

        // Verify multipliers
        assert_eq!(score_recent, 40.0); // 10 * 4.0
        assert_eq!(score_today, 20.0); // 10 * 2.0
        assert_eq!(score_this_week, 5.0); // 10 * 0.5
        assert_eq!(score_old, 2.5); // 10 * 0.25
    }

    #[test]
    fn test_multiple_visits_same_day() {
        let temp_dir = TempDir::new().unwrap();
        let db = FrecencyDb::with_path(temp_dir.path().join("test.db"));

        // Multiple visits to same path on same day should accumulate
        // Visits (count > 0) bypass the throttle
        db.visit("/test/path", 1).unwrap();
        db.visit("/test/path", 1).unwrap();
        db.visit("/test/path", 1).unwrap();

        let scores = db.get_scores();
        let score = scores.get("/test/path").unwrap_or(&0.0);

        // Score should reflect total visits
        assert!(score > &0.0);
    }

    #[test]
    fn test_path_normalization() {
        let temp_dir = TempDir::new().unwrap();
        let db = FrecencyDb::with_path(temp_dir.path().join("test.db"));

        // Create a temporary directory to test path normalization
        let test_dir = TempDir::new().unwrap();
        let test_path = test_dir.path().to_str().unwrap();

        // Visit the same path with different representations
        db.visit(test_path, 1).unwrap();
        db.visit(&format!("{test_path}/."), 1).unwrap();

        // Both visits should be recorded for the same canonical path
        let score = db.get_score(test_path);
        assert!(score > 0.0);

        // Verify the visits were combined
        let scores = db.get_scores();
        let canonical_path = Path::new(test_path)
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned();

        let canonical_score = scores.get(&canonical_path).unwrap_or(&0.0);
        assert!(canonical_score > &0.0);
    }

    #[test]
    fn test_throttle_skips_access_only_updates() {
        let temp_dir = TempDir::new().unwrap();
        let db = FrecencyDb::with_path(temp_dir.path().join("test.db"));

        // First access-only update always goes through (no prior record)
        db.visit("/test/path", 0).unwrap();

        let scores1 = db.get_raw_scores().unwrap();
        let last_accessed1 = {
            let conn = db.open_db().unwrap();
            conn.query_row(
                "SELECT last_accessed FROM frecency WHERE path = '/test/path'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
        };

        // Immediate second access-only update should be throttled (same second)
        db.visit("/test/path", 0).unwrap();

        let scores2 = db.get_raw_scores().unwrap();
        let last_accessed2 = {
            let conn = db.open_db().unwrap();
            conn.query_row(
                "SELECT last_accessed FROM frecency WHERE path = '/test/path'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
        };

        // last_accessed should not change (throttled)
        assert_eq!(
            last_accessed1, last_accessed2,
            "Throttled access should not update last_accessed"
        );
        // Score should be unchanged (still 0 for access-only)
        assert_eq!(
            scores1.get("/test/path"),
            scores2.get("/test/path"),
            "Throttled access should not change score"
        );
    }

    #[test]
    fn test_throttle_does_not_block_visits() {
        let temp_dir = TempDir::new().unwrap();
        let db = FrecencyDb::with_path(temp_dir.path().join("test.db"));

        // Visits (count > 0) always bypass the throttle
        db.visit("/test/path", 1).unwrap();
        let scores1 = db.get_raw_scores().unwrap();
        let score1 = scores1.get("/test/path").copied().unwrap_or(0.0);

        // Immediate second visit — should NOT be throttled
        db.visit("/test/path", 1).unwrap();
        let scores2 = db.get_raw_scores().unwrap();
        let score2 = scores2.get("/test/path").copied().unwrap_or(0.0);

        assert!(
            score2 > score1,
            "Second visit should increase score: {score1} -> {score2}"
        );
    }

    #[test]
    fn test_prune_runs_periodically() {
        let temp_dir = TempDir::new().unwrap();
        let db = FrecencyDb::with_path(temp_dir.path().join("test.db"));

        // Insert an old record directly to simulate stale data
        {
            let conn = db.open_db().unwrap();
            let old_date = -100_i64; // way in the past
            conn.execute(
                "INSERT INTO frecency (path, date, visits, last_accessed) VALUES (?1, ?2, 5, 0)",
                params!["/old/path", old_date],
            )
            .unwrap();
        }

        // Verify old record exists
        {
            let conn = db.open_db().unwrap();
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM frecency WHERE path = '/old/path'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "Old record should exist before prune");
        }

        // Visit with no prior last_pruned → prune should run
        db.visit("/test/path", 1).unwrap();

        // Verify old record was pruned
        {
            let conn = db.open_db().unwrap();
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM frecency WHERE path = '/old/path'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 0, "Old record should be pruned after first write");
        }

        // Verify last_pruned was recorded
        {
            let conn = db.open_db().unwrap();
            let last_pruned: Option<String> = conn
                .query_row(
                    "SELECT value FROM metadata WHERE key = 'last_pruned'",
                    [],
                    |row| row.get(0),
                )
                .ok();
            assert!(last_pruned.is_some(), "last_pruned should be set in metadata");
        }
    }

    #[test]
    fn test_update_frecency_visit() {
        use crate::test_utils::test_env::TestEnvironment;

        let env = TestEnvironment::new();
        env.write_config(None);
        env.set_config_env();

        let test_path = env.create_git_repo("test-repo");

        // Use FrecencyDb with the test environment's path
        let frecency_db = FrecencyDb::with_path(env.frecency_db_path.clone());
        frecency_db.visit(&test_path.to_string_lossy(), 1).unwrap();

        // Verify the visit was recorded
        let score = frecency_db.get_score(&test_path.to_string_lossy());
        assert!(score > 0.0, "Visit should increase frecency score");
    }

    #[test]
    fn test_update_frecency_access() {
        use crate::test_utils::test_env::TestEnvironment;

        let env = TestEnvironment::new();
        // Write config before setting env var to ensure file exists
        env.write_config(None);
        env.set_config_env();

        let test_path = env.create_git_repo("test-repo");

        // Use FrecencyDb with the test environment's path
        let frecency_db = FrecencyDb::with_path(env.frecency_db_path.clone());
        frecency_db.visit(&test_path.to_string_lossy(), 0).unwrap();

        // Verify the access was recorded
        let score = frecency_db.get_score(&test_path.to_string_lossy());
        assert_eq!(
            score, 0.0,
            "Access (visit with 0 count) should not increase frecency score"
        );
    }

    #[test]
    fn test_update_frecency_multiple_visits() {
        use crate::test_utils::test_env::TestEnvironment;

        let env = TestEnvironment::new();
        env.write_config(None);
        env.set_config_env();

        // Use a simple path that doesn't need canonicalization
        let test_path = "/test/simple/path";

        // Use FrecencyDb with the test environment's path
        let frecency_db = FrecencyDb::with_path(env.frecency_db_path.clone());

        // First visit
        frecency_db.visit(test_path, 1).unwrap();
        let first_raw_scores = frecency_db.get_raw_scores().unwrap();
        let first_score = first_raw_scores.get(test_path).copied().unwrap_or(0.0);
        assert!(
            first_score > 0.0,
            "First visit should create a positive score"
        );

        // Second visit (visits bypass throttle)
        frecency_db.visit(test_path, 1).unwrap();
        let second_raw_scores = frecency_db.get_raw_scores().unwrap();
        let second_score = second_raw_scores.get(test_path).copied().unwrap_or(0.0);

        assert!(
            second_score > first_score,
            "Multiple visits should increase score: first={first_score}, second={second_score}"
        );
    }

    #[test]
    fn test_update_frecency_nonexistent_path() {
        use crate::test_utils::test_env::TestEnvironment;

        let env = TestEnvironment::new();
        env.write_config(None);
        env.set_config_env();

        // Test with non-existent path (should still work as it will be stored as-is)
        let frecency_db = FrecencyDb::with_path(env.frecency_db_path.clone());
        let nonexistent_path = "/nonexistent/path";

        frecency_db.visit(nonexistent_path, 1).unwrap();

        let score = frecency_db.get_score(nonexistent_path);
        assert!(
            score > 0.0,
            "Should be able to record frecency for non-existent paths"
        );
    }
}
