/// SchemaEngine — neocortex.
/// Extracts and manages schematic memories (abstract patterns).
/// Schema creation and promotion are wired to storage in cortex.rs (step 7).
/// Mirrors Python engines/neocortex.py SchemaEngine.
pub struct SchemaEngine;

impl Default for SchemaEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl SchemaEngine {
    pub fn new() -> Self { Self }

    /// Extract integer from a `"prefix:N"` tag. Returns `default` if not found.
    pub fn get_tag_int(tags: &[String], prefix: &str, default: i64) -> i64 {
        let key = format!("{prefix}:");
        for tag in tags {
            if let Some(rest) = tag.strip_prefix(&key) {
                if let Ok(n) = rest.parse::<i64>() {
                    return n;
                }
            }
        }
        default
    }

    /// Set a `"prefix:N"` tag, replacing any existing one with the same prefix.
    pub fn set_tag_int(tags: &[String], prefix: &str, value: i64) -> Vec<String> {
        let key = format!("{prefix}:");
        let mut new_tags: Vec<String> = tags.iter()
            .filter(|t| !t.starts_with(&key))
            .cloned()
            .collect();
        new_tags.push(format!("{prefix}:{value}"));
        new_tags
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_tag_int_present() {
        let tags = vec!["support_count:3".to_string(), "other:tag".to_string()];
        assert_eq!(SchemaEngine::get_tag_int(&tags, "support_count", 0), 3);
    }

    #[test]
    fn get_tag_int_missing_returns_default() {
        let tags = vec!["other:tag".to_string()];
        assert_eq!(SchemaEngine::get_tag_int(&tags, "support_count", 7), 7);
    }

    #[test]
    fn get_tag_int_empty_tags() {
        assert_eq!(SchemaEngine::get_tag_int(&[], "support_count", 0), 0);
    }

    #[test]
    fn set_tag_int_adds_new() {
        let tags: Vec<String> = vec!["existing:tag".to_string()];
        let result = SchemaEngine::set_tag_int(&tags, "support_count", 5);
        assert!(result.contains(&"support_count:5".to_string()));
        assert!(result.contains(&"existing:tag".to_string()));
    }

    #[test]
    fn set_tag_int_replaces_existing() {
        let tags = vec!["support_count:2".to_string(), "other:x".to_string()];
        let result = SchemaEngine::set_tag_int(&tags, "support_count", 9);
        assert!(result.contains(&"support_count:9".to_string()));
        assert!(!result.contains(&"support_count:2".to_string()), "old value should be gone");
        assert_eq!(result.iter().filter(|t| t.starts_with("support_count:")).count(), 1);
    }
}
