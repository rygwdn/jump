/// Reftable-format HEAD reader.
///
/// Git repos with `extensions.refStorage = reftable` store refs in a compact
/// binary format under `<gitdir>/reftable/` instead of text files.  For linked
/// worktrees the per-worktree HEAD lives in `<gitdir>/reftable/` where
/// `<gitdir>` is `.git/worktrees/<name>/`.
///
/// This module only implements the narrow slice of the spec needed to read the
/// `HEAD` symbolic ref: header + first ref-block record.  It never loads an
/// entire block into memory, which keeps things fast even for very large repos.
use std::fs;
use std::io::{BufReader, Read};
use std::path::Path;

const REFTABLE_MAGIC: &[u8; 4] = b"REFT";
const FILE_HEADER_LEN: u64 = 24;

const BLOCK_TYPE_REF: u8 = b'r';
// Reftable value types (low 3 bits of the second key varint)
// 0x0 = deletion, 0x1 = one OID, 0x2 = two OIDs (peeled), 0x3 = symbolic ref
const VALUE_TYPE_SYMREF: u64 = 3;

/// Outcome of looking for HEAD in a single reftable file.
enum HeadResult {
    /// HEAD is a symbolic ref; contains the raw target (e.g. `refs/heads/main`).
    Symbolic(String),
    /// HEAD was found but is not a symbolic ref (detached or a deletion record).
    NotSymbolic,
    /// HEAD is not present in this table (check an older table in the stack).
    NotPresent,
}

/// Read the HEAD branch from a reftable-format git directory.
///
/// Returns the branch name (stripping `refs/heads/`) when HEAD is a symbolic
/// ref to a non-default branch, or `None` if:
/// - HEAD is detached
/// - HEAD points to `main` or `master` (same convention as the text-HEAD reader)
/// - The gitdir does not use reftable storage
/// - Any parse error occurs
pub fn read_head_from_reftable(gitdir: &Path) -> Option<String> {
    let reftable_dir = gitdir.join("reftable");
    if !reftable_dir.is_dir() {
        return None;
    }

    let tables_content = fs::read_to_string(reftable_dir.join("tables.list")).ok()?;

    // Tables are listed oldest → newest; search newest-first so we pick up
    // the most recent write to HEAD.
    for table_name in tables_content.lines().rev() {
        let table_name = table_name.trim();
        if table_name.is_empty() {
            continue;
        }
        match search_head_in_table(&reftable_dir.join(table_name)) {
            HeadResult::Symbolic(target) => {
                let branch = target.strip_prefix("refs/heads/")?;
                if branch == "main" || branch == "master" || branch.is_empty() {
                    return None;
                }
                return Some(branch.to_string());
            }
            HeadResult::NotSymbolic => return None,
            HeadResult::NotPresent => continue,
        }
    }
    None
}

/// Search for the HEAD record in a single reftable `.ref` file.
///
/// Only reads the 24-byte file header, 4-byte block header, and the bytes
/// of the first record — never the whole block.
fn search_head_in_table(path: &Path) -> HeadResult {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return HeadResult::NotPresent,
    };
    let mut reader = BufReader::new(file);

    if !check_reftable_magic(&mut reader) {
        return HeadResult::NotPresent;
    }

    // Skip the rest of the 24-byte file header (magic already consumed = 4 bytes;
    // remaining = 20 bytes: version(1) + block_size(3) + min_idx(8) + max_idx(8)).
    if skip_bytes(&mut reader, FILE_HEADER_LEN - 4).is_err() {
        return HeadResult::NotPresent;
    }

    // Read the 4-byte block header.
    let mut block_hdr = [0u8; 4];
    if reader.read_exact(&mut block_hdr).is_err() {
        return HeadResult::NotPresent;
    }

    // If the first block isn't a ref block there are no refs in this file.
    if block_hdr[0] != BLOCK_TYPE_REF {
        return HeadResult::NotPresent;
    }

    // Records start immediately after the block header — parse the first one.
    parse_first_ref_record(&mut reader)
}

