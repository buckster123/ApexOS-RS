use std::{env, path::PathBuf};

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

pub fn data_dir() -> PathBuf {
    if let Ok(v) = env::var("CEREBRO_DATA_DIR") {
        PathBuf::from(v)
    } else {
        dirs_next::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(".cerebro-cortex")
    }
}

pub fn sqlite_db() -> PathBuf { data_dir().join("cerebro.db") }
pub fn export_dir() -> PathBuf { data_dir().join("exports") }

// ---------------------------------------------------------------------------
// Embedding
// ---------------------------------------------------------------------------
pub const EMBED_MODEL: &str = "BAAI/bge-small-en-v1.5"; // 384-dim, ~33MB
pub const EMBEDDING_DIM: usize = 384;

// ---------------------------------------------------------------------------
// ACT-R parameters — mirrors Python config.py exactly
// ---------------------------------------------------------------------------
pub const ACTR_DECAY_RATE: f32 = 0.5;
pub const ACTR_MIN_TIME_SECONDS: f32 = 1.0;
pub const ACTR_RETRIEVAL_THRESHOLD: f32 = 0.0;
pub const ACTR_NOISE: f32 = 0.4;
pub const MAX_STORED_TIMESTAMPS: usize = 50;

// ---------------------------------------------------------------------------
// FSRS parameters
// ---------------------------------------------------------------------------
pub const FSRS_INITIAL_STABILITY: f32 = 1.0;
pub const FSRS_INITIAL_DIFFICULTY: f32 = 5.0;
pub const FSRS_MIN_STABILITY: f32 = 0.1;
pub const FSRS_MAX_STABILITY: f32 = 365.0;

// ---------------------------------------------------------------------------
// Recall scoring weights
// ---------------------------------------------------------------------------
pub const SCORE_WEIGHT_VECTOR: f32       = 0.35;
pub const SCORE_WEIGHT_ACTIVATION: f32   = 0.30;
pub const SCORE_WEIGHT_RETRIEVABILITY: f32 = 0.20;
pub const SCORE_WEIGHT_SALIENCE: f32     = 0.15;

// ---------------------------------------------------------------------------
// Spreading activation
// ---------------------------------------------------------------------------
pub const SPREADING_MAX_HOPS: u8         = 2;
pub const SPREADING_DECAY_PER_HOP: f32   = 0.6;
pub const SPREADING_ACTIVATION_THRESHOLD: f32 = 0.05;
pub const SPREADING_MAX_ACTIVATED: usize = 50;
pub const LINK_DECAY_HALFLIFE_DAYS: f32  = 30.0;

// ---------------------------------------------------------------------------
// Dream engine
// ---------------------------------------------------------------------------
pub const DREAM_MAX_LLM_CALLS: usize = 20;

// ---------------------------------------------------------------------------
// Exo-evolution — competence selection (docs/evolutionary-layer.md)
// ---------------------------------------------------------------------------
/// A procedure whose salience decays to (or below) this floor is tagged
/// `prune_candidate` — selection pressure made concrete, so dream's pruning
/// phase can retire it. Reached by repeated failure (`record_procedure_outcome`,
/// −0.15 each) or by repeatedly losing a niche competition (the new
/// `skill_competition` dream phase). Shared by `cerebro-mcp`'s dispatch and the
/// dream engine so the demote floor can't drift between the two writers.
pub const PRUNE_CANDIDATE_SALIENCE: f32 = 0.25;

// ---------------------------------------------------------------------------
// Runtime config (from env)
// ---------------------------------------------------------------------------
#[derive(Debug, Clone)]
pub struct Config {
    pub db_path:       PathBuf,
    pub anthropic_key: Option<String>,
    pub embed_model:   String,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            db_path:       sqlite_db(),
            anthropic_key: env::var("ANTHROPIC_API_KEY").ok(),
            embed_model:   env::var("CEREBRO_EMBED_MODEL")
                               .unwrap_or_else(|_| EMBED_MODEL.to_string()),
        })
    }
}
