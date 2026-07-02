use std::{collections::HashMap, path::Path, sync::{Arc, OnceLock}};

use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use serde::{de::DeserializeOwned, Serialize};
use tokio::sync::Mutex;

use crate::{
    models::{AssociativeLink, MemoryNode, StrengthState},
    types::{AgentId, LinkType, MemoryId, Visibility, VisibilityScope},
};

/// Register the sqlite-vec extension exactly once for the process.
/// Uses sqlite3_auto_extension so every subsequent Connection::open* call has vec0.
/// Returns true if the extension was successfully registered.
fn register_sqlite_vec() -> bool {
    static REGISTERED: OnceLock<bool> = OnceLock::new();
    *REGISTERED.get_or_init(|| {
        unsafe {
            use rusqlite::ffi::sqlite3_auto_extension;
            use sqlite_vec::sqlite3_vec_init;
            // sqlite3_auto_extension expects void(*)(void); transmute is the
            // canonical way to bridge the extension init signature in Rust.
            type ExtInit = unsafe extern "C" fn(
                *mut rusqlite::ffi::sqlite3,
                *mut *const std::os::raw::c_char,
                *const rusqlite::ffi::sqlite3_api_routines,
            ) -> std::os::raw::c_int;
            sqlite3_auto_extension(Some(std::mem::transmute::<*const (), ExtInit>(
                sqlite3_vec_init as *const (),
            )));
        }
        true
    })
}

/// SQLite backend — single source of truth for all persistent state.
/// Graph and vector index are derived from this; never written independently.
pub struct SqliteStore {
    conn:              Arc<Mutex<Connection>>,
    /// True when sqlite-vec was successfully registered and the vec0 table exists.
    pub vec_available: bool,
}

/// Filters for `list_memories_scoped`.
pub struct ListFilter {
    pub memory_type:     Option<crate::types::MemoryType>,
    pub limit:           usize,
    pub offset:          usize,
    pub include_deleted: bool,
}

impl Default for ListFilter {
    fn default() -> Self {
        Self { memory_type: None, limit: 50, offset: 0, include_deleted: false }
    }
}

// ---------------------------------------------------------------------------
// Enum helpers — store as plain snake_case strings (no JSON quotes) matching
// the Python schema storage format and the SQL filter literals.
// ---------------------------------------------------------------------------

fn enum_to_str<T: Serialize>(val: &T) -> Result<String> {
    let json = serde_json::to_string(val)?;
    Ok(json.trim_matches('"').to_string())
}

fn str_to_enum<T: DeserializeOwned>(s: &str) -> Result<T> {
    Ok(serde_json::from_str(&format!("\"{}\"", s))?)
}

// ---------------------------------------------------------------------------
// Raw row type — extracts primitive values from a rusqlite Row without any
// fallible serde parsing (keeping the closure return type clean).
// Post-process with .into_memory_node() outside the query closure.
// ---------------------------------------------------------------------------

struct RawMemoryRow {
    id:                    String,
    content:               String,
    memory_type_str:       String,
    layer_str:             String,
    salience:              f32,
    tags_json:             String,
    agent_id:              Option<String>,
    visibility_str:        String,
    thread_id:             Option<String>,
    emotional_valence_str: Option<String>,
    emotional_intensity:   f32,
    created_at_str:        String,
    updated_at_str:        String,
    access_count:          i64,
    access_times_json:     String,
    fsrs_stability:        f32,
    fsrs_difficulty:       f32,
    fsrs_last_review_str:  Option<String>,
    metadata_json:         String,
}

fn row_to_raw(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawMemoryRow> {
    Ok(RawMemoryRow {
        id:                    row.get(0)?,
        content:               row.get(1)?,
        memory_type_str:       row.get(2)?,
        layer_str:             row.get(3)?,
        salience:             (row.get::<_, f64>(4)? as f32),
        tags_json:             row.get(5)?,
        agent_id:              row.get(6)?,
        visibility_str:        row.get(7)?,
        thread_id:             row.get(8)?,
        emotional_valence_str: row.get(9)?,
        emotional_intensity:  (row.get::<_, f64>(10)? as f32),
        created_at_str:        row.get(11)?,
        updated_at_str:        row.get(12)?,
        access_count:          row.get(13)?,
        access_times_json:     row.get(14)?,
        fsrs_stability:       (row.get::<_, f64>(15)? as f32),
        fsrs_difficulty:      (row.get::<_, f64>(16)? as f32),
        fsrs_last_review_str:  row.get(17)?,
        metadata_json:         row.get(18)?,
    })
}

impl RawMemoryRow {
    fn into_memory_node(self) -> Result<MemoryNode> {
        Ok(MemoryNode {
            id:                  MemoryId(self.id),
            content:             self.content,
            memory_type:         str_to_enum(&self.memory_type_str)?,
            layer:               str_to_enum(&self.layer_str)?,
            salience:            self.salience,
            tags:                serde_json::from_str(&self.tags_json)?,
            agent_id:            self.agent_id.map(AgentId),
            visibility:          str_to_enum(&self.visibility_str)?,
            thread_id:           self.thread_id,
            emotional_valence:   self.emotional_valence_str
                                     .as_deref()
                                     .map(str_to_enum)
                                     .transpose()?,
            emotional_intensity: self.emotional_intensity,
            created_at:  DateTime::parse_from_rfc3339(&self.created_at_str)?.with_timezone(&Utc),
            updated_at:  DateTime::parse_from_rfc3339(&self.updated_at_str)?.with_timezone(&Utc),
            access_count:        self.access_count as u32,
            access_times:        serde_json::from_str(&self.access_times_json)?,
            strength:            StrengthState {
                stability:   self.fsrs_stability,
                difficulty:  self.fsrs_difficulty,
                last_review: self.fsrs_last_review_str
                                 .as_deref()
                                 .map(|s| {
                                     DateTime::parse_from_rfc3339(s)
                                         .map(|dt| dt.with_timezone(&Utc))
                                 })
                                 .transpose()?,
            },
            metadata:            serde_json::from_str(&self.metadata_json)?,
        })
    }
}

struct RawLinkRow {
    source_id:      String,
    target_id:      String,
    link_type_str:  String,
    weight:         f32,
    created_at_str: String,
    last_traversed: Option<String>,
    traversal_count: i64,
}

fn row_to_raw_link(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawLinkRow> {
    Ok(RawLinkRow {
        source_id:      row.get(0)?,
        target_id:      row.get(1)?,
        link_type_str:  row.get(2)?,
        weight:        (row.get::<_, f64>(3)? as f32),
        created_at_str: row.get(4)?,
        last_traversed: row.get(5)?,
        traversal_count: row.get(6)?,
    })
}

impl RawLinkRow {
    fn into_link(self) -> Result<AssociativeLink> {
        let link_type: LinkType = str_to_enum(&self.link_type_str)?;
        let created_at = DateTime::parse_from_rfc3339(&self.created_at_str)?.with_timezone(&Utc);
        let last_traversed = self.last_traversed
            .as_deref()
            .map(|s| DateTime::parse_from_rfc3339(s).map(|dt| dt.with_timezone(&Utc)))
            .transpose()?;
        let mut link = AssociativeLink::new(
            MemoryId(self.source_id),
            MemoryId(self.target_id),
            link_type,
            self.weight,
        );
        link.created_at     = created_at;
        link.last_traversed = last_traversed;
        link.traversal_count = self.traversal_count as u32;
        Ok(link)
    }
}

// Column order used in all memory SELECT queries
const SELECT_COLS: &str =
    "id, content, memory_type, layer, salience, tags, agent_id, visibility, \
     thread_id, emotional_valence, emotional_intensity, created_at, updated_at, \
     access_count, access_times, fsrs_stability, fsrs_difficulty, fsrs_last_review, metadata";

// ---------------------------------------------------------------------------
// One-time migration: Python CerebroCortex schema → Rust schema
// ---------------------------------------------------------------------------

/// Detects a Python-generated `cerebro.db` (table `memory_nodes` present) and
/// converts it in-place to the Rust schema.  Runs once; subsequent opens are
/// no-ops (schema_version 100 marks completion).
fn migrate_from_python(conn: &mut Connection) -> Result<()> {
    // Is this a Python-schema DB?
    let has_py: bool = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='memory_nodes'",
        [],
        |r| r.get::<_, i64>(0),
    ).unwrap_or(0) > 0;
    if !has_py {
        return Ok(());
    }

    // Already migrated?
    let sv_exists: bool = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='schema_version'",
        [],
        |r| r.get::<_, i64>(0),
    ).unwrap_or(0) > 0;
    if sv_exists {
        let done: bool = conn.query_row(
            "SELECT COUNT(*) FROM schema_version WHERE version=100",
            [],
            |r| r.get::<_, i64>(0),
        ).unwrap_or(0) > 0;
        if done {
            return Ok(());
        }
    }

    tracing::info!("Python CerebroCortex schema detected — running one-time migration to Rust schema");

    // Disable FK enforcement for the duration: we rename parent tables before
    // recreating them, which would otherwise violate FK constraints.
    conn.execute_batch("PRAGMA foreign_keys=OFF;")?;

    {
        let tx = conn.transaction()?;
        tx.execute_batch(MIGRATION_SQL)?;
        tx.commit()?;
    }

    conn.execute_batch("PRAGMA foreign_keys=ON;")?;

    // Rebuild FTS5 from the freshly-populated memories table.
    // (Triggers also fired row-by-row during INSERT — rebuild is idempotent.)
    if let Err(e) = conn.execute_batch(
        "INSERT INTO memories_fts(memories_fts) VALUES('rebuild');"
    ) {
        tracing::warn!("FTS5 rebuild after migration failed (non-fatal): {e}");
    }

    let n: i64 = conn.query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))?;
    tracing::info!("Migration complete — {n} memories now in Rust schema");
    Ok(())
}

