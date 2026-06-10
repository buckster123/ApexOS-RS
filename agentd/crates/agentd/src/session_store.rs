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

    /// Append one message to the session's JSONL file. Fire-and-forget safe.
    /// Sub-agent sessions (IDs in the top half of u64) are not persisted.
    pub async fn append(&self, session_id: SessionId, msg: &Message) {
        if session_id.0 >= (1u64 << 63) { return; }
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

            let text = match fs::read_to_string(&path).await { Ok(t) => t, Err(_) => continue };
            let messages: Vec<Message> = text.lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect();
            if !messages.is_empty() {
                eprintln!("[session] restored {} ({} messages)", id, messages.len());
                result.insert(SessionId(id), messages);
            }
        }
        eprintln!("[session] loaded {} session(s) from disk", result.len());
        result
    }

}
