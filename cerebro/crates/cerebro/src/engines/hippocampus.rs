/// EpisodicEngine — hippocampus.
/// Manages episodic sequences: start, step, end, recall.
/// Episode CRUD is wired to storage in cortex.rs (step 7).
/// Mirrors Python engines/hippocampus.py EpisodicEngine.
pub struct EpisodicEngine;

impl Default for EpisodicEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl EpisodicEngine {
    pub fn new() -> Self { Self }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_constructs() {
        let _ = EpisodicEngine::new();
    }
}
