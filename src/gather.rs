//! Candidate generators for the lateweave pipeline.
//!
//! [`ExhaustiveGatherer`] hands every eligible document to the reranker, which
//! at small scale makes the search exact MaxSim over the corpus.
//! [`LexicalGatherer`] is Lucene BM25 over the same documents, used once the
//! corpus exceeds the gather limit or when there is no encoder. Both consume
//! only query text and honour the subset.

use std::collections::HashMap;

use lateweave::{Candidate, CandidateGenerator, CorpusManifest, Query, Requirements, Result};

use crate::analyzer::tokens;

pub const LEXICAL_K1: f32 = 1.5;
pub const LEXICAL_B: f32 = 0.75;

pub struct ExhaustiveGatherer {
    corpus: CorpusManifest,
    requires: Requirements,
}

impl ExhaustiveGatherer {
    pub fn new(corpus: CorpusManifest) -> Self {
        Self {
            corpus,
            requires: Requirements::new(),
        }
    }
}

impl CandidateGenerator for ExhaustiveGatherer {
    fn corpus(&self) -> &CorpusManifest {
        &self.corpus
    }

    fn requires(&self) -> &Requirements {
        &self.requires
    }

    fn score_semantics(&self) -> &str {
        "exhaustive"
    }

    fn gather(&self, _: &Query, limit: usize, subset: Option<&[u64]>) -> Result<Vec<Candidate>> {
        let ids: Box<dyn Iterator<Item = u64>> = match subset {
            Some(subset) => Box::new(subset.iter().copied()),
            None => Box::new(0..self.corpus.document_count()),
        };
        Ok(ids
            .take(limit)
            .enumerate()
            .map(|(rank, document_id)| Candidate {
                document_id,
                gather_score: 0.0,
                gather_rank: rank,
                provenance: "exhaustive".to_owned(),
            })
            .collect())
    }
}

/// Lucene-variant BM25: `idf = ln(1 + (N - df + 0.5) / (df + 0.5))` times
/// `tf / (tf + k1 * (1 - b + b * dl / avgdl))`, summed over query terms
/// (repeated query terms count again).
struct Bm25 {
    postings: HashMap<String, Vec<(u32, u32)>>,
    lengths: Vec<u32>,
    average_length: f32,
}

impl Bm25 {
    fn new<S: AsRef<str>>(texts: &[S]) -> Self {
        let mut postings: HashMap<String, Vec<(u32, u32)>> = HashMap::new();
        let mut lengths = Vec::with_capacity(texts.len());
        for (document, text) in texts.iter().enumerate() {
            let terms = tokens(text.as_ref());
            lengths.push(terms.len() as u32);
            let mut counts: HashMap<String, u32> = HashMap::new();
            for term in terms {
                *counts.entry(term).or_default() += 1;
            }
            for (term, count) in counts {
                postings
                    .entry(term)
                    .or_default()
                    .push((document as u32, count));
            }
        }
        let total: u64 = lengths.iter().map(|&length| u64::from(length)).sum();
        let average_length = if lengths.is_empty() {
            0.0
        } else {
            total as f32 / lengths.len() as f32
        };
        Self {
            postings,
            lengths,
            average_length,
        }
    }

    /// Positive scores, best first, ties by ascending document ID.
    fn search(&self, terms: &[String], limit: usize, subset: Option<&[u64]>) -> Vec<(u64, f32)> {
        let count = self.lengths.len();
        let eligible = subset.map(|subset| {
            let mut mask = vec![false; count];
            for &document in subset {
                mask[document as usize] = true;
            }
            mask
        });
        let mut scores = vec![0.0f32; count];
        for term in terms {
            let Some(postings) = self.postings.get(term) else {
                continue;
            };
            let frequency = postings.len() as f32;
            let idf = (1.0 + (count as f32 - frequency + 0.5) / (frequency + 0.5)).ln();
            for &(document, term_count) in postings {
                if eligible
                    .as_ref()
                    .is_some_and(|mask| !mask[document as usize])
                {
                    continue;
                }
                let tf = term_count as f32;
                let normalization = LEXICAL_K1
                    * (1.0 - LEXICAL_B
                        + LEXICAL_B * self.lengths[document as usize] as f32 / self.average_length);
                scores[document as usize] += idf * tf / (tf + normalization);
            }
        }
        let mut ranked: Vec<(u64, f32)> = scores
            .into_iter()
            .enumerate()
            .filter(|&(_, score)| score > 0.0)
            .map(|(document, score)| (document as u64, score))
            .collect();
        ranked
            .sort_unstable_by(|left, right| right.1.total_cmp(&left.1).then(left.0.cmp(&right.0)));
        ranked.truncate(limit);
        ranked
    }
}