/// SQL executed inside a transaction during Python→Rust migration.
///
/// Column mapping summary (memory_nodes → memories):
///   tags_json → tags | conversation_thread → thread_id
///   valence → emotional_valence | arousal → emotional_intensity
///   last_accessed_at → updated_at | access_timestamps_json → access_times
///   stability/difficulty → fsrs_stability/fsrs_difficulty | metadata_json → metadata
const MIGRATION_SQL: &str = r#"
-- 1. Memories: memory_nodes → memories (already created empty by SCHEMA_SQL)
INSERT OR IGNORE INTO memories (
    id, content, memory_type, layer, salience, tags, agent_id, visibility,
    thread_id, emotional_valence, emotional_intensity, created_at, updated_at,
    access_count, access_times, fsrs_stability, fsrs_difficulty, fsrs_last_review,
    metadata, embedding, deleted_at
)
SELECT
    id,
    content,
    memory_type,
    layer,
    salience,
    COALESCE(tags_json, '[]'),
    agent_id,
    visibility,
    conversation_thread,
    valence,
    COALESCE(arousal, 0.5),
    strftime('%Y-%m-%dT%H:%M:%SZ', created_at),
    strftime('%Y-%m-%dT%H:%M:%SZ', COALESCE(last_accessed_at, created_at)),
    COALESCE(access_count, 0),
    COALESCE(
        (SELECT json_group_array(strftime('%Y-%m-%dT%H:%M:%SZ', value, 'unixepoch'))
         FROM json_each(memory_nodes.access_timestamps_json)),
        '[]'
    ),
    COALESCE(stability, 1.0),
    COALESCE(difficulty, 5.0),
    NULL,
    COALESCE(metadata_json, 'null'),
    NULL,
    deleted_at
FROM memory_nodes;

-- 2. Links: associative_links → links (already created empty by SCHEMA_SQL)
INSERT OR IGNORE INTO links (
    source_id, target_id, link_type, weight, created_at, last_traversed, traversal_count
)
SELECT source_id, target_id, link_type, weight,
       strftime('%Y-%m-%dT%H:%M:%SZ', created_at),
       CASE WHEN last_activated IS NULL THEN NULL
            ELSE strftime('%Y-%m-%dT%H:%M:%SZ', last_activated) END,
       COALESCE(activation_count, 0)
FROM associative_links;

-- 3. Agents: rename Python table (different columns), recreate with Rust schema
ALTER TABLE agents RENAME TO _py_agents;
CREATE TABLE agents (
    id            TEXT PRIMARY KEY,
    name          TEXT NOT NULL UNIQUE,
    description   TEXT,
    registered_at TEXT NOT NULL,
    last_seen     TEXT,
    metadata      TEXT NOT NULL DEFAULT 'null'
);
INSERT OR IGNORE INTO agents (id, name, description, registered_at, last_seen, metadata)
SELECT
    id,
    display_name,
    specialization,
    strftime('%Y-%m-%dT%H:%M:%SZ', registered_at),
    NULL,
    json_object(
        'symbol',     COALESCE(symbol, 'A'),
        'color',      COALESCE(color, '#888888'),
        'generation', COALESCE(generation, 0)
    )
FROM _py_agents;

-- 4. Episodes: rename Python table, recreate with Rust schema
ALTER TABLE episodes RENAME TO _py_episodes;
CREATE TABLE episodes (
    id         TEXT PRIMARY KEY,
    title      TEXT,
    agent_id   TEXT,
    thread_id  TEXT,
    started_at TEXT NOT NULL,
    ended_at   TEXT,
    summary    TEXT,
    memory_ids TEXT NOT NULL DEFAULT '[]',
    metadata   TEXT NOT NULL DEFAULT 'null'
);
INSERT OR IGNORE INTO episodes (id, title, agent_id, thread_id, started_at, ended_at, summary, memory_ids, metadata)
SELECT id, title, agent_id, session_id,
       strftime('%Y-%m-%dT%H:%M:%SZ', COALESCE(started_at, created_at)),
       CASE WHEN ended_at IS NULL THEN NULL
            ELSE strftime('%Y-%m-%dT%H:%M:%SZ', ended_at) END,
       NULL, '[]', 'null'
FROM _py_episodes;

-- 5. Episode steps: rename Python table, recreate with Rust schema
--    Python: position (int), role (text)  →  Rust: step_index (int), description (text)
ALTER TABLE episode_steps RENAME TO _py_episode_steps;
CREATE TABLE episode_steps (
    episode_id TEXT    NOT NULL,
    step_index INTEGER NOT NULL,
    description TEXT   NOT NULL,
    memory_id  TEXT,
    timestamp  TEXT    NOT NULL,
    PRIMARY KEY (episode_id, step_index),
    FOREIGN KEY (episode_id) REFERENCES episodes(id)
);
INSERT OR IGNORE INTO episode_steps (episode_id, step_index, description, memory_id, timestamp)
SELECT episode_id, position, COALESCE(role, ''),
       memory_id,
       strftime('%Y-%m-%dT%H:%M:%SZ', timestamp)
FROM _py_episode_steps;

-- 6. Audit log: rename Python table (different columns), recreate with Rust schema
--    The index idx_audit_ts was created by SCHEMA_SQL on the old table; drop and recreate.
ALTER TABLE audit_log RENAME TO _py_audit_log;
DROP INDEX IF EXISTS idx_audit_ts;
CREATE TABLE audit_log (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp TEXT NOT NULL,
    agent_id  TEXT,
    action    TEXT NOT NULL,
    memory_id TEXT,
    details   TEXT
);
CREATE INDEX idx_audit_ts ON audit_log(timestamp);
INSERT OR IGNORE INTO audit_log (timestamp, agent_id, action, memory_id, details)
SELECT strftime('%Y-%m-%dT%H:%M:%SZ', timestamp),
       actor_agent_id, event_type, target_memory_id, details_json
FROM _py_audit_log;

-- 7. Mark migration complete in Python's schema_version table.
INSERT OR REPLACE INTO schema_version (version, applied_at, description)
VALUES (100, datetime('now'), 'Migrated from Python CerebroCortex to Rust schema');
"#;

