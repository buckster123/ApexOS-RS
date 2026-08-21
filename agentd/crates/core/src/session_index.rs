//! FTS5 overlay for session JSONL (`docs/session-rag.md` S2).
//!
//! JSONL remains the verbatim store. This index is derived: insert on append,
//! catch-up at boot, search prefers it and falls back to a file scan. No
//! embeddings. Worker/spawn rows are never written (the search corpus law).

use crate::transcript::{clip_snippet, fts_match_query, query_terms, searchable_text, TranscriptHit};
use crate::{is_spawn_session, is_worker_session, Message};
use rusqlite::{params, Connection, OpenFlags};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Derived FTS5 index at `sessions/index.sqlite`.
#[derive(Clone)]
pub struct SessionIndex {
    inner: Arc<Mutex<Connection>>,
    path: PathBuf,
}

impl SessionIndex {
    /// Open (create) the index. WAL + busy timeout; fails closed (caller falls back).
    pub fn open(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("index dir: {e}"))?;
        }
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_FULL_MUTEX,
        )
        .map_err(|e| format!("open {}: {e}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| e.to_string())?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             CREATE VIRTUAL TABLE IF NOT EXISTS session_fts USING fts5(
                 session_id UNINDEXED,
                 msg_index UNINDEXED,
                 role UNINDEXED,
                 body,
                 tokenize = 'unicode61'
             );",
        )
        .map_err(|e| format!("init fts5: {e}"))?;
        Ok(Self {
            inner: Arc::new(Mutex::new(conn)),
            path,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn conn(&self) -> Result<std::sync::MutexGuard<'_, Connection>, String> {
        Ok(self.inner.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Next `msg_index` for `session_id` (0 if none).
    pub fn next_index(&self, session_id: u64) -> Result<u32, String> {
        let conn = self.conn()?;
        let max: Option<i64> = conn
            .query_row(
                "SELECT MAX(CAST(msg_index AS INTEGER)) FROM session_fts WHERE session_id = ?1",
                params![session_id.to_string()],
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())?;
        Ok(max.map(|m| (m + 1) as u32).unwrap_or(0))
    }

    pub fn has_session(&self, session_id: u64) -> bool {
        let Ok(conn) = self.conn() else { return false };
        conn.query_row(
            "SELECT 1 FROM session_fts WHERE session_id = ?1 LIMIT 1",
            params![session_id.to_string()],
            |_| Ok(()),
        )
        .is_ok()
    }

    /// Index one already-appended message. No-op for worker/spawn.
    pub fn insert(&self, session_id: u64, msg: &Message) -> Result<u32, String> {
        if is_worker_session(session_id) || is_spawn_session(session_id) {
            return Ok(0);
        }
        let (role, body) = searchable_text(msg);
        let conn = self.conn()?;
        let max: Option<i64> = conn
            .query_row(
                "SELECT MAX(CAST(msg_index AS INTEGER)) FROM session_fts WHERE session_id = ?1",
                params![session_id.to_string()],
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())?;
        let idx = max.map(|m| (m + 1) as u32).unwrap_or(0);
        conn.execute(
            "INSERT INTO session_fts (session_id, msg_index, role, body) VALUES (?1, ?2, ?3, ?4)",
            params![session_id.to_string(), idx.to_string(), role, body],
        )
        .map_err(|e| format!("index insert: {e}"))?;
        Ok(idx)
    }

    /// Replace this session's rows from a JSONL file (boot catch-up).
    /// Skips worker/spawn. Returns the number of rows written.
    pub fn catch_up_file(&self, session_id: u64, jsonl: &Path) -> Result<u32, String> {
        if is_worker_session(session_id) || is_spawn_session(session_id) {
            return Ok(0);
        }
        let file = std::fs::File::open(jsonl).map_err(|e| format!("catch-up {}: {e}", jsonl.display()))?;
        let reader = std::io::BufReader::new(file);
        let mut rows: Vec<(u32, &'static str, String)> = Vec::new();
        let mut index = 0u32;
        for line in reader.lines() {
            let Ok(line) = line else { continue };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(msg) = serde_json::from_str::<Message>(line) else { continue };
            let (role, body) = searchable_text(&msg);
            rows.push((index, role, body));
            index += 1;
        }
        let mut conn = self.conn()?;
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        tx.execute(
            "DELETE FROM session_fts WHERE session_id = ?1",
            params![session_id.to_string()],
        )
        .map_err(|e| e.to_string())?;
        for (idx, role, body) in &rows {
            tx.execute(
                "INSERT INTO session_fts (session_id, msg_index, role, body) VALUES (?1, ?2, ?3, ?4)",
                params![session_id.to_string(), idx.to_string(), *role, body],
            )
            .map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(rows.len() as u32)
    }

    /// Walk `sessions/*.jsonl` and `sessions/archive/*.jsonl`. Best-effort; logs via `Err` join.
    pub fn catch_up_dir(&self, sessions_dir: &Path) -> Result<u32, String> {
        let mut n = 0u32;
        n += self.catch_up_glob(sessions_dir)?;
        n += self.catch_up_glob(&sessions_dir.join("archive"))?;
        Ok(n)
    }

    fn catch_up_glob(&self, dir: &Path) -> Result<u32, String> {
        let rd = match std::fs::read_dir(dir) {
            Ok(r) => r,
            Err(_) => return Ok(0),
        };
        let mut n = 0u32;
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(id) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.parse().ok())
            else {
                continue;
            };
            n += self.catch_up_file(id, &path)?;
        }
        Ok(n)
    }

    /// Drop all rows for a deleted session. Archive keeps rows.
    pub fn drop_session(&self, session_id: u64) -> Result<(), String> {
        let conn = self.conn()?;
        conn.execute(
            "DELETE FROM session_fts WHERE session_id = ?1",
            params![session_id.to_string()],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// BM25-filtered recency ring: MATCH the quoted AND query, order by msg_index
    /// descending, take `max`, return most-recent last (S1 display law).
    pub fn search(&self, session_id: u64, query: &str, max: usize) -> Result<Vec<TranscriptHit>, String> {
        let Some(match_q) = fts_match_query(query) else {
            return Ok(Vec::new());
        };
        if max == 0 {
            return Ok(Vec::new());
        }
        let terms = query_terms(query);
        let conn = self.conn()?;
        let mut stmt = conn
            .prepare(
                "SELECT msg_index, role, body FROM session_fts \
                 WHERE session_fts MATCH ?1 AND session_id = ?2 \
                 ORDER BY CAST(msg_index AS INTEGER) DESC LIMIT ?3",
            )
            .map_err(|e| e.to_string())?;
        let sid = session_id.to_string();
        let lim = max as i64;
        let rows = stmt
            .query_map(params![match_q, sid, lim], |row| {
                let idx: String = row.get(0)?;
                let role: String = row.get(1)?;
                let body: String = row.get(2)?;
                Ok((idx, role, body))
            })
            .map_err(|e| e.to_string())?;
        let mut hits = Vec::new();
        for row in rows {
            let (idx, role, body) = row.map_err(|e| e.to_string())?;
            let index = idx.parse().unwrap_or(0);
            hits.push(TranscriptHit {
                index,
                role: if role == "assistant" { "assistant" } else { "user" },
                snippet: clip_snippet(&body, &terms),
            });
        }
        hits.reverse();
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ContentBlock;
    use crate::transcript::fts_match_query;

    fn user(text: &str) -> Message {
        Message::User {
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }
    fn asst(text: &str) -> Message {
        Message::Assistant {
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    fn tmp_index(tag: &str) -> (SessionIndex, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "apexos-sidx-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let idx = SessionIndex::open(dir.join("index.sqlite")).expect("open");
        (idx, dir)
    }

    #[test]
    fn fts_quotes_operators_as_literals() {
        let q = fts_match_query("usb OR eject").unwrap();
        assert_eq!(q, "\"usb\" \"or\" \"eject\"");
        assert!(fts_match_query("   ").is_none());
    }

    #[test]
    fn insert_and_and_search_recency() {
        let (idx, dir) = tmp_index("and");
        idx.insert(7, &user("USB mount ok")).unwrap();
        idx.insert(7, &asst("try eject_media next")).unwrap();
        idx.insert(7, &user("USB eject failed on apex1")).unwrap();
        assert_eq!(idx.next_index(7).unwrap(), 3);
        let hits = idx.search(7, "usb eject", 20).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].index, 2);
        assert!(hits[0].snippet.to_lowercase().contains("eject"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn ring_is_most_recent_last() {
        let (idx, dir) = tmp_index("ring");
        for i in 0..8 {
            idx.insert(1, &user(&format!("token {i} needle"))).unwrap();
        }
        let hits = idx.search(1, "needle", 3).unwrap();
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].index, 5);
        assert_eq!(hits[2].index, 7);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn catch_up_replaces_and_skips_workers() {
        let (idx, dir) = tmp_index("cu");
        let jsonl = dir.join("3.jsonl");
        let mut body = String::new();
        body.push_str(&serde_json::to_string(&user("alpha needle")).unwrap());
        body.push('\n');
        body.push_str(&serde_json::to_string(&asst("beta")).unwrap());
        body.push('\n');
        std::fs::write(&jsonl, body).unwrap();
        assert_eq!(idx.catch_up_file(3, &jsonl).unwrap(), 2);
        assert!(idx.has_session(3));
        let hits = idx.search(3, "needle", 10).unwrap();
        assert_eq!(hits.len(), 1);

        let wid = crate::WORKER_SESSION_BASE + 1;
        assert_eq!(idx.catch_up_file(wid, &jsonl).unwrap(), 0);
        assert!(!idx.has_session(wid));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn drop_session_removes_rows() {
        let (idx, dir) = tmp_index("drop");
        idx.insert(4, &user("keep")).unwrap();
        idx.insert(5, &user("gone needle")).unwrap();
        idx.drop_session(5).unwrap();
        assert!(idx.has_session(4));
        assert!(!idx.has_session(5));
        assert!(idx.search(5, "needle", 10).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn catch_up_dir_walks_archive() {
        let (idx, dir) = tmp_index("arch");
        std::fs::create_dir_all(dir.join("archive")).unwrap();
        let live = dir.join("2.jsonl");
        let archived = dir.join("archive/9.jsonl");
        std::fs::write(&live, serde_json::to_string(&user("live needle")).unwrap() + "\n").unwrap();
        std::fs::write(&archived, serde_json::to_string(&user("archived needle")).unwrap() + "\n").unwrap();
        idx.catch_up_dir(&dir).unwrap();
        assert_eq!(idx.search(2, "needle", 5).unwrap().len(), 1);
        assert_eq!(idx.search(9, "needle", 5).unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }
}
