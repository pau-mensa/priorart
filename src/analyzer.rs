//! Text to terms, identically for documents, queries, and excerpts.
//!
//! Casefold, decompose, drop combining marks, take `\w+` runs. No stemming, so
//! the chain is language-agnostic and needs no persisted state.

use std::sync::LazyLock;

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
    fn whitespace_has_no_terms() {
        assert!(tokens("   ").is_empty());
    }
}
