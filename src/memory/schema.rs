use std::sync::Once;

use rusqlite::{ffi::sqlite3_auto_extension, Connection};

/// `mistral-embed`'s real output dimensionality — drives the `vec0` column
/// width for both fact and code-chunk embeddings.
pub const EMBEDDING_DIMS: usize = 1024;

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    project_root TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    role TEXT NOT NULL,
    content TEXT,
    tool_calls TEXT,
    token_count INTEGER NOT NULL,
    timestamp INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id, id);
CREATE TABLE IF NOT EXISTS facts (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    embedding BLOB,
    last_updated INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS code_chunks_meta (
    id INTEGER PRIMARY KEY,
    file_path TEXT NOT NULL,
    chunk_text TEXT NOT NULL,
    start_line INTEGER NOT NULL,
    end_line INTEGER NOT NULL
);
"#;

static REGISTER_EXTENSION: Once = Once::new();

/// Registers the sqlite-vec extension process-wide. Safe to call more than
/// once (guarded by `Once`) since every `MemoryManager::open` call needs it
/// done before opening a connection that uses `vec0`.
fn register_extension() {
    REGISTER_EXTENSION.call_once(|| unsafe {
        sqlite3_auto_extension(Some(std::mem::transmute(sqlite_vec::sqlite3_vec_init as *const ())));
    });
}

pub fn open(path: &std::path::Path) -> rusqlite::Result<Connection> {
    register_extension();
    let conn = Connection::open(path)?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
    conn.execute_batch(SCHEMA_SQL)?;
    conn.execute_batch(&format!("CREATE VIRTUAL TABLE IF NOT EXISTS code_chunks USING vec0(embedding float[{EMBEDDING_DIMS}]);"))?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zerocopy::IntoBytes;

    #[test]
    fn open_creates_schema_and_vec0_table_is_queryable() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(&dir.path().join("test.db")).unwrap();

        let version: String = conn.query_row("select vec_version()", [], |r| r.get(0)).unwrap();
        assert!(version.starts_with('v'));

        // Re-opening (idempotent schema) must not error.
        drop(conn);
        let _conn2 = open(&dir.path().join("test.db")).unwrap();
    }

    #[test]
    fn vec0_insert_with_matched_rowid_and_knn_query_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(&dir.path().join("test.db")).unwrap();

        // Use a small 4-dim vector here purely to prove the mechanics
        // (rowid matching + KNN) cheaply; production code uses EMBEDDING_DIMS.
        conn.execute_batch("CREATE VIRTUAL TABLE probe USING vec0(embedding float[4]);").unwrap();
        conn.execute("CREATE TABLE probe_meta(id INTEGER PRIMARY KEY, label TEXT)", []).unwrap();

        let vectors: [(&str, [f32; 4]); 3] = [("near", [1.0, 0.0, 0.0, 0.0]), ("mid", [0.0, 1.0, 0.0, 0.0]), ("far", [-1.0, 0.0, 0.0, 0.0])];

        for (label, vec) in &vectors {
            conn.execute("INSERT INTO probe_meta(label) VALUES (?1)", [label]).unwrap();
            let rowid = conn.last_insert_rowid();
            conn.execute("INSERT INTO probe(rowid, embedding) VALUES (?1, ?2)", rusqlite::params![rowid, vec.as_bytes()]).unwrap();
        }

        let query = [1.0f32, 0.0, 0.0, 0.0];
        let nearest: String = conn
            .query_row(
                "SELECT probe_meta.label FROM probe JOIN probe_meta ON probe.rowid = probe_meta.id \
                 WHERE probe.embedding MATCH ?1 AND k = 1 ORDER BY probe.distance",
                rusqlite::params![query.as_bytes()],
                |r| r.get(0),
            )
            .unwrap();

        assert_eq!(nearest, "near");
    }
}
