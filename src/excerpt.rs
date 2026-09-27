//! Query-aware excerpts chosen by lexical overlap; no model involved.

use std::collections::HashSet;
use std::ops::Range;

use crate::analyzer::token_spans;

pub const DEFAULT_WIDTH: usize = 400;

/// The `width`-character window with the most query terms wholly inside it,
/// trimmed to word boundaries and marked with ellipses where text was cut.
pub fn excerpt(text: &str, query_tokens: &[String], width: usize) -> String {
    let text = text.trim();
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= width {
        return text.to_owned();
    }
    let wanted: HashSet<&str> = query_tokens.iter().map(String::as_str).collect();
    // Terms never overlap, so both ends ascend and a window's hits are a run.
    let hits: Vec<Range<usize>> = token_spans(text)
        .into_iter()
        .filter(|(term, _)| wanted.contains(term.as_str()))
        .map(|(_, range)| range)
        .collect();
    let step = (width / 4).max(1);
    let last = chars.len() - width;
    let (mut best_start, mut best_hits) = (0, None);
    for start in (0..last + step).step_by(step) {
        let start = start.min(last);
        let first = hits.partition_point(|hit| hit.start < start);
        let past = hits.partition_point(|hit| hit.end <= start + width);
        let count = past.saturating_sub(first);
        if best_hits.is_none_or(|best| count > best) {
            (best_start, best_hits) = (start, Some(count));
        }
        if start == last {
            break;
        }
    }
    trim(&chars, best_start, width)
}

fn trim(chars: &[char], mut start: usize, width: usize) -> String {
    let slack = width / 4;
    let mut end = chars.len().min(start + width);
    if start > 0 {
        if let Some(cut) = chars[start..end].iter().position(|&ch| ch == ' ') {
            if cut < slack {
                start += cut + 1;
            }
        }
    }
    if end < chars.len() {
        if let Some(cut) = chars[start..end].iter().rposition(|&ch| ch == ' ') {
            if end - (start + cut) < slack {
                end = start + cut;
            }
        }
    }
    let piece: String = chars[start..end].iter().collect();
    format!(
        "{}{}{}",
        if start > 0 { "…" } else { "" },
        piece.trim(),
        if end < chars.len() { "…" } else { "" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terms(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| word.to_string()).collect()
    }

    #[test]
    fn short_text_is_returned_whole() {
        assert_eq!(
            excerpt("  hello world  ", &terms(&["x"]), 400),
            "hello world"
        );
    }

    #[test]
    fn window_with_a_late_match_is_selected() {
        let text = format!(
            "{}the NCCL barrier timed out on rank three{}",
            "filler word ".repeat(100),
            " tail".repeat(30)
        );
        let out = excerpt(&text, &terms(&["nccl", "barrier"]), 80);
        assert!(out.contains("NCCL barrier"));
        assert!(out.starts_with('…'));
        assert!(out.chars().count() <= 82);
    }

    #[test]
    fn no_overlap_keeps_the_prefix() {
        let out = excerpt(&"alpha ".repeat(200), &terms(&["zzz"]), 60);
        assert!(out.starts_with("alpha"));
        assert!(out.ends_with('…'));
    }
}
