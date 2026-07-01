use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Newtype IDs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MemoryId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentId(pub String);

impl MemoryId {
    pub fn new() -> Self { Self(Uuid::new_v4().to_string()) }
}

impl AgentId {
    pub fn new() -> Self { Self(Uuid::new_v4().to_string()) }
}

impl Default for MemoryId { fn default() -> Self { Self::new() } }
impl Default for AgentId  { fn default() -> Self { Self::new() } }

// ---------------------------------------------------------------------------
// Core enums — mirrors types.py exactly
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryType {
    Episodic,
    Semantic,
    Procedural,
    Affective,
    Prospective,
    Schematic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkType {
    Temporal,
    Causal,
    Semantic,
    Affective,
    Contextual,
    Contradicts,
    Supports,
    DerivedFrom,
    PartOf,
}

impl LinkType {
    /// Spreading activation conductance weight per link type.
    /// Values mirror Python LINK_TYPE_WEIGHTS exactly.
    pub fn activation_weight(self) -> f32 {
        match self {
            Self::Causal      => 0.9,
            Self::Semantic    => 0.8,
            Self::Supports    => 0.8,
            Self::PartOf      => 0.8,
            Self::Contextual  => 0.7,
            Self::DerivedFrom => 0.7,
            Self::Temporal    => 0.6,
            Self::Affective   => 0.5,
            Self::Contradicts => 0.3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryLayer {
    Sensory,
    Working,
    LongTerm,
    Cortex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    Private,
    Shared,
    Thread,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmotionalValence {
    Positive,
    Negative,
    Neutral,
    Mixed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaType {
    Text,
    Image,
    Pdf,
    Audio,
    Video,
    Html,
    Csv,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DreamPhase {
    SwsReplay,
    PatternExtraction,
    SchemaFormation,
    EmotionalReprocessing,
    Pruning,
    RemRecombination,
}

// ---------------------------------------------------------------------------
// Visibility scoping — every SQL query and graph traversal carries this
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct VisibilityScope {
    pub agent_id: Option<AgentId>,
    /// Restrict to `visibility='shared'` memories ONLY — the scope for
    /// federation / public surfaces (e.g. a mesh peer querying this node).
    /// Strictly NARROWER than `global()` (the unrestricted admin view):
    /// private and thread memories never match, whoever owns them.
    pub shared_only: bool,
}

impl VisibilityScope {
    pub fn global() -> Self { Self { agent_id: None, shared_only: false } }
    pub fn for_agent(id: AgentId) -> Self { Self { agent_id: Some(id), shared_only: false } }
    /// The federation scope: only `visibility='shared'` memories are visible.
    pub fn shared_only() -> Self { Self { agent_id: None, shared_only: true } }

    pub fn can_access(&self, visibility: Visibility, node_agent_id: Option<&AgentId>) -> bool {
        if self.shared_only {
            return matches!(visibility, Visibility::Shared);
        }
        match visibility {
            Visibility::Shared  => true,
            Visibility::Private => self.agent_id.as_ref() == node_agent_id,
            Visibility::Thread  => true, // thread_id checked separately
        }
    }

    /// SQL fragment — mirrors Python _scope_sql()
    pub fn sql_filter(&self) -> (&'static str, Vec<String>) {
        if self.shared_only {
            return ("visibility='shared'", vec![]);
        }
        match &self.agent_id {
            None     => ("1=1", vec![]),
            Some(id) => (
                "(visibility='shared' OR (visibility='private' AND agent_id=?))",
                vec![id.0.clone()],
            ),
        }
    }
}
