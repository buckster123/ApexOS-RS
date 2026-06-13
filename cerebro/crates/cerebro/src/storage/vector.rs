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

        // Insert into vec0 index using the memories table's integer rowid
        if self.vec_available {
            conn.execute(
                "INSERT OR REPLACE INTO memory_vectors(rowid, embedding)
                 SELECT rowid, ?1 FROM memories WHERE id = ?2",
                params![blob, memory_id.0],
            )?;
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
            conn.execute(
                "INSERT OR REPLACE INTO memory_vectors(rowid, embedding)
                 SELECT rowid, ?1 FROM memories WHERE id = ?2",
                params![blob, memory_id.0],
            )?;
        }
        Ok(())
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

fn init_fastembed(model_name: &str) -> Result<fastembed::TextEmbedding> {
    use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
    let model = match model_name {
        "BAAI/bge-small-en-v1.5" => EmbeddingModel::BGESmallENV15,
        other => anyhow::bail!("unsupported embed model: {other}"),
    };
    TextEmbedding::try_new(InitOptions::new(model))
}
