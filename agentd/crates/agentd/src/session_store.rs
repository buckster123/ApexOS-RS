use apexos_core::{Message, SessionId, SessionIndex};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::io::AsyncWriteExt;

pub struct SessionStore {
    pub sessions_dir: PathBuf,
    /// Sessions retired by delete/archive (SA-8). In-memory: a late
    /// `append`/`histories.insert` must not recreate a removed JSONL.
    tombstones: std::sync::Mutex<HashSet<u64>>,
    /// Derived FTS5 overlay (`docs/session-rag.md` S2). Best-effort: a failed
    /// index write never blocks the JSONL append.
    index: std::sync::Mutex<Option<SessionIndex>>,
}

impl SessionStore {
    pub fn new(log_dir: &Path) -> Self {
        Self {
            sessions_dir: log_dir.join("sessions"),
            tombstones: std::sync::Mutex::new(HashSet::new()),
            index: std::sync::Mutex::new(None),
        }
    }

    pub fn set_index(&self, index: SessionIndex) {
        *self.index.lock().unwrap_or_else(|e| e.into_inner()) = Some(index);
    }

    fn index(&self) -> Option<SessionIndex> {
        self.index.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Mark `id` retired. Subsequent [`append`] is a no-op; callers must also
    /// refuse in-memory history reinsert.
    pub fn tombstone(&self, id: SessionId) {
        self.tombstones.lock().unwrap_or_else(|e| e.into_inner()).insert(id.0);
    }

    pub fn is_tombstoned(&self, id: SessionId) -> bool {
        self.tombstones.lock().unwrap_or_else(|e| e.into_inner()).contains(&id.0)
    }

    /// Delete the live JSONL. Missing file → false (same as today's handler).
    pub async fn remove_file(&self, id: SessionId) -> bool {
        let ok = fs::remove_file(self.session_path(id)).await.is_ok();
        if ok {
            if let Some(idx) = self.index() {
                let _ = idx.drop_session(id.0);
            }
        }
        ok
    }

    /// Move the live JSONL into `sessions/archive/`. Recoverable.
    pub async fn archive_file(&self, id: SessionId) -> Result<bool, String> {
        let archive_dir = self.sessions_dir.join("archive");
        fs::create_dir_all(&archive_dir).await.map_err(|e| format!("archive dir: {e}"))?;
        Ok(fs::rename(
            self.session_path(id),
            archive_dir.join(format!("{}.jsonl", id.0)),
        ).await.is_ok())
    }

    pub async fn init(&self) -> std::io::Result<()> {
        fs::create_dir_all(&self.sessions_dir).await
    }

    fn session_path(&self, id: SessionId) -> PathBuf {
        self.sessions_dir.join(format!("{}.jsonl", id.0))
    }

    /// Whether a session has JSONL truth on disk. Cheap sync stat — used by the
    /// parked-worker guard on the prompt path (worker-range ids only, rare).
    pub fn exists(&self, id: SessionId) -> bool {
        let p = self.session_path(id);
        p.exists() || apexos_core::session_gzip::gz_path(&p).exists()
    }

    /// Append one message to the session's JSONL file. Fire-and-forget safe.
    /// Ephemeral spawn sessions are not persisted. A tombstoned session is a
    /// no-op — `create(true)` here is what resurrected deleted JSONLs (SA-8).
    pub async fn append(&self, session_id: SessionId, msg: &Message) {
        if apexos_core::is_spawn_session(session_id.0) { return; }
        if self.is_tombstoned(session_id) { return; }
        let line = match serde_json::to_string(msg) {
            Ok(s) => s + "\n",
            Err(_) => return,
        };
        if let Ok(mut file) = fs::OpenOptions::new()
            .create(true).append(true).open(self.session_path(session_id)).await
        {
            if file.write_all(line.as_bytes()).await.is_ok() {
                if let Some(idx) = self.index() {
                    let msg = msg.clone();
                    let sid = session_id.0;
                    let _ = tokio::task::spawn_blocking(move || idx.insert(sid, &msg)).await;
                }
            }
        }
    }

    /// Load ONE session's history from disk — the parked-worker revive edge
    /// (Fabrica W1b). Same per-file body as `load_all` (parse line-wise, then
    /// `repair_history` for API validity — honest markers, file untouched),
    /// but on demand: a Parked worker's JSONL hydrates only when a send
    /// arrives, never at boot. Returns None on a missing/unreadable file or
    /// an empty history.
    pub async fn load_one(&self, id: SessionId) -> Option<Vec<Message>> {
        let path = self.session_path(id);
        let text = match fs::read_to_string(&path).await {
            Ok(t) => t,
            Err(_) => {
                let gz = apexos_core::session_gzip::gz_path(&path);
                tokio::task::spawn_blocking(move || apexos_core::session_gzip::read_gz_to_string(&gz).ok())
                    .await
                    .ok()
                    .flatten()?
            }
        };
        let mut messages: Vec<Message> = text.lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        if apexos_core::history::repair_history(&mut messages) {
            eprintln!(
                "[session] repaired {} on revive ({} messages) — restored tool pairing/ordering",
                id.0, messages.len()
            );
        }
        if messages.is_empty() { None } else { Some(messages) }
    }

    /// Load all persisted sessions into memory on daemon startup.
    pub async fn load_all(&self) -> HashMap<SessionId, Vec<Message>> {
        let mut result = HashMap::new();
        let mut rd = match fs::read_dir(&self.sessions_dir).await {
            Ok(r) => r,
            Err(_) => return result,
        };
        while let Ok(Some(entry)) = rd.next_entry().await {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") { continue; }
            let id: u64 = match path.file_stem().and_then(|s| s.to_str())
                .and_then(|s| s.parse().ok()) { Some(n) => n, None => continue };
            // Worker sessions (Fabrica W tier) are NOT hydrated at boot: Parked
            // means not memory-resident — on a Pi-class node, eagerly loading
            // every parked worker's history would silently repeal that law. The
            // file stays on disk as truth; W1b's revive-on-send loads it on
            // demand (through repair_history, same as here).
            if apexos_core::is_worker_session(id) { continue; }

            let text = match fs::read_to_string(&path).await { Ok(t) => t, Err(_) => continue };
            let mut messages: Vec<Message> = text.lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect();
            // Self-heal: a file written under the old racing persist path (or
            // truncated by a crash mid-batch) can reload in an order the provider
            // API rejects — which permanently wedges the session (every turn
            // 400s before the model runs). Repair restores API validity with
            // honest markers; the on-disk file stays as-written (append-only
            // doctrine — replay keeps the original record).
            if apexos_core::history::repair_history(&mut messages) {
                eprintln!(
                    "[session] repaired {} ({} messages) — restored tool pairing/ordering from a corrupted file",
                    id, messages.len()
                );
            }
            if !messages.is_empty() {
                eprintln!("[session] restored {} ({} messages)", id, messages.len());
                result.insert(SessionId(id), messages);
            }
        }
        eprintln!("[session] loaded {} session(s) from disk", result.len());
        result
    }

    /// Gzip idle `.jsonl` files (not loaded, not root, not worker). Index stays.
    pub fn gzip_idle(&self, loaded: &std::collections::HashSet<u64>, ttl: std::time::Duration) -> Result<u32, String> {
        apexos_core::session_gzip::gzip_idle_dir(&self.sessions_dir, loaded, ttl)
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use apexos_core::ContentBlock;

    fn tmp_store(tag: &str) -> SessionStore {
        let dir = std::env::temp_dir().join(format!("apexos-sstore-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sessions")).unwrap();
        SessionStore::new(&dir)
    }

    fn user_msg(text: &str) -> Message {
        Message::User { content: vec![ContentBlock::Text { text: text.into() }] }
    }

    #[tokio::test]
    async fn load_one_round_trips_a_worker_session() {
        let store = tmp_store("one");
        let wid = SessionId(apexos_core::WORKER_SESSION_BASE + 3);
        store.append(wid, &user_msg("the task")).await;
        store.append(wid, &user_msg("more context")).await;
        let loaded = store.load_one(wid).await.expect("history on disk");
        assert_eq!(loaded.len(), 2);
        assert!(store.load_one(SessionId(apexos_core::WORKER_SESSION_BASE + 99)).await.is_none());
        let _ = std::fs::remove_dir_all(store.sessions_dir.parent().unwrap());
    }

    #[tokio::test]
    async fn load_all_skips_worker_range_but_load_one_serves_it() {
        // The residency law: boot hydration ignores parked workers; the revive
        // edge reads the same file on demand.
        let store = tmp_store("skip");
        let normal = SessionId(7);
        let worker = SessionId(apexos_core::WORKER_SESSION_BASE);
        store.append(normal, &user_msg("chat")).await;
        store.append(worker, &user_msg("task")).await;
        let all = store.load_all().await;
        assert!(all.contains_key(&normal));
        assert!(!all.contains_key(&worker), "worker JSONL must not hydrate at boot");
        assert!(store.load_one(worker).await.is_some(), "revive edge must still read it");
        let _ = std::fs::remove_dir_all(store.sessions_dir.parent().unwrap());
    }

    #[tokio::test]
    async fn tombstone_blocks_append_from_recreating_the_file() {
        let store = tmp_store("tomb");
        let sid = SessionId(9);
        store.append(sid, &user_msg("before")).await;
        assert!(store.exists(sid));
        store.tombstone(sid);
        assert!(store.remove_file(sid).await);
        assert!(!store.exists(sid));
        store.append(sid, &user_msg("late persist after delete")).await;
        assert!(!store.exists(sid), "tombstoned append must not recreate the JSONL");
        let _ = std::fs::remove_dir_all(store.sessions_dir.parent().unwrap());
    }

    #[tokio::test]
    async fn archive_then_tombstone_does_not_recreate_live_file() {
        let store = tmp_store("arch");
        let sid = SessionId(3);
        store.append(sid, &user_msg("chat")).await;
        store.tombstone(sid);
        assert!(store.archive_file(sid).await.unwrap());
        assert!(!store.exists(sid));
        store.append(sid, &user_msg("late")).await;
        assert!(!store.exists(sid));
        assert!(store.sessions_dir.join("archive").join("3.jsonl").exists());
        let _ = std::fs::remove_dir_all(store.sessions_dir.parent().unwrap());
    }

    #[tokio::test]
    async fn append_feeds_fts5_and_delete_drops_it() {
        let store = tmp_store("fts");
        let idx = apexos_core::SessionIndex::open(store.sessions_dir.join("index.sqlite")).unwrap();
        store.set_index(idx.clone());
        let sid = SessionId(2);
        store.append(sid, &user_msg("hello needle")).await;
        store.append(sid, &user_msg("unrelated")).await;
        let hits = idx.search(2, "needle", 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].index, 0);
        store.tombstone(sid);
        assert!(store.remove_file(sid).await);
        assert!(!idx.has_session(2));
        let _ = std::fs::remove_dir_all(store.sessions_dir.parent().unwrap());
    }

    #[test]
    fn gzip_idle_skips_loaded_and_root() {
        let store = tmp_store("gz");
        let root = SessionId(0);
        let idle = SessionId(4);
        // sync write via std so mtime is in the past after we sleep? use gzip_idle with 0 ttl = no-op
        std::fs::write(store.sessions_dir.join("0.jsonl"), "{}\n").unwrap();
        std::fs::write(store.sessions_dir.join("4.jsonl"), "{}\n").unwrap();
        let mut loaded = std::collections::HashSet::new();
        loaded.insert(0);
        assert_eq!(store.gzip_idle(&loaded, std::time::Duration::ZERO).unwrap(), 0);
        assert_eq!(
            store.gzip_idle(&loaded, std::time::Duration::from_secs(0)).unwrap(),
            0
        );
        // age is ~0 so even a 1ns ttl may not fire; just assert root/live skip via should_gzip
        assert!(!apexos_core::session_gzip::should_gzip(
            0,
            &loaded,
            std::time::Duration::from_secs(99),
            std::time::Duration::from_secs(1)
        ));
        let _ = std::fs::remove_dir_all(store.sessions_dir.parent().unwrap());
    }
}
