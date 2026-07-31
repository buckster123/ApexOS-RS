use apexos_core::{Message, SessionId};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::io::AsyncWriteExt;

pub struct SessionStore {
    pub sessions_dir: PathBuf,
}

impl SessionStore {
    pub fn new(log_dir: &Path) -> Self {
        Self { sessions_dir: log_dir.join("sessions") }
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
        self.session_path(id).exists()
    }

    /// Append one message to the session's JSONL file. Fire-and-forget safe.
    /// Ephemeral spawn sessions are not persisted.
    pub async fn append(&self, session_id: SessionId, msg: &Message) {
        if apexos_core::is_spawn_session(session_id.0) { return; }
        let line = match serde_json::to_string(msg) {
            Ok(s) => s + "\n",
            Err(_) => return,
        };
        if let Ok(mut file) = fs::OpenOptions::new()
            .create(true).append(true).open(self.session_path(session_id)).await
        {
            let _ = file.write_all(line.as_bytes()).await;
        }
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

}
