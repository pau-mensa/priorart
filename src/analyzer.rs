//! Text to terms, identically for documents, queries, and excerpts.
//!
//! Casefold, decompose, drop combining marks, take `\w+` runs. No stemming, so
//! the chain is language-agnostic and needs no persisted state.

use std::ops::Range;
use std::sync::LazyLock;

use caseless::Caseless;
use regex::Regex;
use unicode_normalization::char::canonical_combining_class;
use unicode_normalization::UnicodeNormalization;

static TOKEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\w+").expect("valid token regex"));

pub fn tokens(text: &str) -> Vec<String> {
    let folded: String = caseless::default_case_fold_str(text)
        .nfkd()
        .filter(|&ch| canonical_combining_class(ch) == 0)
        .collect();
    TOKEN
        .find_iter(&folded)
        .map(|term| term.as_str().to_owned())
        .collect()
}

/// [`tokens`] with the range of `text` characters each term came from.
/// Folding and decomposition are per character, so each folded character
/// belongs to exactly one source character.
pub fn token_spans(text: &str) -> Vec<(String, Range<usize>)> {
    let mut folded = String::with_capacity(text.len());
    let mut source = Vec::with_capacity(text.len());
    for (position, ch) in text.chars().enumerate() {
        if ch.is_ascii() {
            folded.push(ch.to_ascii_lowercase());
            source.push(position);
            continue;
        }
        for folded_char in std::iter::once(ch)
            .default_case_fold()
            .nfkd()
            .filter(|&ch| canonical_combining_class(ch) == 0)
        {
            folded.push(folded_char);
            source.extend(std::iter::repeat_n(position, folded_char.len_utf8()));
        }
    }
    TOKEN
        .find_iter(&folded)
        .map(|term| {
            let range = source[term.start()]..source[term.end() - 1] + 1;
            (term.as_str().to_owned(), range)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn casefolds_and_strips_accents() {
        assert_eq!(
            tokens("CUDA_ERROR Illegal-Address café Straße"),
            ["cuda_error", "illegal", "address", "cafe", "strasse"]
        );
    }

    #[test]
    fn spans_match_tokens_and_point_at_their_source() {
        let text = "Ǆemo CUDA_ERROR, café ﬁx Straße x\u{301}y";
        let spans = token_spans(text);
        let terms: Vec<&str> = spans.iter().map(|(term, _)| term.as_str()).collect();
        assert_eq!(terms, tokens(text));
        let chars: Vec<char> = text.chars().collect();
        let sources: Vec<String> = spans
            .iter()
            .map(|(_, range)| chars[range.clone()].iter().collect())
            .collect();
        assert_eq!(
            sources,
            ["Ǆemo", "CUDA_ERROR", "café", "ﬁx", "Straße", "x\u{301}y"]
        );
    }

    #[test]
    fn whitespace_has_no_terms() {
        assert!(tokens("   ").is_empty());
    }
}
