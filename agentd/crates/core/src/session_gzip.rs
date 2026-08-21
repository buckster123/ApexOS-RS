//! Idle JSONL gzip (`docs/session-rag.md` S3).
//!
//! Compresses on-disk transcripts that are not in the live window. Root
//! session 0 and worker/spawn ids are never candidates. JSONL stays the
//! uncompressed live file while a session is loaded.

use crate::{is_spawn_session, is_worker_session};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Whether this session file may be gzipped: not root, not worker/spawn,
/// not currently loaded, and age has reached the TTL. `ttl == 0` disables.
pub fn should_gzip(id: u64, loaded: &HashSet<u64>, age: Duration, ttl: Duration) -> bool {
    if ttl.is_zero() {
        return false;
    }
    if id == 0 {
        return false;
    }
    if is_worker_session(id) || is_spawn_session(id) {
        return false;
    }
    if loaded.contains(&id) {
        return false;
    }
    age >= ttl
}

/// Parse `12.jsonl` or `12.jsonl.gz`.
pub fn session_id_from_filename(name: &str) -> Option<u64> {
    name.strip_suffix(".jsonl.gz")
        .or_else(|| name.strip_suffix(".jsonl"))?
        .parse()
        .ok()
}

/// `foo.jsonl` → `foo.jsonl.gz`.
pub fn gz_path(jsonl: &Path) -> PathBuf {
    let mut p = jsonl.as_os_str().to_os_string();
    p.push(".gz");
    PathBuf::from(p)
}

/// Gzip `jsonl` to `jsonl.gz` (tmp+rename), then remove the uncompressed file.
/// No-op if the gzip already exists. Returns the gzip path.
pub fn gzip_jsonl(jsonl: &Path) -> Result<PathBuf, String> {
    if jsonl.extension().and_then(|e| e.to_str()) != Some("jsonl") {
        return Err("not a .jsonl file".into());
    }
    let dest = gz_path(jsonl);
    if dest.exists() {
        let _ = std::fs::remove_file(jsonl);
        return Ok(dest);
    }
    let tmp = dest.with_extension("gz.tmp");
    {
        let src = File::open(jsonl).map_err(|e| format!("read {}: {e}", jsonl.display()))?;
        let mut enc = GzEncoder::new(
            File::create(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?,
            Compression::default(),
        );
        std::io::copy(&mut std::io::BufReader::new(src), &mut enc)
            .and_then(|_| enc.finish())
            .map_err(|e| format!("gzip: {e}"))?;
    }
    std::fs::rename(&tmp, &dest).map_err(|e| format!("rename gzip: {e}"))?;
    let _ = std::fs::remove_file(jsonl);
    Ok(dest)
}

/// Open a live JSONL or its `.gz` as a BufRead. Prefer uncompressed.
pub fn open_jsonl_or_gz(jsonl: &Path) -> std::io::Result<Box<dyn BufRead + Send>> {
    if jsonl.exists() {
        return Ok(Box::new(BufReader::new(File::open(jsonl)?)));
    }
    let gz = gz_path(jsonl);
    let f = File::open(&gz)?;
    Ok(Box::new(BufReader::new(GzDecoder::new(f))))
}

/// Read a gzipped JSONL to a string (resume/load_one).
pub fn read_gz_to_string(gz: &Path) -> std::io::Result<String> {
    let mut s = String::new();
    GzDecoder::new(File::open(gz)?).read_to_string(&mut s)?;
    Ok(s)
}

/// Walk `dir` and `dir/archive` for idle `.jsonl` files and gzip them.
/// Returns how many files compressed.
pub fn gzip_idle_dir(
    sessions_dir: &Path,
    loaded: &HashSet<u64>,
    ttl: Duration,
) -> Result<u32, String> {
    if ttl.is_zero() {
        return Ok(0);
    }
    let now = std::time::SystemTime::now();
    let mut n = 0u32;
    n += gzip_idle_one_dir(sessions_dir, loaded, ttl, now)?;
    n += gzip_idle_one_dir(&sessions_dir.join("archive"), loaded, ttl, now)?;
    Ok(n)
}

fn gzip_idle_one_dir(
    dir: &Path,
    loaded: &HashSet<u64>,
    ttl: Duration,
    now: std::time::SystemTime,
) -> Result<u32, String> {
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
        let age = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| now.duration_since(t).ok())
            .unwrap_or(Duration::ZERO);
        if !should_gzip(id, loaded, age, ttl) {
            continue;
        }
        gzip_jsonl(&path)?;
        n += 1;
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn gate_skips_root_loaded_worker_and_zero_ttl() {
        let mut loaded = HashSet::new();
        loaded.insert(3);
        let ttl = Duration::from_secs(86400);
        let old = Duration::from_secs(86400 * 40);
        assert!(!should_gzip(0, &loaded, old, ttl));
        assert!(!should_gzip(3, &loaded, old, ttl));
        assert!(!should_gzip(crate::WORKER_SESSION_BASE, &HashSet::new(), old, ttl));
        assert!(!should_gzip(5, &HashSet::new(), old, Duration::ZERO));
        assert!(!should_gzip(5, &HashSet::new(), Duration::from_secs(10), ttl));
        assert!(should_gzip(5, &HashSet::new(), old, ttl));
        assert_eq!(session_id_from_filename("12.jsonl"), Some(12));
        assert_eq!(session_id_from_filename("12.jsonl.gz"), Some(12));
        assert_eq!(session_id_from_filename("index.sqlite"), None);
    }

    #[test]
    fn gzip_round_trip_and_open() {
        let dir = std::env::temp_dir().join(format!(
            "apexos-sgz-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let jsonl = dir.join("5.jsonl");
        std::fs::write(&jsonl, "{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}\n").unwrap();
        let gz = gzip_jsonl(&jsonl).unwrap();
        assert!(gz.exists());
        assert!(!jsonl.exists());
        let text = read_gz_to_string(&gz).unwrap();
        assert!(text.contains("hi"));
        let mut r = open_jsonl_or_gz(&jsonl).unwrap();
        let mut s = String::new();
        r.read_to_string(&mut s).unwrap();
        assert!(s.contains("hi"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