/// Parse the very first record in a ref block and check whether it is HEAD.
///
/// Reftable record key encoding (two separate varints):
///   varint( prefix_length )
///   varint( (suffix_length << 3) | value_type )
///   u8[suffix_length]  ← key suffix bytes (NOT NUL-terminated)
///   varint( update_index_delta )
///   [payload depends on value_type]
///
/// Symref payload (value_type == 3):
///   varint( target_length )
///   u8[target_length]  ← target bytes (NOT NUL-terminated)
fn parse_first_ref_record<R: Read>(reader: &mut R) -> HeadResult {
    // First varint: prefix_length.
    let prefix_len = match read_varint(reader) {
        Some(v) => v,
        None => return HeadResult::NotPresent,
    };

    // The very first record in a block always has prefix_len == 0.
    if prefix_len != 0 {
        return HeadResult::NotPresent;
    }

    // Second varint: (suffix_length << 3) | value_type.
    let suffix_varint = match read_varint(reader) {
        Some(v) => v,
        None => return HeadResult::NotPresent,
    };
    let suffix_len = (suffix_varint >> 3) as usize;
    let value_type = suffix_varint & 0x7;

    // Read exactly suffix_len bytes for the key.
    let mut key = vec![0u8; suffix_len];
    if reader.read_exact(&mut key).is_err() {
        return HeadResult::NotPresent;
    }

    if key != b"HEAD" {
        // First record is not HEAD — HEAD is absent from this table.
        return HeadResult::NotPresent;
    }

    // Skip the update_index_delta varint.
    if read_varint(reader).is_none() {
        return HeadResult::NotSymbolic;
    }

    if value_type != VALUE_TYPE_SYMREF {
        // Detached HEAD (OID) or deletion record.
        return HeadResult::NotSymbolic;
    }

    // Symref payload: varint(target_length) then target bytes (not NUL-terminated).
    let target_len = match read_varint(reader) {
        Some(n) => n as usize,
        None => return HeadResult::NotSymbolic,
    };
    let mut target_bytes = vec![0u8; target_len];
    if reader.read_exact(&mut target_bytes).is_err() {
        return HeadResult::NotSymbolic;
    }
    match String::from_utf8(target_bytes) {
        Ok(target) => HeadResult::Symbolic(target),
        Err(_) => HeadResult::NotSymbolic,
    }
}

// ── low-level I/O helpers ────────────────────────────────────────────────────

fn check_reftable_magic<R: Read>(reader: &mut R) -> bool {
    let mut magic = [0u8; 4];
    reader.read_exact(&mut magic).is_ok() && &magic == REFTABLE_MAGIC
}

fn skip_bytes<R: Read>(reader: &mut R, n: u64) -> std::io::Result<()> {
    // Read and discard `n` bytes using a small stack buffer.
    let mut buf = [0u8; 64];
    let mut remaining = n;
    while remaining > 0 {
        let chunk = remaining.min(buf.len() as u64) as usize;
        reader.read_exact(&mut buf[..chunk])?;
        remaining -= chunk as u64;
    }
    Ok(())
}

