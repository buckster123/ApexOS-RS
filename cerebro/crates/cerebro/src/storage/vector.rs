use std::sync::Arc;

use anyhow::Result;
use rusqlite::params;
use tokio::sync::Mutex;

use crate::{storage::SqliteStore, types::MemoryId};

/// Vector similarity search via sqlite-vec (vec0) + FTS5 keyword fallback.
///
/// sqlite-vec is registered as an auto-extension in SqliteStore::open.
/// If that fails (extension missing on system), vec_available stays false and
/// every search falls through to the FTS5 path automatically.
///
/// Embeddings: fastembed BAAI/bge-small-en-v1.5 — 384-dim f32.
/// fastembed is initialized lazily; if the model cannot be loaded (no net on first
/// run, ONNX runtime error), embedder stays None and FTS5 is the sole search path.
pub struct VectorStore {
    conn:              Arc<Mutex<rusqlite::Connection>>,
    embedder:          Option<Arc<fastembed::TextEmbedding>>,
    pub vec_available: bool,
}

impl VectorStore {
    pub async fn new(sqlite: &SqliteStore, embed_model: &str) -> Result<Self> {
        let conn         = sqlite.shared_conn();
        let vec_available = sqlite.vec_available;

        // Initialize fastembed unless embed_model is empty (skipped in tests).
        let embedder = if embed_model.is_empty() {
            None
        } else {
            let model_str = embed_model.to_string();
            match tokio::task::spawn_blocking(move || {
                init_fastembed(&model_str)
            }).await {
                Ok(Ok(model)) => {
                    tracing::info!("fastembed model loaded: {embed_model}");
                    Some(Arc::new(model))
                }
                Ok(Err(e)) => {
                    tracing::warn!("fastembed init failed ({e}) — embedding disabled, FTS5 only");
                    None
                }
                Err(e) => {
                    tracing::warn!("fastembed spawn_blocking failed ({e}) — FTS5 only");
                    None
                }
            }
        };

        Ok(Self { conn, embedder, vec_available })
    }

    /// Embed content and persist the vector.
    ///
    /// Stores the embedding as a BLOB in `memories.embedding` and, if vec0 is
    /// available, inserts into `memory_vectors` keyed by the memory's integer rowid.
    /// Returns the embedding or an empty vec if fastembed is not available.
    pub async fn embed_and_store(&self, memory_id: &MemoryId, content: &str) -> Result<Vec<f32>> {
        let Some(ref embedder) = self.embedder else {
            return Ok(vec![]);
        };

        let content_owned = content.to_string();
        let embedder_arc  = embedder.clone();
        let embedding: Vec<f32> = tokio::task::spawn_blocking(move || {
            embedder_arc.embed(vec![content_owned], None)
                .map(|mut v| v.remove(0))
        }).await??;

        let blob = vec_to_blob(&embedding);
        let conn = self.conn.lock().await;

        // Persist in memories.embedding column
        conn.execute(
            "UPDATE memories SET embedding = ?1 WHERE id = ?2",
            params![blob, memory_id.0],
        )?;

        // Upsert into the vec0 index, keyed by the memories table's integer rowid
        // (vec0 rejects INSERT OR REPLACE — see upsert_memory_vector).
        if self.vec_available {
            upsert_memory_vector(&conn, memory_id, &blob)?;
        }

        Ok(embedding)
    }

