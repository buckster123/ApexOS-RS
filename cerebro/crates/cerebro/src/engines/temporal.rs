use std::collections::HashMap;

use crate::models::MemoryNode;

/// SemanticEngine — temporal cortex.
/// Extracts concepts from text and creates semantic links between related knowledge.
/// Mirrors Python engines/temporal.py SemanticEngine.
pub struct SemanticEngine;

impl Default for SemanticEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl SemanticEngine {
    pub fn new() -> Self { Self }

    /// Extract key concepts from text content.
    ///
    /// Tokenizes with a `[a-z][a-z0-9_-]+` equivalent filter, removes stop words,
    /// counts word frequencies. Also extracts capitalized word bigrams from the
    /// original content (boosted ×2). Returns top `max_concepts` by frequency.
    pub fn extract_concepts(&self, content: &str, max_concepts: usize) -> Vec<String> {
        let lower = content.to_lowercase();
        let mut freq: HashMap<String, u32> = HashMap::new();

        // Unigrams
        for word in tokenize_words(&lower) {
            if word.len() > 2 && !is_stop_word(&word) {
                *freq.entry(word).or_insert(0) += 1;
            }
        }

        // Bigrams of consecutive Title-case words in original content
        let orig_words: Vec<&str> = content.split_whitespace().collect();
        for pair in orig_words.windows(2) {
            let a = strip_punctuation(pair[0]);
            let b = strip_punctuation(pair[1]);
            if is_title_word(a) && is_title_word(b) {
                let bigram = format!("{} {}", a.to_lowercase(), b.to_lowercase());
                *freq.entry(bigram).or_insert(0) += 2;
            }
        }

        let mut sorted: Vec<(String, u32)> = freq.into_iter().collect();
        // Sort by frequency desc, then alphabetically for determinism
        sorted.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        sorted.into_iter().take(max_concepts).map(|(w, _)| w).collect()
    }

    /// Enrich a node with extracted concepts stored in metadata.
    ///
    /// Only extracts if `node.metadata["concepts"]` is absent or null.
    pub fn enrich_node(&self, mut node: MemoryNode) -> MemoryNode {
        let already_set = node.metadata.get("concepts")
            .map(|v| !v.is_null())
            .unwrap_or(false);
        if already_set {
            return node;
        }
        let concepts = self.extract_concepts(&node.content, 10);
        // Ensure metadata is a JSON object before inserting
        if !node.metadata.is_object() {
            node.metadata = serde_json::Value::Object(serde_json::Map::new());
        }
        if let Some(map) = node.metadata.as_object_mut() {
            map.insert("concepts".into(), serde_json::json!(concepts));
        }
        node
    }
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Tokenize lowercase text into word tokens matching `[a-z][a-z0-9_-]+`.
fn tokenize_words(lower: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut start: Option<usize> = None;
    let bytes = lower.as_bytes();

    for (i, &b) in bytes.iter().enumerate() {
        let in_word = match start {
            None    => b.is_ascii_lowercase(),
            Some(_) => b.is_ascii_alphanumeric() || b == b'_' || b == b'-',
        };
        if in_word {
            if start.is_none() { start = Some(i); }
        } else if let Some(s) = start.take() {
            words.push(lower[s..i].to_string());
        }
    }
    if let Some(s) = start {
        words.push(lower[s..].to_string());
    }
    words
}

/// Strip trailing non-alphanumeric characters (punctuation after words).
fn strip_punctuation(s: &str) -> &str {
    s.trim_end_matches(|c: char| !c.is_alphanumeric())
}

/// True if the word starts with an uppercase letter followed by lowercase letters.
fn is_title_word(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.is_ascii_uppercase() && chars.all(|c| c.is_ascii_lowercase()),
        None => false,
    }
}

fn is_stop_word(w: &str) -> bool {
    matches!(w,
        "the"|"a"|"an"|"is"|"are"|"was"|"were"|"be"|"been"|"being"|
        "have"|"has"|"had"|"do"|"does"|"did"|"will"|"would"|"could"|
        "should"|"may"|"might"|"shall"|"can"|"need"|"dare"|"ought"|
        "used"|"to"|"of"|"in"|"for"|"on"|"with"|"at"|"by"|"from"|
        "as"|"into"|"through"|"during"|"before"|"after"|"above"|
        "below"|"between"|"out"|"off"|"over"|"under"|"again"|"further"|
        "then"|"once"|"here"|"there"|"when"|"where"|"why"|"how"|"all"|
        "each"|"every"|"both"|"few"|"more"|"most"|"other"|"some"|
        "such"|"no"|"nor"|"not"|"only"|"own"|"same"|"so"|"than"|
        "too"|"very"|"just"|"because"|"but"|"and"|"or"|"if"|"while"|
        "that"|"this"|"these"|"those"|"it"|"its"|"i"|"you"|"he"|
        "she"|"we"|"they"|"me"|"him"|"her"|"us"|"them"|"my"|"your"|
        "his"|"our"|"their"|"what"|"which"|"who"|"whom"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::MemoryType;

    fn engine() -> SemanticEngine { SemanticEngine::new() }

    #[test]
    fn extract_concepts_filters_stop_words() {
        let concepts = engine().extract_concepts("the quick brown fox jumps over the lazy dog", 10);
        assert!(!concepts.contains(&"the".to_string()), "stop word should be filtered");
        assert!(!concepts.contains(&"over".to_string()), "stop word should be filtered");
        assert!(concepts.contains(&"quick".to_string()) || concepts.contains(&"brown".to_string()));
    }

    #[test]
    fn extract_concepts_short_words_excluded() {
        // "a", "is" are stop words; "go", "do" are 2 chars — excluded by len > 2
        let concepts = engine().extract_concepts("go do the work is it", 10);
        assert!(!concepts.iter().any(|c| c.len() <= 2));
    }

    #[test]
    fn extract_concepts_respects_max() {
        let content = "rust memory graph storage sqlite vector engine thalamus amygdala temporal";
        let concepts = engine().extract_concepts(content, 3);
        assert!(concepts.len() <= 3);
    }

    #[test]
    fn extract_concepts_bigrams_get_boost() {
        // "Rust Memory" is a bigram of title-case words — should appear boosted
        let concepts = engine().extract_concepts("Rust Memory is the best Rust Memory system", 10);
        assert!(concepts.contains(&"rust memory".to_string()), "bigram should be extracted");
    }

    #[test]
    fn enrich_node_adds_concepts_when_empty() {
        let node = MemoryNode::new("sqlite vector storage engine for rust", MemoryType::Semantic);
        let enriched = engine().enrich_node(node);
        let concepts = enriched.metadata["concepts"].as_array().expect("concepts array");
        assert!(!concepts.is_empty(), "concepts should be added");
    }

    #[test]
    fn enrich_node_skips_when_already_set() {
        let mut node = MemoryNode::new("sqlite vector storage engine for rust", MemoryType::Semantic);
        node.metadata = serde_json::json!({"concepts": ["already", "set"]});
        let enriched = engine().enrich_node(node);
        let concepts = enriched.metadata["concepts"].as_array().unwrap();
        // Should still be the original ["already", "set"]
        assert_eq!(concepts.len(), 2);
    }
}