pub struct LexicalGatherer {
    corpus: CorpusManifest,
    requires: Requirements,
    index: Bm25,
}

impl LexicalGatherer {
    pub fn new<S: AsRef<str>>(corpus: CorpusManifest, texts: &[S]) -> Result<Self> {
        if texts.len() as u64 != corpus.document_count() {
            return Err(lateweave::Error::InvalidInput(
                "lexical texts do not match the corpus manifest".to_owned(),
            ));
        }
        Ok(Self {
            corpus,
            requires: Requirements::new(),
            index: Bm25::new(texts),
        })
    }
}

impl CandidateGenerator for LexicalGatherer {
    fn corpus(&self) -> &CorpusManifest {
        &self.corpus
    }

    fn requires(&self) -> &Requirements {
        &self.requires
    }

    fn score_semantics(&self) -> &str {
        "bm25-lucene"
    }

    fn gather(
        &self,
        query: &Query,
        limit: usize,
        subset: Option<&[u64]>,
    ) -> Result<Vec<Candidate>> {
        let terms = tokens(query.text());
        Ok(self
            .index
            .search(&terms, limit, subset)
            .into_iter()
            .enumerate()
            .map(|(rank, (document_id, score))| Candidate {
                document_id,
                gather_score: score,
                gather_rank: rank,
                provenance: "bm25".to_owned(),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use lateweave::document_ids_digest;

    use super::*;

    const TEXTS: [&str; 3] = [
        "CUDA illegal address after switching attention to bf16",
        "pytest fixture scope error when using async",
        "NCCL timeout: one worker never reached the barrier",
    ];

    fn corpus() -> CorpusManifest {
        let ids = (0..TEXTS.len()).map(|index| format!("r{index}"));
        CorpusManifest::new("t", "v", TEXTS.len() as u64, document_ids_digest(ids)).unwrap()
    }

    fn ids(candidates: &[Candidate]) -> Vec<u64> {
        candidates
            .iter()
            .map(|candidate| candidate.document_id)
            .collect()
    }

    #[test]
    fn exhaustive_returns_everything_in_order() {
        let gatherer = ExhaustiveGatherer::new(corpus());
        let all = gatherer.gather(&Query::new("anything"), 10, None).unwrap();
        assert_eq!(ids(&all), [0, 1, 2]);
        assert_eq!(
            all.iter()
                .map(|candidate| candidate.gather_rank)
                .collect::<Vec<_>>(),
            [0, 1, 2]
        );
        assert_eq!(
            ids(&gatherer.gather(&Query::new("x"), 2, None).unwrap()),
            [0, 1]
        );
        assert_eq!(
            ids(&gatherer
                .gather(&Query::new("x"), 10, Some(&[0, 2]))
                .unwrap()),
            [0, 2]
        );
    }

    #[test]
    fn lexical_ranks_matches_first_and_drops_zero_scores() {
        let gatherer = LexicalGatherer::new(corpus(), &TEXTS).unwrap();
        let found = gatherer
            .gather(&Query::new("worker barrier timeout"), 10, None)
            .unwrap();
        assert_eq!(ids(&found), [2]);
        assert!(found[0].gather_score > 0.0);
        assert!(gatherer
            .gather(&Query::new(""), 10, None)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn lexical_honours_the_subset() {
        let gatherer = LexicalGatherer::new(corpus(), &TEXTS).unwrap();
        let found = gatherer
            .gather(&Query::new("cuda attention barrier"), 10, Some(&[1, 2]))
            .unwrap();
        assert_eq!(ids(&found), [2]);
        assert!(gatherer
            .gather(&Query::new("cuda"), 10, Some(&[]))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn lucene_scores_match_the_reference_formula() {
        let index = Bm25::new(&["a b", "a", "c c c d"]);
        let scores = index.search(&["a".to_owned()], 10, None);
        let average = 7.0 / 3.0;
        let idf = (1.0f32 + (3.0 - 2.0 + 0.5) / (2.0 + 0.5)).ln();
        let expected = |length: f32| idf / (1.0 + 1.5 * (0.25 + 0.75 * length / average));
        assert_eq!(scores[0].0, 1);
        assert!((scores[0].1 - expected(1.0)).abs() < 1e-6);
        assert_eq!(scores[1].0, 0);
        assert!((scores[1].1 - expected(2.0)).abs() < 1e-6);
    }

    #[test]
    fn mismatched_texts_are_rejected() {
        assert!(LexicalGatherer::new(corpus(), &TEXTS[..2]).is_err());
    }
}