    /// Store a pre-computed embedding (for testing / offline ingestion).
    pub async fn store_raw_embedding(&self, memory_id: &MemoryId, embedding: &[f32]) -> Result<()> {
        let blob = vec_to_blob(embedding);
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE memories SET embedding = ?1 WHERE id = ?2",
            params![blob, memory_id.0],
        )?;
        if self.vec_available {
            upsert_memory_vector(&conn, memory_id, &blob)?;
        }
        Ok(())
    }

    // ── CLIP visual recall (search_vision) ──────────────────────────────────
    // A separate 512-dim CLIP image-vector space (distinct from the 384-dim bge
    // text store above). Stored in the plain `vision_embeddings` row table; recall
    // is brute-force cosine in Rust (image counts are modest). The embedding itself
    // is computed by the caller (`cerebro::vision` CLIP towers) — this layer only
    // persists + ranks, so the store stays embedder-agnostic.

    /// Persist (or replace) a memory's CLIP image embedding + source path.
    pub async fn store_vision_embedding(
        &self,
        memory_id:  &MemoryId,
        embedding:  &[f32],
        image_path: Option<&str>,
    ) -> Result<()> {
        let blob = vec_to_blob(embedding);
        let now  = chrono::Utc::now().to_rfc3339();
        let path = image_path.map(|s| s.to_string());
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO vision_embeddings (memory_id, embedding, image_path, created_at) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(memory_id) DO UPDATE SET embedding=?2, image_path=?3, created_at=?4",
            params![memory_id.0, blob, path, now],
        )?;
        Ok(())
    }

    /// Brute-force cosine search over stored CLIP image vectors. Returns
    /// `(memory_id, similarity, image_path)` for the top `k`, unscoped — the caller
    /// (Cortex) fetches the memory nodes and applies visibility scope.
    pub async fn vision_search(
        &self,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<(MemoryId, f32, Option<String>)>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare("SELECT memory_id, embedding, image_path FROM vision_embeddings")?;
        let rows = stmt.query_map([], |row| {
            let id:   String      = row.get(0)?;
            let blob: Vec<u8>     = row.get(1)?;
            let path: Option<String> = row.get(2)?;
            Ok((id, blob, path))
        })?;
        let mut scored: Vec<(MemoryId, f32, Option<String>)> = Vec::new();
        for r in rows {
            let (id, blob, path) = r?;
            let sim = cosine(query, &blob_to_vec(&blob));
            scored.push((MemoryId(id), sim, path));
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(k);
        Ok(scored)
    }

    /// Return top-k most similar memory IDs.
    ///
    /// When vec0 is available and the embedder is initialized, runs cosine-distance
    /// KNN in sqlite-vec (over-fetches 5× then filters by scope).
    /// Falls back to FTS5 BM25 keyword search otherwise.
    pub async fn search(
        &self,
        query: &str,
        k: usize,
        scope_sql:    &str,
        scope_params: &[String],
    ) -> Result<Vec<(MemoryId, f32)>> {
        if self.vec_available {
            if let Some(ref embedder) = self.embedder {
                let results = self.vec_search(query, k, scope_sql, scope_params, embedder.clone()).await?;
                if !results.is_empty() {
                    return Ok(results);
                }
                // vec0 returned nothing (e.g. no embeddings stored yet) — fall through to FTS5
            }
        }
        self.fts5_search(query, k, scope_sql, scope_params).await
    }

    pub fn is_vec_available(&self) -> bool { self.vec_available }
    pub fn is_embedder_loaded(&self) -> bool { self.embedder.is_some() }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    async fn vec_search(
        &self,
        query:        &str,
        k:            usize,
        scope_sql:    &str,
        scope_params: &[String],
        embedder:     Arc<fastembed::TextEmbedding>,
    ) -> Result<Vec<(MemoryId, f32)>> {
        let q = query.to_string();
        let query_vec: Vec<f32> = tokio::task::spawn_blocking(move || {
            embedder.embed(vec![q], None).map(|mut v| v.remove(0))
        }).await??;

        let query_blob  = vec_to_blob(&query_vec);
        let candidates  = (k * 5) as i64;
        let k_i64       = k as i64;

        // Over-fetch from vec0, then apply scope + deleted filter in outer query.
        // vec0 distance is cosine distance [0, 1]; similarity = 1 - distance.
        let sql = format!(
            "SELECT m.id, 1.0 - v.distance \
             FROM (SELECT rowid, distance FROM memory_vectors \
                   WHERE embedding MATCH ? ORDER BY distance LIMIT ?) v \
             JOIN memories m ON m.rowid = v.rowid \
             WHERE {scope_sql} AND m.deleted_at IS NULL \
             ORDER BY v.distance LIMIT ?"
        );

        let conn = self.conn.lock().await;
        let mut dyn_params: Vec<&dyn rusqlite::ToSql> = vec![&query_blob, &candidates];
        for s in scope_params { dyn_params.push(s); }
        dyn_params.push(&k_i64);

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(dyn_params.as_slice(), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)? as f32))
        })?;

        let mut results = Vec::new();
        for row in rows {
            let (id, score) = row?;
            results.push((MemoryId(id), score.clamp(0.0, 1.0)));
        }
        Ok(results)
    }

    async fn fts5_search(
        &self,
        query:        &str,
        k:            usize,
        scope_sql:    &str,
        scope_params: &[String],
    ) -> Result<Vec<(MemoryId, f32)>> {
        // Quote each token individually — FTS5 treats "word1" "word2" as implicit
        // AND with no positional constraint, which matches the original semantics
        // while safely neutralizing any FTS5 operators in the raw query string.
        let safe_query: String = query
            .split_whitespace()
            .map(|w| format!("\"{}\"", w.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" ");
        let safe_query = if safe_query.is_empty() { return Ok(Vec::new()); } else { safe_query };
        let k_i64 = k as i64;

        // CB-014: surface FTS5's bm25() rank instead of a flat 0.5, so keyword
        // relevance actually discriminates results when this score feeds
        // recall_score (vector_sim, the largest weight) and the spreading seed.
        // bm25() is more negative for a better match; map it monotonically into
        // (0,1] via a logistic on the negated score so a better match scores higher.
        let sql = format!(
            "SELECT m.id, bm25(memories_fts) FROM memories_fts \
             JOIN memories m ON m.id = memories_fts.id \
             WHERE memories_fts MATCH ? AND {scope_sql} AND m.deleted_at IS NULL \
             ORDER BY rank LIMIT ?"
        );

        let conn = self.conn.lock().await;
        let mut all_params: Vec<&dyn rusqlite::ToSql> = vec![&safe_query];
        for s in scope_params { all_params.push(s); }
        all_params.push(&k_i64);

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(all_params.as_slice(), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
        })?;

        let mut results = Vec::new();
        for row in rows {
            let (id, bm25) = row?;
            // bm25 < 0 → relevance > 0.5 (good match); bm25 → 0 → relevance → 0.5.
            let relevance = (1.0 / (1.0 + (bm25 as f32).exp())).clamp(0.0, 1.0);
            results.push((MemoryId(id), relevance));
        }
        Ok(results)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Serialize a f32 slice as little-endian bytes — the format sqlite-vec expects.
pub fn vec_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Deserialize little-endian bytes back to f32 slice.
pub fn blob_to_vec(blob: &[u8]) -> Vec<f32> {
    blob.chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect()
}

/// Cosine similarity in [-1, 1]; 0 for a zero/empty or length-mismatched vector.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na  += a[i] * a[i];
        nb  += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Upsert a memory's row in the vec0 index, keyed by its integer rowid.
///
/// sqlite-vec's vec0 virtual table does **not** honor `INSERT OR REPLACE` — it
/// raises "UNIQUE constraint failed on memory_vectors primary key" when the rowid
/// already holds a vector (i.e. re-embedding an existing memory via update_memory).
/// So delete the stale row then insert — the same convention `insert_memory` /
/// `purge_memory` already use (CB-005). No-op if the memory row is gone (the
/// SELECT yields no rowid). Caller holds the connection lock, so the pair is
/// effectively atomic against other writers.
fn upsert_memory_vector(
    conn:      &rusqlite::Connection,
    memory_id: &MemoryId,
    blob:      &[u8],
) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM memory_vectors WHERE rowid IN \
         (SELECT rowid FROM memories WHERE id = ?1)",
        params![memory_id.0],
    )?;
    conn.execute(
        "INSERT INTO memory_vectors(rowid, embedding) \
         SELECT rowid, ?1 FROM memories WHERE id = ?2",
        params![blob, memory_id.0],
    )?;
    Ok(())
}