impl SqliteStore {
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Register sqlite-vec before opening so the extension is available on this connection.
        register_sqlite_vec();
        let mut conn = Connection::open(path)?;
        // CB-002: two daemons (cerebro-mcp + cerebro-api) share one DB file. WAL allows
        // N readers + 1 writer; a second cross-process writer otherwise fails instantly
        // with SQLITE_BUSY. A busy_timeout makes colliding writers wait instead of dropping
        // the write. synchronous=NORMAL is the standard durability/throughput tradeoff for WAL.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA synchronous=NORMAL;",
        )?;

        // Base schema (no vec0 dependency)
        conn.execute_batch(SCHEMA_SQL)?;

        // One-time migration: if this DB was created by the Python CerebroCortex server
        // (uses memory_nodes / associative_links), transparently convert it to Rust schema.
        migrate_from_python(&mut conn)?;

        // Try to create the vec0 virtual table; works only if sqlite-vec loaded successfully.
        let vec_available = conn.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS memory_vectors USING vec0(embedding float[384]);"
        ).is_ok();
        if vec_available {
            tracing::info!("sqlite-vec loaded — vector search enabled");
        } else {
            tracing::warn!("vec0 table init failed — falling back to FTS5 keyword search");
        }

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            vec_available,
        })
    }

    /// Clone the shared connection Arc — used by VectorStore and GraphStore.
    pub fn shared_conn(&self) -> Arc<Mutex<Connection>> {
        self.conn.clone()
    }

    // -----------------------------------------------------------------------
    // Memory CRUD
    // -----------------------------------------------------------------------

    pub async fn insert_memory(&self, node: &MemoryNode) -> Result<()> {
        let conn = self.conn.lock().await;
        // CB-005: INSERT OR REPLACE on an existing id deletes the old row and
        // reinserts, which can allocate a fresh integer rowid. The vec0 index
        // (memory_vectors) is keyed by that rowid and is not FK/trigger-bound,
        // so the prior vector would be orphaned (and its old rowid later reused
        // by another memory → mis-ranked recall). Drop the stale vec row first;
        // the caller re-embeds via embed_and_store after insert.
        if self.vec_available {
            conn.execute(
                "DELETE FROM memory_vectors WHERE rowid IN \
                 (SELECT rowid FROM memories WHERE id = ?)",
                params![node.id.0],
            )?;
        }
        conn.execute(
            "INSERT OR REPLACE INTO memories \
             (id, content, memory_type, layer, salience, tags, agent_id, visibility, \
              thread_id, emotional_valence, emotional_intensity, \
              created_at, updated_at, access_count, access_times, \
              fsrs_stability, fsrs_difficulty, fsrs_last_review, metadata) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)",
            params![
                node.id.0,
                node.content,
                enum_to_str(&node.memory_type)?,
                enum_to_str(&node.layer)?,
                node.salience as f64,
                serde_json::to_string(&node.tags)?,
                node.agent_id.as_ref().map(|a| &a.0),
                enum_to_str(&node.visibility)?,
                node.thread_id,
                node.emotional_valence.as_ref().map(enum_to_str).transpose()?,
                node.emotional_intensity as f64,
                node.created_at.to_rfc3339(),
                node.updated_at.to_rfc3339(),
                node.access_count as i64,
                serde_json::to_string(&node.access_times)?,
                node.strength.stability as f64,
                node.strength.difficulty as f64,
                node.strength.last_review.map(|t| t.to_rfc3339()),
                serde_json::to_string(&node.metadata)?,
            ],
        )?;
        Ok(())
    }

    pub async fn get_memory(&self, id: &MemoryId, scope: &VisibilityScope) -> Result<Option<MemoryNode>> {
        let conn = self.conn.lock().await;
        let (scope_sql, scope_params) = scope.sql_filter();
        let sql = format!(
            "SELECT {SELECT_COLS} FROM memories \
             WHERE id = ? AND {scope_sql} AND deleted_at IS NULL"
        );
        let id_str = id.0.clone();
        let mut dyn_params: Vec<&dyn rusqlite::ToSql> = vec![&id_str];
        for s in &scope_params {
            dyn_params.push(s);
        }
        let mut stmt = conn.prepare(&sql)?;
        let result = stmt.query_row(dyn_params.as_slice(), row_to_raw);
        match result {
            Ok(raw) => Ok(Some(raw.into_memory_node()?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Soft-delete a memory. Returns true if the memory existed and was deleted.
    pub async fn delete_memory(&self, id: &MemoryId) -> Result<bool> {
        let conn = self.conn.lock().await;
        let changed = conn.execute(
            "UPDATE memories SET deleted_at = ?1 WHERE id = ?2 AND deleted_at IS NULL",
            params![Utc::now().to_rfc3339(), id.0],
        )?;
        // CB-020: the memories_au trigger now evicts a soft-deleted row from the
        // FTS5 index (it only re-inserts when deleted_at IS NULL), so the keyword
        // index shrinks on soft-delete and restore_memory's UPDATE re-indexes it.
        Ok(changed > 0)
    }

    pub async fn update_memory(&self, node: &MemoryNode) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE memories SET \
             content=?2, memory_type=?3, layer=?4, salience=?5, tags=?6, agent_id=?7, \
             visibility=?8, thread_id=?9, emotional_valence=?10, emotional_intensity=?11, \
             updated_at=?12, access_count=?13, access_times=?14, \
             fsrs_stability=?15, fsrs_difficulty=?16, fsrs_last_review=?17, metadata=?18 \
             WHERE id=?1 AND deleted_at IS NULL",
            params![
                node.id.0,
                node.content,
                enum_to_str(&node.memory_type)?,
                enum_to_str(&node.layer)?,
                node.salience as f64,
                serde_json::to_string(&node.tags)?,
                node.agent_id.as_ref().map(|a| &a.0),
                enum_to_str(&node.visibility)?,
                node.thread_id,
                node.emotional_valence.as_ref().map(enum_to_str).transpose()?,
                node.emotional_intensity as f64,
                Utc::now().to_rfc3339(),
                node.access_count as i64,
                serde_json::to_string(&node.access_times)?,
                node.strength.stability as f64,
                node.strength.difficulty as f64,
                node.strength.last_review.map(|t| t.to_rfc3339()),
                serde_json::to_string(&node.metadata)?,
            ],
        )?;
        Ok(())
    }

    /// Persist ACT-R access reinforcement for a batch of recalled memories.
    ///
    /// Only `access_count` + `access_times` are written (a lean UPDATE, not the
    /// full-row `update_memory`), so the recall hot path stays cheap. All updates
    /// run in one transaction. Rows that are soft-deleted are skipped by the
    /// `deleted_at IS NULL` guard. Called by `recall()` so retrieval strengthens
    /// base-level activation ("recall sharpens memory").
    /// Persist the recall-time reinforcement for each node: ACT-R access history
    /// (`access_count`/`access_times`) AND the FSRS review state
    /// (`fsrs_stability`/`fsrs_difficulty`/`fsrs_last_review`). One batched UPDATE
    /// per node keeps the recall hot path cheap. `last_review` is RFC3339 (NULL
    /// when unset) to match the read-side parse.
    // Tuple = (id, access_count, access_times_json, fsrs_stability,
    // fsrs_difficulty, fsrs_last_review_rfc3339) — an internal one-call record;
    // a named struct buys nothing at the single (cortex::recall) build site.
    #[allow(clippy::type_complexity)]
    pub async fn record_accesses(
        &self,
        updates: &[(MemoryId, u32, String, f32, f32, Option<String>)],
    ) -> Result<()> {
        if updates.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        for (id, count, times_json, stability, difficulty, last_review) in updates {
            tx.execute(
                "UPDATE memories SET access_count = ?1, access_times = ?2, \
                 fsrs_stability = ?3, fsrs_difficulty = ?4, fsrs_last_review = ?5 \
                 WHERE id = ?6 AND deleted_at IS NULL",
                params![*count as i64, times_json, *stability as f64, *difficulty as f64, last_review, id.0],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub async fn list_memories_scoped(
        &self,
        scope: &VisibilityScope,
        filter: &ListFilter,
    ) -> Result<Vec<MemoryNode>> {
        let conn = self.conn.lock().await;
        let (scope_sql, scope_params) = scope.sql_filter();

        let type_str: Option<String> = filter.memory_type
            .as_ref()
            .map(enum_to_str)
            .transpose()?;

        let deleted_clause = if filter.include_deleted { "" } else { "AND deleted_at IS NULL" };
        let type_clause    = if type_str.is_some() { "AND memory_type = ?" } else { "" };

        let sql = format!(
            "SELECT {SELECT_COLS} FROM memories \
             WHERE {scope_sql} {deleted_clause} {type_clause} \
             ORDER BY salience DESC, created_at DESC \
             LIMIT ? OFFSET ?"
        );

        let limit_val  = filter.limit  as i64;
        let offset_val = filter.offset as i64;

        let mut dyn_params: Vec<&dyn rusqlite::ToSql> = Vec::new();
        for s in &scope_params { dyn_params.push(s); }
        if let Some(ref ts) = type_str { dyn_params.push(ts); }
        dyn_params.push(&limit_val);
        dyn_params.push(&offset_val);

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(dyn_params.as_slice(), row_to_raw)?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row?.into_memory_node()?);
        }
        Ok(results)
    }

    /// Exact-tag lookup: memories carrying **every** tag in `tags` (AND), scoped,
    /// newest first. Matches the exact quoted string inside the stored tags-JSON
    /// (`%"<tag>"%`, LIKE-escaped) — precise where FTS/vector recall is fuzzy.
    /// The provenance query: `["from:apex1", "origin:mem_x"]` finds a prior
    /// federated import; `["from:apex1"]` lists everything a peer ever sent.
    pub async fn find_by_tags(
        &self,
        scope: &VisibilityScope,
        tags: &[String],
        limit: usize,
    ) -> Result<Vec<MemoryNode>> {
        if tags.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock().await;
        let (scope_sql, scope_params) = scope.sql_filter();

        let tag_clause = "AND tags LIKE ? ESCAPE '\\' ".repeat(tags.len());
        let sql = format!(
            "SELECT {SELECT_COLS} FROM memories \
             WHERE {scope_sql} AND deleted_at IS NULL {tag_clause} \
             ORDER BY created_at DESC LIMIT ?"
        );
        // The exact tag string, quoted as it appears inside the JSON array, with
        // LIKE metacharacters escaped so a tag can't wildcard the match.
        let patterns: Vec<String> = tags.iter()
            .map(|t| {
                let escaped = t.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
                format!("%\"{escaped}\"%")
            })
            .collect();
        let limit_val = limit as i64;
        let mut dyn_params: Vec<&dyn rusqlite::ToSql> = Vec::new();
        for s in &scope_params { dyn_params.push(s); }
        for p in &patterns { dyn_params.push(p); }
        dyn_params.push(&limit_val);

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(dyn_params.as_slice(), row_to_raw)?;
        let mut results = Vec::new();
        for row in rows {
            results.push(row?.into_memory_node()?);
        }
        Ok(results)
    }

    // -----------------------------------------------------------------------
    // Link CRUD
    // -----------------------------------------------------------------------

    pub async fn insert_link(&self, link: &AssociativeLink) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT OR REPLACE INTO links \
             (source_id, target_id, link_type, weight, created_at, last_traversed, traversal_count) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                link.source_id.0,
                link.target_id.0,
                enum_to_str(&link.link_type)?,
                link.weight as f64,
                link.created_at.to_rfc3339(),
                link.last_traversed.map(|t| t.to_rfc3339()),
                link.traversal_count as i64,
            ],
        )?;
        Ok(())
    }

    pub async fn list_links_from(&self, id: &MemoryId) -> Result<Vec<AssociativeLink>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT source_id, target_id, link_type, weight, created_at, last_traversed, traversal_count \
             FROM links WHERE source_id = ?"
        )?;
        let rows = stmt.query_map(params![id.0], row_to_raw_link)?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row?.into_link()?);
        }
        Ok(results)
    }

    /// Hard-delete a single memory (use after backup/purge confirmation).
    ///
    /// Cleans dependent rows in the same transaction so the hard delete is
    /// consistent: CB-022 — `links` FK-references `memories(id)` without
    /// CASCADE, so a still-linked memory would otherwise fail the
    /// `foreign_keys=ON` constraint; CB-005 — the `memory_vectors` vec0 index
    /// is keyed by the reusable integer rowid and has no FK/trigger, so an
    /// orphaned vector would bloat the index and, after rowid reuse, mis-rank
    /// a future memory. The FTS5 index is cleaned by the `memories_ad` trigger.
    pub async fn purge_memory(&self, id: &MemoryId) -> Result<bool> {
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        if self.vec_available {
            tx.execute(
                "DELETE FROM memory_vectors WHERE rowid IN \
                 (SELECT rowid FROM memories WHERE id = ?)",
                params![id.0],
            )?;
        }
        tx.execute(
            "DELETE FROM links WHERE source_id = ?1 OR target_id = ?1",
            params![id.0],
        )?;
        let changed = tx.execute("DELETE FROM memories WHERE id = ?", params![id.0])?;
        tx.commit()?;
        Ok(changed > 0)
    }

    /// Hard-delete all soft-deleted memories.
    ///
    /// Same dependent-row cleanup as `purge_memory` (CB-005 / CB-022), applied
    /// to the whole soft-deleted set inside one transaction.
    pub async fn purge_all_deleted(&self) -> Result<usize> {
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        if self.vec_available {
            tx.execute(
                "DELETE FROM memory_vectors WHERE rowid IN \
                 (SELECT rowid FROM memories WHERE deleted_at IS NOT NULL)",
                [],
            )?;
        }
        tx.execute(
            "DELETE FROM links WHERE source_id IN \
               (SELECT id FROM memories WHERE deleted_at IS NOT NULL) \
             OR target_id IN \
               (SELECT id FROM memories WHERE deleted_at IS NOT NULL)",
            [],
        )?;
        let changed = tx.execute("DELETE FROM memories WHERE deleted_at IS NOT NULL", [])?;
        tx.commit()?;
        Ok(changed)
    }

    /// Restore a soft-deleted memory.
    pub async fn restore_memory(&self, id: &MemoryId) -> Result<bool> {
        let conn = self.conn.lock().await;
        let changed = conn.execute(
            "UPDATE memories SET deleted_at = NULL WHERE id = ? AND deleted_at IS NOT NULL",
            params![id.0],
        )?;
        Ok(changed > 0)
    }

    /// Aggregate counts: (total_live, total_deleted, count_per_type as JSON).
    pub async fn memory_stats(&self) -> Result<serde_json::Value> {
        let conn = self.conn.lock().await;
        let live: i64 = conn.query_row(
            "SELECT COUNT(*) FROM memories WHERE deleted_at IS NULL", [], |r| r.get(0))?;
        let deleted: i64 = conn.query_row(
            "SELECT COUNT(*) FROM memories WHERE deleted_at IS NOT NULL", [], |r| r.get(0))?;
        let links: i64 = conn.query_row("SELECT COUNT(*) FROM links", [], |r| r.get(0))?;

        let mut by_type: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
        let mut stmt = conn.prepare(
            "SELECT memory_type, COUNT(*) FROM memories WHERE deleted_at IS NULL GROUP BY memory_type"
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (t, count) = row?;
            by_type.insert(t, serde_json::Value::Number(count.into()));
        }

        Ok(serde_json::json!({
            "total_memories": live,
            "deleted_memories": deleted,
            "total_links": links,
            "by_type": by_type,
        }))
    }

    /// Bulk-load memories by ID list — used by the recall pipeline.
    pub async fn get_memories_by_ids(&self, ids: &[MemoryId], scope: &VisibilityScope) -> Result<Vec<MemoryNode>> {
        if ids.is_empty() { return Ok(vec![]); }
        let placeholders: String = ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let (scope_sql, scope_params) = scope.sql_filter();
        let sql = format!(
            "SELECT {SELECT_COLS} FROM memories \
             WHERE id IN ({placeholders}) AND {scope_sql} AND deleted_at IS NULL"
        );
        let conn = self.conn.lock().await;
        let id_strs: Vec<&str> = ids.iter().map(|id| id.0.as_str()).collect();
        let mut dyn_params: Vec<&dyn rusqlite::ToSql> =
            id_strs.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        for s in &scope_params { dyn_params.push(s); }
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(dyn_params.as_slice(), row_to_raw)?;
        let mut results = Vec::new();
        for row in rows { results.push(row?.into_memory_node()?); }
        Ok(results)
    }

    /// All non-deleted memory IDs — used by GraphStore::rebuild_from_db.
    pub async fn list_all_memory_ids(&self) -> Result<Vec<MemoryId>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT id FROM memories WHERE deleted_at IS NULL"
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut ids = Vec::new();
        for row in rows {
            ids.push(MemoryId(row?));
        }
        Ok(ids)
    }

    /// Bulk-fetch `(visibility, agent_id)` for the given memory ids — used to
    /// build the scope-visibility map that gates spreading activation (C-RS-003).
    /// Mirrors Python `_build_visibility_cache`. Ids absent from the result are
    /// not in the DB and are treated as visible (then filtered by the final
    /// SQLite scope query), matching Python's `_check_access` fallthrough.
    pub async fn get_visibility_meta(
        &self,
        ids: &[MemoryId],
    ) -> Result<HashMap<MemoryId, (Visibility, Option<AgentId>)>> {
        if ids.is_empty() { return Ok(HashMap::new()); }
        let placeholders: String = ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let sql = format!(
            "SELECT id, visibility, agent_id FROM memories \
             WHERE id IN ({placeholders}) AND deleted_at IS NULL"
        );
        let conn = self.conn.lock().await;
        let id_strs: Vec<&str> = ids.iter().map(|id| id.0.as_str()).collect();
        let dyn_params: Vec<&dyn rusqlite::ToSql> =
            id_strs.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(dyn_params.as_slice(), |row| {
            let id:  String = row.get(0)?;
            let vis: String = row.get(1)?;
            let agent: Option<String> = row.get(2)?;
            Ok((id, vis, agent))
        })?;
        let mut map = HashMap::new();
        for row in rows {
            let (id, vis, agent) = row?;
            let visibility = match vis.as_str() {
                "private" => Visibility::Private,
                "thread"  => Visibility::Thread,
                _         => Visibility::Shared,
            };
            map.insert(MemoryId(id), (visibility, agent.map(AgentId)));
        }
        Ok(map)
    }

    /// All links whose both endpoints are non-deleted memories — for graph rebuild.
    pub async fn list_all_links(&self) -> Result<Vec<AssociativeLink>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT l.source_id, l.target_id, l.link_type, l.weight, \
                    l.created_at, l.last_traversed, l.traversal_count \
             FROM links l \
             JOIN memories ms ON ms.id = l.source_id AND ms.deleted_at IS NULL \
             JOIN memories mt ON mt.id = l.target_id AND mt.deleted_at IS NULL"
        )?;
        let rows = stmt.query_map([], row_to_raw_link)?;
        let mut results = Vec::new();
        for row in rows {
            results.push(row?.into_link()?);
        }
        Ok(results)
    }

    // -----------------------------------------------------------------------
    // Deleted-memory helpers
    // -----------------------------------------------------------------------

    pub async fn list_deleted_memories(&self, scope: &VisibilityScope, limit: usize) -> Result<Vec<MemoryNode>> {
        let conn = self.conn.lock().await;
        let (scope_sql, scope_params) = scope.sql_filter();
        let sql = format!(
            "SELECT {SELECT_COLS} FROM memories WHERE {scope_sql} AND deleted_at IS NOT NULL \
             ORDER BY deleted_at DESC LIMIT ?"
        );
        let limit_val = limit as i64;
        let mut dp: Vec<&dyn rusqlite::ToSql> =
            scope_params.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        dp.push(&limit_val);
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(dp.as_slice(), row_to_raw)?;
        let mut out = Vec::new();
        for r in rows { out.push(r?.into_memory_node()?); }
        Ok(out)
    }

    pub async fn bulk_delete(&self, ids: &[MemoryId]) -> Result<usize> {
        if ids.is_empty() { return Ok(0); }
        let placeholders = std::iter::repeat_n("?", ids.len()).collect::<Vec<_>>().join(", ");
        let sql = format!(
            "UPDATE memories SET deleted_at = ? WHERE id IN ({placeholders}) AND deleted_at IS NULL"
        );
        let conn = self.conn.lock().await;
        let now = Utc::now().to_rfc3339();
        let id_strs: Vec<String> = ids.iter().map(|id| id.0.clone()).collect();
        let mut dp: Vec<&dyn rusqlite::ToSql> = vec![&now as &dyn rusqlite::ToSql];
        for s in &id_strs { dp.push(s); }
        Ok(conn.execute(&sql, dp.as_slice())?)
    }

    // -----------------------------------------------------------------------
    // Agent registry
    // -----------------------------------------------------------------------

    pub async fn register_agent(
        &self,
        id: &str,
        name: &str,
        description: Option<&str>,
        metadata: &serde_json::Value,
    ) -> Result<()> {
        let conn = self.conn.lock().await;
        let now = Utc::now().to_rfc3339();
        let meta_str = serde_json::to_string(metadata)?;
        conn.execute(
            "INSERT OR REPLACE INTO agents \
             (id, name, description, registered_at, last_seen, metadata) \
             VALUES (?1, ?2, ?3, ?4, ?4, ?5)",
            params![id, name, description, now, meta_str],
        )?;
        Ok(())
    }

    pub async fn list_agents(&self) -> Result<Vec<serde_json::Value>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT id, name, description, registered_at, last_seen, metadata \
             FROM agents ORDER BY name"
        )?;
        let rows = stmt.query_map([], |r| Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, Option<String>>(4)?,
            r.get::<_, String>(5)?,
        )))?;
        let mut agents = Vec::new();
        for row in rows {
            let (id, name, desc, reg_at, last_seen, meta_str) = row?;
            let metadata: serde_json::Value =
                serde_json::from_str(&meta_str).unwrap_or(serde_json::Value::Null);
            agents.push(serde_json::json!({
                "id": id, "name": name, "description": desc,
                "registered_at": reg_at, "last_seen": last_seen,
                "metadata": metadata,
            }));
        }
        Ok(agents)
    }

    pub async fn share_memory(&self, memory_id: &MemoryId, target_agent_id: Option<&str>) -> Result<bool> {
        let conn = self.conn.lock().await;
        let changed = if let Some(aid) = target_agent_id {
            conn.execute(
                "UPDATE memories SET visibility='private', agent_id=?1 \
                 WHERE id=?2 AND deleted_at IS NULL",
                params![aid, memory_id.0],
            )?
        } else {
            conn.execute(
                "UPDATE memories SET visibility='shared', agent_id=NULL \
                 WHERE id=?1 AND deleted_at IS NULL",
                params![memory_id.0],
            )?
        };
        Ok(changed > 0)
    }

    // -----------------------------------------------------------------------
    // Thread operations
    // -----------------------------------------------------------------------

    pub async fn list_threads(&self, scope: &VisibilityScope) -> Result<Vec<String>> {
        let conn = self.conn.lock().await;
        let (scope_sql, scope_params) = scope.sql_filter();
        let sql = format!(
            "SELECT DISTINCT thread_id FROM memories \
             WHERE {scope_sql} AND deleted_at IS NULL AND thread_id IS NOT NULL \
             ORDER BY thread_id"
        );
        let dp: Vec<&dyn rusqlite::ToSql> =
            scope_params.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(dp.as_slice(), |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows { out.push(r?); }
        Ok(out)
    }

    pub async fn get_thread_memories(
        &self,
        thread_id: &str,
        scope: &VisibilityScope,
    ) -> Result<Vec<MemoryNode>> {
        let conn = self.conn.lock().await;
        let (scope_sql, scope_params) = scope.sql_filter();
        let sql = format!(
            "SELECT {SELECT_COLS} FROM memories \
             WHERE {scope_sql} AND deleted_at IS NULL AND thread_id = ? \
             ORDER BY created_at ASC"
        );
        let tid = thread_id.to_string();
        let mut dp: Vec<&dyn rusqlite::ToSql> =
            scope_params.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        dp.push(&tid as &dyn rusqlite::ToSql);
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(dp.as_slice(), row_to_raw)?;
        let mut out = Vec::new();
        for r in rows { out.push(r?.into_memory_node()?); }
        Ok(out)
    }

    pub async fn prune_thread(&self, thread_id: &str) -> Result<usize> {
        let conn = self.conn.lock().await;
        Ok(conn.execute(
            "UPDATE memories SET deleted_at=?1 WHERE thread_id=?2 AND deleted_at IS NULL",
            params![Utc::now().to_rfc3339(), thread_id],
        )?)
    }

    // -----------------------------------------------------------------------
    // Inbox (tag-based messaging: tag = "to:{agent_id}")
    // -----------------------------------------------------------------------

    pub async fn check_inbox(
        &self,
        agent_id: &str,
        scope: &VisibilityScope,
        limit: usize,
    ) -> Result<Vec<MemoryNode>> {
        let conn = self.conn.lock().await;
        let (scope_sql, scope_params) = scope.sql_filter();
        let tag_val = format!("to:{agent_id}");
        let limit_val = limit as i64;
        let sql = format!(
            "SELECT {SELECT_COLS} FROM memories \
             WHERE {scope_sql} AND deleted_at IS NULL \
               AND EXISTS (SELECT 1 FROM json_each(tags) WHERE value = ?) \
             ORDER BY created_at DESC LIMIT ?"
        );
        let mut dp: Vec<&dyn rusqlite::ToSql> =
            scope_params.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        dp.push(&tag_val as &dyn rusqlite::ToSql);
        dp.push(&limit_val as &dyn rusqlite::ToSql);
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(dp.as_slice(), row_to_raw)?;
        let mut out = Vec::new();
        for r in rows { out.push(r?.into_memory_node()?); }
        Ok(out)
    }

    // -----------------------------------------------------------------------
    // Tag operations (operate on memories.tags JSON array)
    // -----------------------------------------------------------------------

    pub async fn list_tags(&self, scope: &VisibilityScope) -> Result<Vec<serde_json::Value>> {
        let conn = self.conn.lock().await;
        let (scope_sql, scope_params) = scope.sql_filter();
        let sql = format!(
            "SELECT value, COUNT(*) as cnt \
             FROM memories, json_each(memories.tags) \
             WHERE {scope_sql} AND deleted_at IS NULL \
             GROUP BY value ORDER BY cnt DESC"
        );
        let dp: Vec<&dyn rusqlite::ToSql> =
            scope_params.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(dp.as_slice(), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        let mut out = Vec::new();
        for r in rows {
            let (tag, cnt) = r?;
            out.push(serde_json::json!({ "tag": tag, "count": cnt }));
        }
        Ok(out)
    }

    pub async fn delete_tag_everywhere(&self, tag: &str) -> Result<usize> {
        let conn = self.conn.lock().await;
        Ok(conn.execute(
            "UPDATE memories \
             SET tags = IFNULL(\
                 (SELECT json_group_array(value) FROM json_each(tags) WHERE value != ?1), '[]') \
             WHERE deleted_at IS NULL \
               AND EXISTS (SELECT 1 FROM json_each(tags) WHERE value = ?1)",
            params![tag],
        )?)
    }

    pub async fn rename_tag_everywhere(&self, old_tag: &str, new_tag: &str) -> Result<usize> {
        let conn = self.conn.lock().await;
        Ok(conn.execute(
            "UPDATE memories \
             SET tags = (\
                 SELECT json_group_array(CASE WHEN value = ?1 THEN ?2 ELSE value END) \
                 FROM json_each(tags)) \
             WHERE deleted_at IS NULL \
               AND EXISTS (SELECT 1 FROM json_each(tags) WHERE value = ?1)",
            params![old_tag, new_tag],
        )?)
    }

    // -----------------------------------------------------------------------
    // Analytics
    // -----------------------------------------------------------------------

    pub async fn emotional_summary(&self, scope: &VisibilityScope) -> Result<serde_json::Value> {
        let conn = self.conn.lock().await;
        let (scope_sql, scope_params) = scope.sql_filter();
        let sql = format!(
            "SELECT emotional_valence, COUNT(*), AVG(emotional_intensity) \
             FROM memories WHERE {scope_sql} AND deleted_at IS NULL \
             GROUP BY emotional_valence"
        );
        let dp: Vec<&dyn rusqlite::ToSql> =
            scope_params.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(dp.as_slice(), |r| Ok((
            r.get::<_, Option<String>>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, f64>(2)?,
        )))?;
        let mut by_valence = serde_json::Map::new();
        let (mut total, mut total_intensity) = (0i64, 0.0f64);
        for r in rows {
            let (valence, cnt, avg_i) = r?;
            let key = valence.unwrap_or_else(|| "neutral".to_string());
            by_valence.insert(key, serde_json::json!({ "count": cnt, "avg_intensity": avg_i }));
            total += cnt;
            total_intensity += avg_i * cnt as f64;
        }
        Ok(serde_json::json!({
            "by_valence": by_valence,
            "total_with_emotion": total,
            "avg_intensity": if total > 0 { total_intensity / total as f64 } else { 0.0 },
        }))
    }

    /// Memories whose FSRS retrievability falls below `threshold`.
    /// Uses the canonical FSRS power-law curve R(t) = (1 + t / (9·S))^-1
    /// (crate::activation::retrievability), the same function store/recall
    /// scheduling uses — so the `retrievability` value reported here is
    /// consistent with the rest of the system (CB-013).
    pub async fn activation_at_risk(
        &self,
        scope: &VisibilityScope,
        threshold: f32,
        limit: usize,
    ) -> Result<Vec<serde_json::Value>> {
        let conn = self.conn.lock().await;
        let (scope_sql, scope_params) = scope.sql_filter();
        let sql = format!(
            "SELECT id, content, fsrs_stability, fsrs_last_review, salience, memory_type \
             FROM memories \
             WHERE {scope_sql} AND deleted_at IS NULL AND fsrs_last_review IS NOT NULL"
        );
        let dp: Vec<&dyn rusqlite::ToSql> =
            scope_params.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(dp.as_slice(), |r| Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, f64>(2)? as f32,
            r.get::<_, String>(3)?,
            r.get::<_, f64>(4)? as f32,
            r.get::<_, String>(5)?,
        )))?;
        let now = Utc::now();
        let mut at_risk: Vec<serde_json::Value> = Vec::new();
        for r in rows {
            let (id, content, stability, lr_str, salience, mem_type) = r?;
            if let Ok(lr) = DateTime::parse_from_rfc3339(&lr_str) {
                let days = (now - lr.with_timezone(&Utc)).num_seconds() as f32 / 86400.0;
                let ret = crate::activation::retrievability(days.max(0.0), stability);
                if ret < threshold {
                    at_risk.push(serde_json::json!({
                        "id": id, "content": content,
                        "retrievability": ret, "stability": stability,
                        "salience": salience, "memory_type": mem_type,
                    }));
                }
            }
        }
        at_risk.sort_by(|a, b| {
            a["retrievability"].as_f64().unwrap_or(1.0)
                .partial_cmp(&b["retrievability"].as_f64().unwrap_or(1.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        at_risk.truncate(limit);
        Ok(at_risk)
    }

    pub async fn memory_health(&self, scope: &VisibilityScope) -> Result<serde_json::Value> {
        let conn = self.conn.lock().await;
        let (scope_sql, scope_params) = scope.sql_filter();

        let stats_sql = format!(
            "SELECT \
               SUM(CASE WHEN deleted_at IS NULL THEN 1 ELSE 0 END), \
               SUM(CASE WHEN deleted_at IS NOT NULL THEN 1 ELSE 0 END), \
               AVG(CASE WHEN deleted_at IS NULL THEN salience ELSE NULL END), \
               AVG(CASE WHEN deleted_at IS NULL THEN fsrs_stability ELSE NULL END) \
             FROM memories WHERE {scope_sql}"
        );
        let (total, deleted, avg_sal, avg_stab): (i64, i64, f64, f64) = {
            let dp: Vec<&dyn rusqlite::ToSql> =
                scope_params.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
            conn.query_row(&stats_sql, dp.as_slice(), |r| Ok((
                r.get::<_, i64>(0).unwrap_or(0),
                r.get::<_, i64>(1).unwrap_or(0),
                r.get::<_, f64>(2).unwrap_or(0.0),
                r.get::<_, f64>(3).unwrap_or(1.0),
            )))?
        };
        let links: i64 = conn.query_row("SELECT COUNT(*) FROM links", [], |r| r.get(0))?;

        let type_sql = format!(
            "SELECT memory_type, COUNT(*) FROM memories \
             WHERE {scope_sql} AND deleted_at IS NULL GROUP BY memory_type"
        );
        let mut by_type = serde_json::Map::new();
        {
            let dp: Vec<&dyn rusqlite::ToSql> =
                scope_params.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
            let mut stmt = conn.prepare(&type_sql)?;
            let rows = stmt.query_map(dp.as_slice(), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })?;
            for r in rows { let (t, cnt) = r?; by_type.insert(t, cnt.into()); }
        }

        Ok(serde_json::json!({
            "total_memories": total,
            "deleted_memories": deleted,
            "total_links": links,
            "avg_salience": avg_sal,
            "avg_stability": avg_stab,
            "by_type": by_type,
        }))
    }

    pub async fn activation_heatmap(&self, scope: &VisibilityScope) -> Result<serde_json::Value> {
        let conn = self.conn.lock().await;
        let (scope_sql, scope_params) = scope.sql_filter();
        let sql = format!(
            "SELECT memory_type, strftime('%Y-%m', created_at) as month, COUNT(*) \
             FROM memories WHERE {scope_sql} AND deleted_at IS NULL \
             GROUP BY memory_type, month ORDER BY month ASC"
        );
        let dp: Vec<&dyn rusqlite::ToSql> =
            scope_params.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(dp.as_slice(), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
        })?;
        let mut heatmap: std::collections::HashMap<
            String,
            serde_json::Map<String, serde_json::Value>,
        > = Default::default();
        for r in rows {
            let (mt, month, cnt) = r?;
            heatmap.entry(mt).or_default().insert(month, cnt.into());
        }
        Ok(serde_json::to_value(heatmap)?)
    }

    // -----------------------------------------------------------------------
    // Episodes (table already in SCHEMA_SQL)
    // -----------------------------------------------------------------------

    pub async fn create_episode(
        &self,
        id: &str,
        title: Option<&str>,
        agent_id: Option<&str>,
        thread_id: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO episodes (id, title, agent_id, thread_id, started_at, memory_ids, metadata) \
             VALUES (?1, ?2, ?3, ?4, ?5, '[]', 'null')",
            params![id, title, agent_id, thread_id, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub async fn add_episode_step(
        &self,
        episode_id: &str,
        step_index: i64,
        description: &str,
        memory_id: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT OR REPLACE INTO episode_steps \
             (episode_id, step_index, description, memory_id, timestamp) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![episode_id, step_index, description, memory_id, Utc::now().to_rfc3339()],
        )?;
        if let Some(mid) = memory_id {
            conn.execute(
                "UPDATE episodes SET memory_ids = json_insert(memory_ids, '$[#]', ?1) WHERE id = ?2",
                params![mid, episode_id],
            )?;
        }
        Ok(())
    }

    pub async fn end_episode(&self, episode_id: &str, summary: Option<&str>) -> Result<bool> {
        let conn = self.conn.lock().await;
        Ok(conn.execute(
            "UPDATE episodes SET ended_at=?1, summary=?2 WHERE id=?3 AND ended_at IS NULL",
            params![Utc::now().to_rfc3339(), summary, episode_id],
        )? > 0)
    }

    /// Auto-close open episodes older than `max_age_hours` (C-RS-004). Mirrors
    /// Python's pre-phase dream cleanup: a stale open episode gets `ended_at`
    /// stamped now plus placeholder title/summary if unset. Returns the count
    /// closed.
    pub async fn close_stale_episodes(&self, max_age_hours: i64) -> Result<usize> {
        let cutoff = (Utc::now() - chrono::Duration::hours(max_age_hours)).to_rfc3339();
        let conn = self.conn.lock().await;
        let n = conn.execute(
            "UPDATE episodes \
             SET ended_at = ?1, \
                 title    = COALESCE(title, '(auto-closed)'), \
                 summary  = COALESCE(summary, 'auto-closed: stale open episode') \
             WHERE ended_at IS NULL AND started_at < ?2",
            params![Utc::now().to_rfc3339(), cutoff],
        )?;
        Ok(n)
    }

    pub async fn get_episode_raw(&self, episode_id: &str) -> Result<Option<serde_json::Value>> {
        let conn = self.conn.lock().await;
        let ep = conn.query_row(
            "SELECT id, title, agent_id, thread_id, started_at, ended_at, summary, memory_ids, metadata \
             FROM episodes WHERE id = ?",
            params![episode_id],
            |r| Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, String>(7)?,
                r.get::<_, String>(8)?,
            )),
        );
        match ep {
            Ok((id, title, aid, tid, started_at, ended_at, summary, mem_ids_str, meta_str)) => {
                let memory_ids: serde_json::Value =
                    serde_json::from_str(&mem_ids_str).unwrap_or(serde_json::json!([]));
                let metadata: serde_json::Value =
                    serde_json::from_str(&meta_str).unwrap_or(serde_json::Value::Null);
                let mut stmt = conn.prepare(
                    "SELECT step_index, description, memory_id, timestamp \
                     FROM episode_steps WHERE episode_id = ? ORDER BY step_index"
                )?;
                let step_rows = stmt.query_map(params![&id], |r| Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, String>(3)?,
                )))?;
                let mut steps = Vec::new();
                for sr in step_rows {
                    let (idx, desc, mid, ts) = sr?;
                    steps.push(serde_json::json!({
                        "step_index": idx, "description": desc,
                        "memory_id": mid, "timestamp": ts,
                    }));
                }
                Ok(Some(serde_json::json!({
                    "id": id, "title": title, "agent_id": aid, "thread_id": tid,
                    "started_at": started_at, "ended_at": ended_at, "summary": summary,
                    "memory_ids": memory_ids, "metadata": metadata, "steps": steps,
                })))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn list_episodes(
        &self,
        agent_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<serde_json::Value>> {
        let conn = self.conn.lock().await;
        let limit_val = limit as i64;
        let mut episodes = Vec::new();
        let row_to_ep = |r: &rusqlite::Row<'_>| -> rusqlite::Result<serde_json::Value> {
            Ok(serde_json::json!({
                "id":         r.get::<_, String>(0)?,
                "title":      r.get::<_, Option<String>>(1)?,
                "agent_id":   r.get::<_, Option<String>>(2)?,
                "thread_id":  r.get::<_, Option<String>>(3)?,
                "started_at": r.get::<_, String>(4)?,
                "ended_at":   r.get::<_, Option<String>>(5)?,
                "summary":    r.get::<_, Option<String>>(6)?,
            }))
        };
        if let Some(aid) = agent_id {
            let aid_s = aid.to_string();
            let mut stmt = conn.prepare(
                "SELECT id, title, agent_id, thread_id, started_at, ended_at, summary \
                 FROM episodes WHERE agent_id = ?1 ORDER BY started_at DESC LIMIT ?2"
            )?;
            let rows = stmt.query_map(params![aid_s, limit_val], row_to_ep)?;
            for r in rows { episodes.push(r?); }
        } else {
            let mut stmt = conn.prepare(
                "SELECT id, title, agent_id, thread_id, started_at, ended_at, summary \
                 FROM episodes ORDER BY started_at DESC LIMIT ?1"
            )?;
            let rows = stmt.query_map(params![limit_val], row_to_ep)?;
            for r in rows { episodes.push(r?); }
        }
        Ok(episodes)
    }

    pub async fn get_episode_memory_ids(&self, episode_id: &str) -> Result<Vec<MemoryId>> {
        let conn = self.conn.lock().await;
        let mem_ids_str: String = conn.query_row(
            "SELECT memory_ids FROM episodes WHERE id = ?",
            params![episode_id],
            |r| r.get(0),
        ).unwrap_or_else(|_| "[]".to_string());
        let mut ids: Vec<String> = serde_json::from_str(&mem_ids_str).unwrap_or_default();
        let mut stmt = conn.prepare(
            "SELECT memory_id FROM episode_steps \
             WHERE episode_id = ? AND memory_id IS NOT NULL"
        )?;
        let step_ids = stmt.query_map(params![episode_id], |r| r.get::<_, String>(0))?;
        for r in step_ids { ids.push(r?); }
        let mut seen = std::collections::HashSet::new();
        Ok(ids.into_iter().filter(|id| seen.insert(id.clone())).map(MemoryId).collect())
    }

    // -----------------------------------------------------------------------
    // Audit log (table already in SCHEMA_SQL)
    // -----------------------------------------------------------------------

    pub async fn log_audit_event(
        &self,
        agent_id: Option<&str>,
        action: &str,
        memory_id: Option<&str>,
        details: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO audit_log (timestamp, agent_id, action, memory_id, details) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![Utc::now().to_rfc3339(), agent_id, action, memory_id, details],
        )?;
        Ok(())
    }

    pub async fn query_audit(
        &self,
        limit: usize,
        agent_id_filter: Option<&str>,
    ) -> Result<Vec<serde_json::Value>> {
        let conn = self.conn.lock().await;
        let limit_val = limit as i64;
        let row_to_entry = |r: &rusqlite::Row<'_>| -> rusqlite::Result<serde_json::Value> {
            Ok(serde_json::json!({
                "id":        r.get::<_, i64>(0)?,
                "timestamp": r.get::<_, String>(1)?,
                "agent_id":  r.get::<_, Option<String>>(2)?,
                "action":    r.get::<_, String>(3)?,
                "memory_id": r.get::<_, Option<String>>(4)?,
                "details":   r.get::<_, Option<String>>(5)?,
            }))
        };
        let mut out = Vec::new();
        if let Some(aid) = agent_id_filter {
            let aid_s = aid.to_string();
            let mut stmt = conn.prepare(
                "SELECT id, timestamp, agent_id, action, memory_id, details \
                 FROM audit_log WHERE agent_id = ?1 ORDER BY timestamp DESC LIMIT ?2"
            )?;
            let rows = stmt.query_map(params![aid_s, limit_val], row_to_entry)?;
            for r in rows { out.push(r?); }
        } else {
            let mut stmt = conn.prepare(
                "SELECT id, timestamp, agent_id, action, memory_id, details \
                 FROM audit_log ORDER BY timestamp DESC LIMIT ?1"
            )?;
            let rows = stmt.query_map(params![limit_val], row_to_entry)?;
            for r in rows { out.push(r?); }
        }
        Ok(out)
    }

    pub async fn audit_summary(&self) -> Result<serde_json::Value> {
        let conn = self.conn.lock().await;
        let total: i64 =
            conn.query_row("SELECT COUNT(*) FROM audit_log", [], |r| r.get(0))?;
        let mut stmt = conn.prepare(
            "SELECT action, COUNT(*) FROM audit_log GROUP BY action ORDER BY COUNT(*) DESC"
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        let mut by_action = serde_json::Map::new();
        for r in rows { let (a, cnt) = r?; by_action.insert(a, cnt.into()); }
        Ok(serde_json::json!({ "total_events": total, "by_action": by_action }))
    }

    // -----------------------------------------------------------------------
    // Memory versions — snapshot content before each update
    // -----------------------------------------------------------------------

    pub async fn log_memory_version(
        &self,
        node: &MemoryNode,
        edited_by: Option<&str>,
        change_note: Option<&str>,
    ) -> Result<i64> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO memory_versions \
             (memory_id, content, tags_json, salience, visibility, edited_by, edited_at, change_note) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                node.id.0,
                node.content,
                serde_json::to_string(&node.tags)?,
                node.salience as f64,
                enum_to_str(&node.visibility)?,
                edited_by,
                Utc::now().to_rfc3339(),
                change_note,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub async fn get_memory_versions_raw(
        &self,
        memory_id: &str,
        limit: usize,
    ) -> Result<Vec<serde_json::Value>> {
        let conn = self.conn.lock().await;
        let limit_val = limit as i64;
        let mut stmt = conn.prepare(
            "SELECT id, memory_id, content, tags_json, salience, visibility, \
             edited_by, edited_at, change_note \
             FROM memory_versions WHERE memory_id = ? ORDER BY edited_at DESC LIMIT ?"
        )?;
        let rows = stmt.query_map(params![memory_id, limit_val], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, f64>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, String>(7)?,
                r.get::<_, Option<String>>(8)?,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            let (id, mid, content, tags_json, salience, visibility, edited_by, edited_at, change_note) = r?;
            out.push(serde_json::json!({
                "id":          id,
                "memory_id":   mid,
                "content":     content,
                "tags_json":   tags_json,
                "salience":    salience,
                "visibility":  visibility,
                "edited_by":   edited_by,
                "edited_at":   edited_at,
                "change_note": change_note,
            }));
        }
        Ok(out)
    }

    pub async fn get_version_raw(&self, version_id: i64) -> Result<Option<serde_json::Value>> {
        let conn = self.conn.lock().await;
        let result = conn.query_row(
            "SELECT id, memory_id, content, tags_json, salience, visibility, \
             edited_by, edited_at, change_note \
             FROM memory_versions WHERE id = ?",
            params![version_id],
            |r| Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, f64>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, String>(7)?,
                r.get::<_, Option<String>>(8)?,
            )),
        );
        match result {
            Ok((id, mid, content, tags_json, salience, visibility, edited_by, edited_at, change_note)) =>
                Ok(Some(serde_json::json!({
                    "id":          id,
                    "memory_id":   mid,
                    "content":     content,
                    "tags_json":   tags_json,
                    "salience":    salience,
                    "visibility":  visibility,
                    "edited_by":   edited_by,
                    "edited_at":   edited_at,
                    "change_note": change_note,
                }))),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn has_link_between(&self, a: &MemoryId, b: &MemoryId) -> Result<bool> {
        let conn = self.conn.lock().await;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM links \
             WHERE (source_id = ?1 AND target_id = ?2) \
                OR (source_id = ?2 AND target_id = ?1)",
            params![a.0, b.0],
            |r| r.get(0),
        )?;
        Ok(count > 0)
    }

    pub async fn save_dream_report(
        &self,
        id:       &str,
        agent_id: Option<&str>,
        report:   &crate::engines::dream::DreamReport,
    ) -> Result<()> {
        // save happens at the END of the cycle, so now == ended_at; reconstruct
        // started_at from the measured duration (was binding both to ?3 == now, so
        // ended_at always equalled started_at and the span was unrecoverable).
        let ended = chrono::Utc::now();
        let started = ended
            - chrono::Duration::milliseconds((report.total_duration_secs * 1000.0) as i64);
        let started_at = started.to_rfc3339();
        let ended_at   = ended.to_rfc3339();
        let phases_json = serde_json::to_string(&report.phases)?;
        let metadata_json = serde_json::to_string(&serde_json::json!({
            "total_llm_calls":     report.total_llm_calls,
            "total_duration_secs": report.total_duration_secs,
            "success":             report.success,
        }))?;
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT OR REPLACE INTO dream_reports \
             (id, agent_id, started_at, ended_at, phases, metadata) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, agent_id, started_at, ended_at, phases_json, metadata_json],
        )?;
        Ok(())
    }

    pub async fn get_last_dream_report(&self) -> Result<Option<serde_json::Value>> {
        let conn = self.conn.lock().await;
        let result = conn.query_row(
            "SELECT id, agent_id, started_at, ended_at, phases, metadata \
             FROM dream_reports ORDER BY started_at DESC LIMIT 1",
            [],
            |r| Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
            )),
        );
        match result {
            Ok((id, agent_id, started_at, ended_at, phases_json, metadata_json)) => {
                let phases = serde_json::from_str::<serde_json::Value>(&phases_json)
                    .unwrap_or(serde_json::Value::Array(vec![]));
                let metadata = serde_json::from_str::<serde_json::Value>(&metadata_json)
                    .unwrap_or(serde_json::Value::Null);
                Ok(Some(serde_json::json!({
                    "id":         id,
                    "agent_id":   agent_id,
                    "started_at": started_at,
                    "ended_at":   ended_at,
                    "phases":     phases,
                    "metadata":   metadata,
                })))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS memories (
    id                  TEXT PRIMARY KEY,
    content             TEXT NOT NULL,
    memory_type         TEXT NOT NULL,
    layer               TEXT NOT NULL DEFAULT 'working',
    salience            REAL NOT NULL DEFAULT 0.5,
    tags                TEXT NOT NULL DEFAULT '[]',
    agent_id            TEXT,
    visibility          TEXT NOT NULL DEFAULT 'shared',
    thread_id           TEXT,
    emotional_valence   TEXT,
    emotional_intensity REAL NOT NULL DEFAULT 0.0,
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL,
    access_count        INTEGER NOT NULL DEFAULT 0,
    access_times        TEXT NOT NULL DEFAULT '[]',
    fsrs_stability      REAL NOT NULL DEFAULT 1.0,
    fsrs_difficulty     REAL NOT NULL DEFAULT 5.0,
    fsrs_last_review    TEXT,
    metadata            TEXT NOT NULL DEFAULT 'null',
    embedding           BLOB,
    deleted_at          TEXT
);

CREATE TABLE IF NOT EXISTS links (
    source_id           TEXT NOT NULL,
    target_id           TEXT NOT NULL,
    link_type           TEXT NOT NULL,
    weight              REAL NOT NULL DEFAULT 0.5,
    created_at          TEXT NOT NULL,
    last_traversed      TEXT,
    traversal_count     INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (source_id, target_id, link_type),
    FOREIGN KEY (source_id) REFERENCES memories(id),
    FOREIGN KEY (target_id) REFERENCES memories(id)
);

CREATE TABLE IF NOT EXISTS agents (
    id              TEXT PRIMARY KEY,
    name            TEXT NOT NULL UNIQUE,
    description     TEXT,
    registered_at   TEXT NOT NULL,
    last_seen       TEXT,
    metadata        TEXT NOT NULL DEFAULT 'null'
);

CREATE TABLE IF NOT EXISTS episodes (
    id          TEXT PRIMARY KEY,
    title       TEXT,
    agent_id    TEXT,
    thread_id   TEXT,
    started_at  TEXT NOT NULL,
    ended_at    TEXT,
    summary     TEXT,
    memory_ids  TEXT NOT NULL DEFAULT '[]',
    metadata    TEXT NOT NULL DEFAULT 'null'
);

CREATE TABLE IF NOT EXISTS episode_steps (
    episode_id  TEXT NOT NULL,
    step_index  INTEGER NOT NULL,
    description TEXT NOT NULL,
    memory_id   TEXT,
    timestamp   TEXT NOT NULL,
    PRIMARY KEY (episode_id, step_index),
    FOREIGN KEY (episode_id) REFERENCES episodes(id)
);

CREATE TABLE IF NOT EXISTS tags (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    name        TEXT NOT NULL UNIQUE,
    created_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS memory_tags (
    memory_id   TEXT NOT NULL,
    tag_id      INTEGER NOT NULL,
    PRIMARY KEY (memory_id, tag_id),
    FOREIGN KEY (memory_id) REFERENCES memories(id),
    FOREIGN KEY (tag_id)    REFERENCES tags(id)
);

CREATE TABLE IF NOT EXISTS audit_log (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp   TEXT NOT NULL,
    agent_id    TEXT,
    action      TEXT NOT NULL,
    memory_id   TEXT,
    details     TEXT
);

CREATE TABLE IF NOT EXISTS dream_reports (
    id          TEXT PRIMARY KEY,
    agent_id    TEXT,
    started_at  TEXT NOT NULL,
    ended_at    TEXT,
    phases      TEXT NOT NULL DEFAULT '[]',
    metadata    TEXT NOT NULL DEFAULT 'null'
);

CREATE TABLE IF NOT EXISTS memory_versions (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    memory_id   TEXT NOT NULL,
    content     TEXT NOT NULL,
    tags_json   TEXT NOT NULL DEFAULT '[]',
    salience    REAL NOT NULL,
    visibility  TEXT NOT NULL,
    edited_by   TEXT,
    edited_at   TEXT NOT NULL,
    change_note TEXT,
    FOREIGN KEY (memory_id) REFERENCES memories(id)
);

CREATE INDEX IF NOT EXISTS idx_versions_memory ON memory_versions(memory_id);

-- CLIP image embeddings for visual recall (search_vision). 512-dim f32 LE blob,
-- keyed by the caption memory it indexes; image_path lets a hit return the source
-- image for re-viewing. Brute-force cosine in Rust (image counts are modest), so
-- no vec0 virtual table — a plain row store. Scope is enforced via the memories
-- join at search time, not denormalized here.
CREATE TABLE IF NOT EXISTS vision_embeddings (
    memory_id   TEXT PRIMARY KEY,
    embedding   BLOB NOT NULL,
    image_path  TEXT,
    created_at  TEXT NOT NULL,
    FOREIGN KEY (memory_id) REFERENCES memories(id)
);

-- FTS5 virtual table for keyword search (FTS5 fallback when vector search unavailable)
CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
    id UNINDEXED,
    content,
    tags,
    content='memories',
    content_rowid='rowid'
);

CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
    INSERT INTO memories_fts(rowid, id, content, tags)
    VALUES (new.rowid, new.id, new.content, new.tags);
END;

-- CB-020: only evict from FTS if the row was still indexed. A soft-deleted row
-- (deleted_at NOT NULL) was already removed from FTS by memories_au, so issuing
-- a second 'delete' against an absent row corrupts the FTS index.
CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, id, content, tags)
    SELECT 'delete', old.rowid, old.id, old.content, old.tags
    WHERE old.deleted_at IS NULL;
END;

-- CB-020: re-insert into FTS only for a LIVE row. A soft-delete is an UPDATE
-- setting deleted_at; without this guard the row would be re-indexed and the
-- FTS5 index would never shrink. Restoring (deleted_at -> NULL) re-inserts it.
CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE ON memories BEGIN
    -- Only evict the OLD image if it was actually indexed (was live). A row that
    -- was already soft-deleted is absent from FTS; deleting it again corrupts the
    -- index. This makes restore (NOT NULL -> NULL) a clean re-insert.
    INSERT INTO memories_fts(memories_fts, rowid, id, content, tags)
    SELECT 'delete', old.rowid, old.id, old.content, old.tags
    WHERE old.deleted_at IS NULL;
    INSERT INTO memories_fts(rowid, id, content, tags)
    SELECT new.rowid, new.id, new.content, new.tags
    WHERE new.deleted_at IS NULL;
END;

-- Indices for common query patterns
CREATE INDEX IF NOT EXISTS idx_memories_agent    ON memories(agent_id);
CREATE INDEX IF NOT EXISTS idx_memories_type     ON memories(memory_type);
CREATE INDEX IF NOT EXISTS idx_memories_layer    ON memories(layer);
CREATE INDEX IF NOT EXISTS idx_memories_vis      ON memories(visibility);
CREATE INDEX IF NOT EXISTS idx_memories_thread   ON memories(thread_id);
CREATE INDEX IF NOT EXISTS idx_memories_deleted  ON memories(deleted_at);
CREATE INDEX IF NOT EXISTS idx_links_source      ON links(source_id);
CREATE INDEX IF NOT EXISTS idx_links_target      ON links(target_id);
CREATE INDEX IF NOT EXISTS idx_audit_ts          ON audit_log(timestamp);
"#;
