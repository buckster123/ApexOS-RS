use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::types::{AgentId, EmotionalValence, MemoryId, MemoryLayer, MemoryType, Visibility};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryNode {
    pub id:            MemoryId,
    pub content:       String,
    pub memory_type:   MemoryType,
    pub layer:         MemoryLayer,
    pub salience:      f32,
    pub tags:          Vec<String>,
    pub agent_id:      Option<AgentId>,
    pub visibility:    Visibility,
    pub thread_id:     Option<String>,
    pub emotional_valence: Option<EmotionalValence>,
    pub emotional_intensity: f32,
    pub created_at:    DateTime<Utc>,
    pub updated_at:    DateTime<Utc>,
    pub access_count:  u32,
    /// ACT-R timestamps — capped at MAX_STORED_TIMESTAMPS (50)
    pub access_times:  Vec<DateTime<Utc>>,
    pub strength:      StrengthState,
    pub metadata:      serde_json::Value,
}

impl MemoryNode {
    pub fn new(content: impl Into<String>, memory_type: MemoryType) -> Self {
        let now = Utc::now();
        Self {
            id:                  MemoryId::new(),
            content:             content.into(),
            memory_type,
            layer:               MemoryLayer::Working,
            salience:            0.5,
            tags:                vec![],
            agent_id:            None,
            visibility:          Visibility::Shared,
            thread_id:           None,
            emotional_valence:   None,
            emotional_intensity: 0.0,
            created_at:          now,
            updated_at:          now,
            access_count:        0,
            access_times:        vec![now],
            strength:            StrengthState::default(),
            metadata:            serde_json::Value::Null,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrengthState {
    pub stability:   f32,  // FSRS S parameter
    pub difficulty:  f32,  // FSRS D parameter
    pub last_review: Option<DateTime<Utc>>,
}

impl Default for StrengthState {
    fn default() -> Self {
        Self {
            stability:   crate::config::FSRS_INITIAL_STABILITY,
            difficulty:  crate::config::FSRS_INITIAL_DIFFICULTY,
            last_review: None,
        }
    }
}