fn init_fastembed(model_name: &str) -> Result<fastembed::TextEmbedding> {
    use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
    // Only bge-small is wired through (384-dim — what the vector store assumes). An
    // unrecognized model name must NOT disable embeddings: that degraded cerebro to
    // FTS5-only *silently* and cost a long debugging hunt. Fall back to bge-small and
    // warn loudly instead — memory search keeps working.
    let model = match model_name {
        "BAAI/bge-small-en-v1.5" => EmbeddingModel::BGESmallENV15,
        other => {
            tracing::warn!(
                "unsupported embed model '{other}' — falling back to BAAI/bge-small-en-v1.5 \
                 (embeddings stay enabled; set CEREBRO_EMBED_MODEL=BAAI/bge-small-en-v1.5 to silence)"
            );
            EmbeddingModel::BGESmallENV15
        }
    };
    TextEmbedding::try_new(InitOptions::new(model))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::MemoryNode;
    use crate::types::MemoryType;

    async fn fresh_store() -> (SqliteStore, VectorStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let sqlite = SqliteStore::open(&dir.path().join("t.db")).await.unwrap();
        let vector = VectorStore::new(&sqlite, "").await.unwrap(); // "" → no embedder
        (sqlite, vector, dir)
    }

    /// Regression (reported by APEX, 2026-06-20): re-embedding an existing memory
    /// must not fail. sqlite-vec's vec0 table rejects `INSERT OR REPLACE` (raises
    /// "UNIQUE constraint failed on memory_vectors primary key" on an existing
    /// rowid), which is exactly what `update_memory`'s re-embed hit. `store_raw_
    /// embedding` shares the vec-upsert path with `embed_and_store`, so this covers
    /// both without needing the ONNX model.
    #[tokio::test]
    async fn reembedding_an_existing_memory_succeeds() {
        let (sqlite, vector, _dir) = fresh_store().await;
        assert!(vector.vec_available, "vec0 must be available for this regression test");

        let node = MemoryNode::new("a thermal frame caption", MemoryType::Episodic);
        sqlite.insert_memory(&node).await.unwrap();

        vector.store_raw_embedding(&node.id, &vec![0.1f32; 384]).await
            .expect("first embed");
        // The bug: this second write hit `UNIQUE constraint failed on memory_vectors`.
        vector.store_raw_embedding(&node.id, &vec![0.9f32; 384]).await
            .expect("re-embed of the same memory must succeed");
    }

    #[test]
    fn cosine_basics() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6, "identical → 1");
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6, "orthogonal → 0");
        assert_eq!(cosine(&[1.0], &[1.0, 2.0]), 0.0, "length mismatch → 0");
        assert_eq!(cosine(&[], &[]), 0.0, "empty → 0");
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0, "zero vector → 0");
    }

    // The CLIP visual-recall store/search ranking, exercised with fake vectors so it
    // needs no ONNX model. Covers the brute-force cosine ordering, image_path
    // round-trip, and the ON CONFLICT replace.
    #[tokio::test]
    async fn vision_store_and_search_ranks_by_cosine() {
        let (sqlite, vector, _dir) = fresh_store().await;
        let a = MemoryNode::new("a red bicycle", MemoryType::Episodic);
        let b = MemoryNode::new("a blue car", MemoryType::Episodic);
        sqlite.insert_memory(&a).await.unwrap();
        sqlite.insert_memory(&b).await.unwrap();

        vector.store_vision_embedding(&a.id, &[1.0, 0.0, 0.0, 0.0], Some("imgs/a.png")).await.unwrap();
        vector.store_vision_embedding(&b.id, &[0.0, 1.0, 0.0, 0.0], None).await.unwrap();

        // A query closest to `a`.
        let hits = vector.vision_search(&[0.9, 0.1, 0.0, 0.0], 5).await.unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].0, a.id, "nearest vector ranks first");
        assert_eq!(hits[0].2.as_deref(), Some("imgs/a.png"), "image_path returned");
        assert!(hits[0].1 > hits[1].1, "cosine descending");

        // ON CONFLICT replaces the vector + path in place (no error).
        vector.store_vision_embedding(&a.id, &[0.0, 0.0, 1.0, 0.0], Some("imgs/a2.png")).await.unwrap();
        let hits2 = vector.vision_search(&[0.0, 0.0, 1.0, 0.0], 1).await.unwrap();
        assert_eq!(hits2[0].0, a.id);
        assert_eq!(hits2[0].2.as_deref(), Some("imgs/a2.png"), "path updated on conflict");
    }
}
