use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::types::AgentId;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub id:           AgentId,
    pub name:         String,
    pub description:  Option<String>,
    pub registered_at: DateTime<Utc>,
    pub last_seen:    Option<DateTime<Utc>>,
    pub metadata:     serde_json::Value,
}

impl Agent {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id:            AgentId::new(),
            name:          name.into(),
            description:   None,
            registered_at: Utc::now(),
            last_seen:     None,
            metadata:      serde_json::Value::Null,
        }
    }
}
