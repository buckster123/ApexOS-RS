use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::types::{LinkType, MemoryId};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssociativeLink {
    pub source_id:       MemoryId,
    pub target_id:       MemoryId,
    pub link_type:       LinkType,
    /// Base weight [0, 1] — Hebbian strengthening writes here; decay is computed on-the-fly.
    pub weight:          f32,
    pub created_at:      DateTime<Utc>,
    pub last_traversed:  Option<DateTime<Utc>>,
    pub traversal_count: u32,
}

impl AssociativeLink {
    pub fn new(source_id: MemoryId, target_id: MemoryId, link_type: LinkType, weight: f32) -> Self {
        Self {
            source_id,
            target_id,
            link_type,
            weight: weight.clamp(0.0, 1.0),
            created_at: Utc::now(),
            last_traversed: None,
            traversal_count: 0,
        }
    }

    /// Effective weight accounting for link age — FSRS-style power-law decay:
    ///   w_eff = w × (1 + age_days / (9 × H))^{-1}
    ///
    /// Same formula as Python `effective_link_weight()` in `activation/spreading.py`.
    /// H = halflife_days (default 30). At 9H days without traversal, effective weight = 0.5 × stored.
    /// If `last_traversed` is None, uses `created_at` as the reference point.
    pub fn effective_weight(&self, now: DateTime<Utc>, halflife_days: f32) -> f32 {
        let reference = self.last_traversed.unwrap_or(self.created_at);
        let age_days = (now - reference).num_seconds().max(0) as f32 / 86400.0;
        if age_days <= 0.0 || halflife_days <= 0.0 {
            return self.weight;
        }
        let decay = (1.0 + age_days / (9.0 * halflife_days)).powi(-1);
        self.weight * decay
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{LinkType, MemoryId};
    use chrono::Duration;

    fn make_link() -> AssociativeLink {
        AssociativeLink::new(
            MemoryId("a".into()),
            MemoryId("b".into()),
            LinkType::Semantic,
            1.0,
        )
    }

    #[test]
    fn brand_new_link_full_weight() {
        let now = Utc::now();
        let mut link = make_link();
        link.created_at = now;
        let eff = link.effective_weight(now, 30.0);
        assert!((eff - 1.0).abs() < 1e-6, "got {eff}");
    }

    #[test]
    fn at_nine_halflives_is_half() {
        // age = 9 × H → decay = (1 + 1)^{-1} = 0.5
        let now = Utc::now();
        let mut link = make_link();
        link.created_at = now - Duration::days(270); // 9 × 30 days
        let eff = link.effective_weight(now, 30.0);
        assert!((eff - 0.5).abs() < 1e-5, "got {eff}");
    }

    #[test]
    fn at_one_halflife_is_ninety_pct() {
        // age = 30, H = 30 → decay = (1 + 30/270)^{-1} = (10/9)^{-1} = 0.9
        let now = Utc::now();
        let mut link = make_link();
        link.created_at = now - Duration::days(30);
        let eff = link.effective_weight(now, 30.0);
        assert!((eff - 0.9).abs() < 1e-5, "got {eff}");
    }

    #[test]
    fn uses_last_traversed_not_created_at() {
        let now = Utc::now();
        let mut link = make_link();
        link.created_at    = now - Duration::days(270); // very old
        link.last_traversed = Some(now);                // but just traversed
        let eff = link.effective_weight(now, 30.0);
        assert!((eff - 1.0).abs() < 1e-6, "just traversed should be ~1.0, got {eff}");
    }
}