/// Decode a base-128 (LEB128-style) varint, returning the decoded value.
fn read_varint<R: Read>(reader: &mut R) -> Option<u64> {
    let mut result = 0u64;
    let mut shift = 0u32;
    loop {
        let mut byte = [0u8; 1];
        reader.read_exact(&mut byte).ok()?;
        let b = byte[0] as u64;
        result |= (b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Some(result);
        }
        shift += 7;
        if shift >= 64 {
            return None; // overflow guard
        }
    }
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
pub mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Write a minimal reftable stack under `<gitdir>/reftable/` with a single
    /// table whose HEAD record points at `target` (e.g. `"refs/heads/feature"`).
    /// Used by other test modules that need a realistic gitdir.
    pub fn make_reftable_gitdir(gitdir: &Path, target: &str) {
        let payload = symref_payload(target.as_bytes());
        let table = build_reftable(3, &payload);
        make_table_dir(&gitdir.join("reftable"), &[("0001.ref", table)]);
    }

    /// Build a minimal valid reftable `.ref` file with a single HEAD record.
    ///
    /// Layout:
    ///   [24] file header
    ///   [4]  block header  (type='r', block_len = 4 + record_bytes)
    ///   [N]  first record
    ///   [footer — omitted, not needed for this parser]
    fn build_reftable(head_value_type: u8, head_payload: &[u8]) -> Vec<u8> {
        // Correct reftable record encoding:
        //   varint: prefix_length = 0
        //   varint: (suffix_length << 3) | value_type
        //   u8[suffix_length]: key bytes (NOT NUL-terminated)
        //   varint: update_index_delta = 0
        //   payload (for symref: varint(target_len) + target bytes, no NUL)
        let suffix = b"HEAD";
        let mut record = Vec::new();
        record.push(0u8); // prefix_length = 0
        record.push(((suffix.len() as u8) << 3) | (head_value_type & 0x7)); // (4<<3)|vt
        record.extend_from_slice(suffix); // "HEAD" (no NUL)
        record.push(0); // update_index_delta = 0
        record.extend_from_slice(head_payload);

        let block_len = 4 + record.len();

        let mut data = Vec::new();

        // File header (24 bytes)
        data.extend_from_slice(b"REFT"); // magic
        data.push(1); // version
                      // block_size = 4096 (arbitrary, not used by parser)
        data.extend_from_slice(&[0x00, 0x10, 0x00]);
        data.extend_from_slice(&0u64.to_be_bytes()); // min_update_index
        data.extend_from_slice(&1u64.to_be_bytes()); // max_update_index

        // Block header (4 bytes)
        data.push(b'r');
        let bl = block_len as u32;
        data.extend_from_slice(&[(bl >> 16) as u8, (bl >> 8) as u8, bl as u8]);

        // Record
        data.extend_from_slice(&record);

        data
    }

    fn make_table_dir(dir: &Path, tables: &[(&str, Vec<u8>)]) {
        fs::create_dir_all(dir).unwrap();
        let names: Vec<&str> = tables.iter().map(|(n, _)| *n).collect();
        fs::write(dir.join("tables.list"), names.join("\n") + "\n").unwrap();
        for (name, data) in tables {
            fs::write(dir.join(name), data).unwrap();
        }
    }

    /// Encode a symref target as varint(len) + bytes (no NUL).
    fn symref_payload(target: &[u8]) -> Vec<u8> {
        let mut p = Vec::new();
        p.push(target.len() as u8); // varint length (fits in 1 byte for realistic targets)
        p.extend_from_slice(target);
        p
    }

    #[test]
    fn test_symbolic_ref_branch() {
        let tmp = TempDir::new().unwrap();
        let table = build_reftable(3, &symref_payload(b"refs/heads/feature-x"));
        make_table_dir(&tmp.path().join("reftable"), &[("0001.ref", table)]);

        let branch = read_head_from_reftable(tmp.path());
        assert_eq!(branch, Some("feature-x".to_string()));
    }

    #[test]
    fn test_main_branch_returns_none() {
        let tmp = TempDir::new().unwrap();
        let table = build_reftable(3, &symref_payload(b"refs/heads/main"));
        make_table_dir(&tmp.path().join("reftable"), &[("0001.ref", table)]);

        assert_eq!(read_head_from_reftable(tmp.path()), None);
    }

    #[test]
    fn test_master_branch_returns_none() {
        let tmp = TempDir::new().unwrap();
        let table = build_reftable(3, &symref_payload(b"refs/heads/master"));
        make_table_dir(&tmp.path().join("reftable"), &[("0001.ref", table)]);

        assert_eq!(read_head_from_reftable(tmp.path()), None);
    }

    #[test]
    fn test_detached_head_returns_none() {
        let tmp = TempDir::new().unwrap();
        // value_type=1 (one OID), 20-byte SHA-1 payload
        let oid = [0xdeu8; 20];
        let table = build_reftable(1, &oid);
        make_table_dir(&tmp.path().join("reftable"), &[("0001.ref", table)]);

        assert_eq!(read_head_from_reftable(tmp.path()), None);
    }

    #[test]
    fn test_no_reftable_dir_returns_none() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(read_head_from_reftable(tmp.path()), None);
    }

    #[test]
    fn test_multi_table_stack_newest_first() {
        // Older table has "feature-old", newer has "feature-new".
        // Should return the value from the newer table.
        let tmp = TempDir::new().unwrap();
        let old_table = build_reftable(3, &symref_payload(b"refs/heads/feature-old"));
        let new_table = build_reftable(3, &symref_payload(b"refs/heads/feature-new"));
        make_table_dir(
            &tmp.path().join("reftable"),
            &[("0001.ref", old_table), ("0002.ref", new_table)],
        );

        assert_eq!(
            read_head_from_reftable(tmp.path()),
            Some("feature-new".to_string())
        );
    }

    #[test]
    fn test_head_absent_in_newest_falls_back_to_older() {
        // Newest table has no HEAD record (first key is "refs/heads/foo").
        // Older table has HEAD → feature-old.  Should return feature-old.
        let tmp = TempDir::new().unwrap();
        let old_table = build_reftable(3, &symref_payload(b"refs/heads/feature-old"));

        // Build a table whose first record is NOT HEAD.
        let mut non_head_record = Vec::new();
        non_head_record.push(2u8); // value_type=2, prefix_len=0
        non_head_record.extend_from_slice(b"refs/heads/foo\0");
        non_head_record.push(0); // update_index_delta
        non_head_record.extend_from_slice(b"refs/heads/foo\0");
        let block_len = 4 + non_head_record.len();
        let mut new_table_bytes = Vec::new();
        new_table_bytes.extend_from_slice(b"REFT");
        new_table_bytes.push(1);
        new_table_bytes.extend_from_slice(&[0x00, 0x10, 0x00]);
        new_table_bytes.extend_from_slice(&0u64.to_be_bytes());
        new_table_bytes.extend_from_slice(&2u64.to_be_bytes());
        new_table_bytes.push(b'r');
        let bl = block_len as u32;
        new_table_bytes.extend_from_slice(&[(bl >> 16) as u8, (bl >> 8) as u8, bl as u8]);
        new_table_bytes.extend_from_slice(&non_head_record);

        make_table_dir(
            &tmp.path().join("reftable"),
            &[("0001.ref", old_table), ("0002.ref", new_table_bytes)],
        );

        assert_eq!(
            read_head_from_reftable(tmp.path()),
            Some("feature-old".to_string())
        );
    }
}
