//! Repeated-tool-action detection.
//!
//! The detector is deliberately narrow: only tool actions participate. A final
//! answer ends the run, so it also clears the observation window.

use std::collections::VecDeque;

use serde_json::Value;
use sha2::{Digest, Sha256};
use silyn_core::SilynError;
use silyn_core::contracts::ToolCall;

/// Default number of recent fingerprints kept in the window.
pub const DEFAULT_LOOP_WINDOW: usize = 8;
/// Default number of repeats that counts as a loop.
pub const DEFAULT_LOOP_THRESHOLD: usize = 3;

/// Sliding-window repetition detector.
#[derive(Debug, Clone)]
pub struct LoopDetector {
    window: VecDeque<String>,
    max_window: usize,
    threshold: usize,
}

impl Default for LoopDetector {
    fn default() -> Self {
        Self::new(DEFAULT_LOOP_WINDOW, DEFAULT_LOOP_THRESHOLD)
    }
}

impl LoopDetector {
    /// Creates a detector. Values below one are clamped to one.
    #[must_use]
    pub fn new(window: usize, threshold: usize) -> Self {
        Self {
            window: VecDeque::new(),
            max_window: window.max(1),
            threshold: threshold.max(1),
        }
    }

    /// Observes one fingerprint and returns true when it repeats at least
    /// `threshold` times inside the window.
    pub fn observe(&mut self, fingerprint: &str) -> bool {
        self.window.push_back(fingerprint.to_owned());
        while self.window.len() > self.max_window {
            self.window.pop_front();
        }
        let repeats = self
            .window
            .iter()
            .filter(|candidate| candidate.as_str() == fingerprint)
            .count();
        repeats >= self.threshold
    }

    /// Clears the window; called when the model produces a final answer.
    pub fn reset(&mut self) {
        self.window.clear();
    }

    /// Returns the current window in observation order.
    #[must_use]
    pub fn fingerprints(&self) -> Vec<String> {
        self.window.iter().cloned().collect()
    }
}

/// SHA-256 over canonical JSON `{"tool": ..., "arguments": ...}`.
///
/// Object keys are sorted recursively so semantically identical actions
/// fingerprint identically regardless of field order.
pub fn fingerprint_tool_call(call: &ToolCall) -> Result<String, SilynError> {
    let mut canonical = String::from("{\"tool\":");
    canonical.push_str(&serde_json::to_string(&call.tool)?);
    canonical.push_str(",\"arguments\":");
    write_canonical(&call.arguments, &mut canonical)?;
    canonical.push('}');

    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    Ok(format!("{:x}", hasher.finalize()))
}

fn write_canonical(value: &Value, out: &mut String) -> Result<(), SilynError> {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(key)?);
                out.push(':');
                if let Some(inner) = map.get(*key) {
                    write_canonical(inner, out)?;
                }
            }
            out.push('}');
            Ok(())
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(item, out)?;
            }
            out.push(']');
            Ok(())
        }
        other => {
            out.push_str(&serde_json::to_string(other)?);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn call(arguments: Value) -> ToolCall {
        ToolCall {
            call_id: "call".to_owned(),
            tool: "fs.read".to_owned(),
            arguments,
        }
    }

    #[test]
    fn fingerprint_is_independent_of_key_order() {
        let first = call(json!({ "path": "a.csv", "limit": 10 }));
        let second = call(json!({ "limit": 10, "path": "a.csv" }));
        assert_eq!(
            fingerprint_tool_call(&first).expect("fingerprint"),
            fingerprint_tool_call(&second).expect("fingerprint")
        );
        let different = call(json!({ "path": "b.csv", "limit": 10 }));
        assert_ne!(
            fingerprint_tool_call(&first).expect("fingerprint"),
            fingerprint_tool_call(&different).expect("fingerprint")
        );
    }

    #[test]
    fn third_repeat_inside_window_is_a_loop() {
        let mut detector = LoopDetector::default();
        assert!(!detector.observe("a"));
        assert!(!detector.observe("b"));
        assert!(!detector.observe("a"));
        assert!(detector.observe("a"));
    }

    #[test]
    fn old_repeats_leave_the_window() {
        let mut detector = LoopDetector::new(2, 2);
        assert!(!detector.observe("a"));
        assert!(!detector.observe("b"));
        assert!(detector.observe("b"));
        // `a` from two observations ago has left the window.
        assert!(!detector.observe("a"));
        detector.reset();
        assert!(!detector.observe("a"));
    }
}
