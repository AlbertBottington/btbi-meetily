//! Per-recording transcript collection. Late results from a timed-out worker
//! cannot contaminate the next recording's upload package.
#[derive(Default)]
pub struct TranscriptCollector {
    generation: u64,
    pub failed: bool,
    pub segments: Vec<serde_json::Value>,
}
impl TranscriptCollector {
    pub const fn new() -> Self {
        Self {
            generation: 0,
            failed: false,
            segments: Vec::new(),
        }
    }
    pub fn begin(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.failed = false;
        self.segments.clear();
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn fail(&mut self, generation: u64) {
        if generation == self.generation {
            self.failed = true;
        }
    }
    pub fn push(&mut self, generation: u64, value: serde_json::Value) -> bool {
        if generation != self.generation {
            return false;
        }
        self.segments.push(value);
        true
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn late_worker_cannot_change_next_recording() {
        let mut c = TranscriptCollector::new();
        c.begin();
        let old = c.generation();
        assert!(c.push(old, serde_json::json!("first")));
        c.fail(old);
        assert!(c.failed);
        c.begin();
        assert!(!c.push(old, serde_json::json!("late")));
        c.fail(old);
        assert!(!c.failed);
        assert!(c.segments.is_empty());
        assert!(c.push(c.generation(), serde_json::json!("new")));
        assert_eq!(c.segments.len(), 1);
    }
}
