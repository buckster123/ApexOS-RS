use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::types::{AgentId, MemoryId};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Episode {
    pub id:          String,
    pub title:       Option<String>,
    pub agent_id:    Option<AgentId>,
    pub thread_id:   Option<String>,
    pub started_at:  DateTime<Utc>,
    pub ended_at:    Option<DateTime<Utc>>,
    pub summary:     Option<String>,
    pub steps:       Vec<EpisodeStep>,
    pub memory_ids:  Vec<MemoryId>,
    pub metadata:    serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EpisodeStep {
    pub step_index:  u32,
    pub description: String,
    pub memory_id:   Option<MemoryId>,
    pub timestamp:   DateTime<Utc>,
}

impl Episode {
    pub fn new(agent_id: Option<AgentId>) -> Self {
        Self {
            id:         uuid::Uuid::new_v4().to_string(),
            title:      None,
            agent_id,
            thread_id:  None,
            started_at: Utc::now(),
            ended_at:   None,
            summary:    None,
            steps:      vec![],
            memory_ids: vec![],
            metadata:   serde_json::Value::Null,
        }
    }
}
