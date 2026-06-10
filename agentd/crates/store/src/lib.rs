use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use apexos_core::Event;
use chrono::NaiveDate;
use tokio::sync::broadcast;

/// Subscribe to the event bus and write every event as a JSONL line.
///
/// Files are named `events-YYYY-MM-DD.jsonl` under `log_dir`.
/// Rolls to a new file automatically at date change.
/// Flushes the kernel buffer after every event — durable without fsync overhead.
pub async fn run_log_writer(log_dir: PathBuf, mut rx: broadcast::Receiver<Event>) {
    let mut current_date = today();
    let mut writer = open_writer(&log_dir, current_date);

    loop {
        match rx.recv().await {
            Ok(event) => {
                // Roll file at midnight.
                let now = today();
                if now != current_date {
                    current_date = now;
                    writer = open_writer(&log_dir, now);
                }

                if let Some(ref mut w) = writer {
                    match serde_json::to_string(&event) {
                        Ok(line) => {
                            let _ = writeln!(w, "{line}");
                            let _ = w.flush();
                        }
                        Err(e) => eprintln!("[store] serialize error: {e}"),
                    }
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                eprintln!("[store] lagged — dropped {n} event(s)");
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

fn today() -> NaiveDate {
    chrono::Local::now().date_naive()
}

fn open_writer(log_dir: &Path, date: NaiveDate) -> Option<BufWriter<File>> {
    if let Err(e) = fs::create_dir_all(log_dir) {
        eprintln!("[store] cannot create log dir {}: {e}", log_dir.display());
        return None;
    }
    let path = log_dir.join(format!("events-{}.jsonl", date.format("%Y-%m-%d")));
    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(f) => {
            eprintln!("[store] logging → {}", path.display());
            Some(BufWriter::new(f))
        }
        Err(e) => {
            eprintln!("[store] open {}: {e}", path.display());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apexos_core::SessionId;
    use tokio::sync::broadcast;

    #[tokio::test]
    async fn writes_events_to_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = broadcast::channel(16);

        let log_dir = dir.path().to_path_buf();
        let handle = tokio::spawn(run_log_writer(log_dir.clone(), rx));

        tx.send(Event::UserPrompt { session: SessionId(1), text: "hello".into() }).unwrap();
        tx.send(Event::AgentText  { session: SessionId(1), delta: "hi".into() }).unwrap();
        tx.send(Event::TurnComplete { session: SessionId(1) }).unwrap();

        // Give the writer task a moment to flush.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(tx);
        let _ = handle.await;

        // Exactly one file should exist.
        let files: Vec<_> = fs::read_dir(&log_dir).unwrap().collect();
        assert_eq!(files.len(), 1);

        let content = fs::read_to_string(files[0].as_ref().unwrap().path()).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 3);

        // Each line is valid JSON with a "type" field.
        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert!(v.get("type").is_some(), "missing type field in: {line}");
        }

        // First line is the user_prompt, last is turn_complete.
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["type"], "user_prompt");
        let last: serde_json::Value  = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(last["type"], "turn_complete");
    }

    #[tokio::test]
    async fn survives_lagged_receiver() {
        let dir = tempfile::tempdir().unwrap();
        // Tiny capacity to force a lag.
        let (tx, rx) = broadcast::channel(2);

        let handle = tokio::spawn(run_log_writer(dir.path().to_path_buf(), rx));

        // Flood the channel — some will be dropped due to lag.
        for i in 0..10 {
            let _ = tx.send(Event::UserPrompt {
                session: SessionId(i),
                text: format!("msg {i}"),
            });
        }

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(tx);
        let _ = handle.await;

        // Writer should have survived and written whatever it received.
        let files: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(files.len(), 1, "log file should exist even after lag");
    }
}
