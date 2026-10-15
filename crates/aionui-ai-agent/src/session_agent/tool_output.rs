//! Bounded, replace-style live previews. Authoritative ToolResult output never
//! passes through this buffer. Limits apply to previews, not all session memory.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

pub(super) const PREVIEW_BYTES: usize = 64 * 1024;
pub(super) const PREVIEW_QUEUE_HIGH_WATER: usize = 16;
pub(super) const PREVIEW_PERIOD: Duration = Duration::from_millis(100);
const TRUNCATED: &str = "[Live preview truncated; showing latest output]\n";

struct Preview {
    tail: String,
    truncated: bool,
    dirty: bool,
}

impl Default for Preview {
    fn default() -> Self {
        Self {
            // Fixed capacity avoids briefly allocating a huge delta, or String's
            // geometric growth allocating more than the per-tool budget.
            tail: String::with_capacity(PREVIEW_BYTES),
            truncated: false,
            dirty: false,
        }
    }
}

impl Preview {
    /// Returns true only when this tool first crosses the preview limit.
    fn append(&mut self, text: &str) -> bool {
        let was_truncated = self.truncated;
        self.truncated |= text.len() > PREVIEW_BYTES - self.tail.len();
        let limit = PREVIEW_BYTES - if self.truncated { TRUNCATED.len() } else { 0 };
        if text.len() >= limit {
            self.tail.clear();
            self.tail.push_str(suffix(text, limit));
        } else {
            let keep = limit - text.len();
            if self.tail.len() > keep {
                let start = self.tail.ceil_char_boundary(self.tail.len() - keep);
                self.tail.drain(..start);
            }
            self.tail.push_str(text);
        }
        !was_truncated && self.truncated
    }

    fn snapshot(&self) -> String {
        if self.truncated {
            let mut output = String::with_capacity(TRUNCATED.len() + self.tail.len());
            output.push_str(TRUNCATED);
            output.push_str(&self.tail);
            output
        } else {
            self.tail.clone()
        }
    }
}

fn suffix(text: &str, max_bytes: usize) -> &str {
    &text[text.ceil_char_boundary(text.len().saturating_sub(max_bytes))..]
}

#[derive(Default)]
pub(super) struct ToolOutputPreviews {
    tools: HashMap<String, Preview>,
    // Each dirty call appears once. A hot tool re-enters at the back after its
    // snapshot is taken, so it cannot starve another tool's pending update.
    pending: VecDeque<String>,
}

impl ToolOutputPreviews {
    pub(super) fn append(&mut self, call_id: &str, text: &str) -> bool {
        if text.is_empty() {
            return false;
        }
        let preview = self.tools.entry(call_id.to_owned()).or_default();
        if !preview.dirty {
            self.pending.push_back(call_id.to_owned());
            preview.dirty = true;
        }
        preview.append(text)
    }

    pub(super) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    pub(super) fn take_next(&mut self) -> Option<(String, String)> {
        let call_id = self.pending.pop_front()?;
        let preview = self.tools.get_mut(&call_id).expect("pending preview exists");
        preview.dirty = false;
        Some((call_id, preview.snapshot()))
    }

    pub(super) fn remove(&mut self, call_id: &str) -> Option<String> {
        self.pending.retain(|id| id != call_id);
        self.tools.remove(call_id).map(|p| p.snapshot())
    }

    pub(super) fn discard(&mut self, call_id: &str) {
        self.pending.retain(|id| id != call_id);
        self.tools.remove(call_id);
    }

    pub(super) fn retain(&mut self, mut keep: impl FnMut(&str) -> bool) {
        self.tools.retain(|id, _| keep(id));
        self.pending.retain(|id| self.tools.contains_key(id));
    }

    pub(super) fn clear(&mut self) {
        self.tools.clear();
        self.pending.clear();
    }

    pub(super) fn drain(&mut self) -> impl Iterator<Item = (String, String)> + '_ {
        self.pending.clear();
        self.tools.drain().map(|(id, p)| (id, p.snapshot()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn huge_delta_never_grows_backing_buffer_and_keeps_utf8_tail() {
        let mut previews = ToolOutputPreviews::default();
        let text = format!("{}THE-END", "é🙂".repeat(1024 * 1024));
        assert!(previews.append("a", &text));
        assert!(previews.tools["a"].tail.capacity() <= PREVIEW_BYTES);
        let (_, output) = previews.take_next().unwrap();
        assert!(output.len() <= PREVIEW_BYTES);
        assert!(output.starts_with(TRUNCATED));
        assert!(output.ends_with("THE-END"));
        assert!(text.ends_with(output.strip_prefix(TRUNCATED).unwrap()));
        assert!(!previews.append("a", "é🙂"), "log truncation only once per tool");
    }

    #[test]
    fn exact_boundary_and_fragmented_unicode() {
        let mut previews = ToolOutputPreviews::default();
        let text = "x".repeat(PREVIEW_BYTES);
        assert!(!previews.append("a", &text));
        assert_eq!(previews.take_next().unwrap().1, text);
        assert!(previews.append("a", "🙂"));
        for _ in 0..10_000 {
            previews.append("a", "é🙂");
        }
        let output = previews.take_next().unwrap().1;
        assert!(output.starts_with(TRUNCATED));
        assert!(output.len() <= PREVIEW_BYTES);
        assert!(output.ends_with("é🙂"));
        assert_eq!(previews.tools["a"].tail.capacity(), PREVIEW_BYTES);
    }

    #[test]
    fn coalescing_is_fair_and_removal_clears_pending_entries() {
        let mut previews = ToolOutputPreviews::default();
        previews.append("a", "one");
        previews.append("b", "two");
        previews.append("a", "three");
        assert_eq!(previews.take_next(), Some(("a".into(), "onethree".into())));
        previews.append("a", "four");
        assert_eq!(previews.take_next(), Some(("b".into(), "two".into())));
        previews.append("b", "five");
        previews.retain(|id| id == "b");
        assert_eq!(previews.remove("b"), Some("twofive".into()));
        assert!(!previews.has_pending());
        assert!(previews.tools.is_empty());
    }
}
